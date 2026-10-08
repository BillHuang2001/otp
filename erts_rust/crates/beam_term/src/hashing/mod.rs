//! Structural term hashing — the lowest-common-ancestor `make_hash2`.
//!
//! The legacy `make_hash2`/`make_hash2_helper` live in
//! `erts/emulator/beam/erl_term_hashing.c`. This module reproduces the
//! *properties* that matter for the rewrite — a deterministic, structural hash
//! where **equal terms hash equal** and structurally different terms (almost
//! always) hash differently — but **not** `make_hash2`'s exact bit values.
//!
//! # These hash values are internal
//!
//! The exact hash values produced here are **not part of any compatibility
//! contract**. Nothing outside the Rust runtime observes them: they are only
//! compared against other values produced by *this* function (e.g. `beam_ets`
//! bucket selection for `set`/`hash` tables and `ordered_set` map keys). The
//! only guarantee callers may rely on is the hash/equality consistency rule —
//! `beam_term::eq(a, b, atoms)` implies `term_hash(a, atoms) == term_hash(b, atoms)`.
//! A change to the mixing constants or algorithm is therefore *not* a
//! compatibility break.
//!
//! # Known Issues
//!
//! * The hash covers the structural classes the rewrite needs today
//!   (`small`/`big`/`float`, atoms by name, tuples, lists, binaries/bitstrings,
//!   `nil`, internal pids/ports/refs, funs).
//! * **Funs** hash over exactly the fields [`crate::compare`] compares — see
//!   [`hash_fun`] — so `eq` implies equal hashes for every fun kind.
//! * Classes without a faithful layout-aware representation (maps, records,
//!   external pid/port/ref, on-heap bin refs) use an **opaque object
//!   fallback**: the object's raw words are mixed in. This is deterministic for
//!   a given runtime image but, because a map node's words are pointers, the
//!   hash of such a term is not stable across processes/images.
//!   It is only ever used as an *internal* hash, never compared against the C.
//!
//! # Layout
//!
//! This structural hash (and the shared low-level primitives) live here; the two
//! *compatibility-pinned* ports — `make_hash2` and `make_hash` — live in the
//! `make_hash2` and `make_hash` submodules (see the section comment below).

mod common;
mod make_hash;
mod make_hash2;

pub use make_hash::make_hash;
pub use make_hash2::{compute_record_definition_hash, make_hash2};

use crate::atom::AtomTable;
use crate::bitstring::{bitstring_parts, float_bits};
use crate::boxed::{boxed_val, header_arity, thing_arityval};
use crate::eterm::Eterm;
use crate::fun::{fun_arity, fun_desc_words, fun_kind_of_header, fun_num_free, FunKind};
use crate::immediate::{atom_val, internal_pid_data, internal_port_data, signed_value};
use crate::list::{car, cdr, is_list};
use crate::ops::{class, TermClass};
use crate::tag::BIG_SIGN_BIT;
use crate::tuple::{tuple_element, tuple_size};

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

// Per-class discriminators, so structurally similar terms of different types
// (e.g. `{1}` vs `[1]`) hash differently.
const TAG_SMALL: u64 = 0x01;
const TAG_BIG: u64 = 0x02;
const TAG_FLOAT: u64 = 0x03;
const TAG_ATOM: u64 = 0x04;
const TAG_NIL: u64 = 0x05;
const TAG_LIST: u64 = 0x06;
const TAG_TUPLE: u64 = 0x07;
const TAG_BITS: u64 = 0x08;
const TAG_PID: u64 = 0x09;
const TAG_PORT: u64 = 0x0a;
const TAG_OPAQUE: u64 = 0x0b;
/// Marks the transition from the list cells to their (improper/`nil`) tail.
const TAG_LIST_TAIL: u64 = 0x0c;
/// Map discriminator (structural, over the canonical flat-map key order).
const TAG_MAP: u64 = 0x0d;
/// Fun discriminators — one per [`FunKind`], so the three descriptor shapes can
/// never collide.
const TAG_FUN_LOCAL: u64 = 0x0e;
const TAG_FUN_IMPORTED: u64 = 0x0f;
const TAG_FUN_EXTERNAL: u64 = 0x10;
/// Discriminator for a word that is **not a term at all** (a bare header word such
/// as the ABI's `THE_NON_VALUE`; see [`crate::tag::is_term_word`]).
const TAG_NON_VALUE: u64 = 0x11;

/// Mix a word into a running FNV-1a style hash.
#[inline]
fn mix64(h: u64, word: u64) -> u64 {
    (h ^ word).wrapping_mul(FNV_PRIME)
}

/// A deterministic structural hash of `a`.
///
/// Consistent with [`crate::eq`] / [`crate::cmp`] (equal terms hash equal). See
/// the module docs: the exact values are internal and not a compatibility
/// contract.
///
/// Total and panic-free: a word that is not a term at all (a bare header word, e.g.
/// the ABI's `THE_NON_VALUE`; see [`crate::tag::is_term_word`]) hashes from its raw
/// word — mixed with a per-class discriminator, like every other class — and is
/// never dereferenced. Since [`crate::eq`] calls two non-terms equal only when
/// their words are identical, the `eq ⇒ equal hash` rule still holds.
pub fn term_hash(a: Eterm, atoms: &AtomTable) -> u64 {
    hash_term(a, atoms, FNV_OFFSET)
}

/// Recursively hash a term into the running hash `h`.
fn hash_term(t: Eterm, atoms: &AtomTable, h: u64) -> u64 {
    // SAFETY: `class` is total — it dereferences only the header word of a
    // well-formed boxed term and answers `NonValue` (no dereference) for a word that
    // is not a term.
    match unsafe { class(t) } {
        TermClass::Small => mix64(mix64(h, TAG_SMALL), signed_value(t) as u64),
        TermClass::Big => unsafe { hash_big(t, mix64(h, TAG_BIG)) },
        // SAFETY: `t` is a boxed float.
        TermClass::Float => mix64(mix64(h, TAG_FLOAT), unsafe { float_bits(t) }),
        TermClass::Atom => hash_atom(t, atoms, mix64(h, TAG_ATOM)),
        TermClass::Nil => mix64(h, TAG_NIL),
        // SAFETY: `t` is a cons cell / tuple.
        TermClass::List => unsafe { hash_list(t, atoms, mix64(h, TAG_LIST)) },
        TermClass::Tuple => unsafe { hash_tuple(t, atoms, mix64(h, TAG_TUPLE)) },
        // SAFETY: `t` is a bitstring.
        TermClass::Bitstring => unsafe { hash_bits(t, mix64(h, TAG_BITS)) },
        TermClass::Pid => mix64(mix64(h, TAG_PID), internal_pid_data(t)),
        TermClass::Port => mix64(mix64(h, TAG_PORT), internal_port_data(t)),
        // SAFETY: `t` is a live map.
        TermClass::Map => hash_map(t, atoms, mix64(h, TAG_MAP)),
        // SAFETY: `t` is a live fun.
        TermClass::Fun => unsafe { hash_fun(t, atoms, h) },
        // A word that is not a term at all: hash its raw word (never dereference it).
        TermClass::NonValue => mix64(mix64(h, TAG_NON_VALUE), t.raw() as u64),
        // Refs plus every opaque class.
        _ => unsafe { hash_words(t, mix64(h, TAG_OPAQUE)) },
    }
}

/// Hash a fun over exactly the fields [`crate::compare::cmp`] compares, so `eq`
/// implies equal hashes for every kind (see [`crate::fun`] and
/// [`crate::compare::cmp`]):
///
/// * [`FunKind::Local`] ⇒ `(uniq, index)`, then `num_free`, then the environment.
/// * [`FunKind::Imported`] ⇒ `(module, index, old_uniq)`, then `num_free`, then
///   the environment.
/// * [`FunKind::External`] ⇒ `(module, function, arity)`.
///
/// The fields the comparison ignores are deliberately not hashed — the fun arity
/// for the two local kinds, and the imported `uniq`/`old_index`/creator pid — so
/// two funs the comparison calls `=:=` always hash equal.
///
/// # Safety
///
/// `t` must be a live fun term.
unsafe fn hash_fun(t: Eterm, atoms: &AtomTable, h: u64) -> u64 {
    let p = boxed_val(t);
    // SAFETY: `t` is a live fun, so word 0 is its (live) header word.
    let hdr = unsafe { *p };
    match fun_kind_of_header(hdr) {
        FunKind::Local => {
            let mut h = mix64(h, TAG_FUN_LOCAL);
            // SAFETY: a local fun's descriptor words 1 and 2 hold `uniq` and `index`.
            h = mix64(h, unsafe { (*p.add(1)).raw() } as u64);
            h = mix64(h, unsafe { (*p.add(2)).raw() } as u64);
            // SAFETY: `p` is the header word of the live local fun `hdr`.
            unsafe { hash_fun_env(p, hdr, atoms, h) }
        }
        FunKind::Imported => {
            let mut h = mix64(h, TAG_FUN_IMPORTED);
            // SAFETY: an imported fun's descriptor word 1 holds its module atom.
            h = hash_term(unsafe { *p.add(1) }, atoms, h);
            // SAFETY: an imported fun's descriptor words 4 and 6 hold `index` and
            // `old_uniq`.
            h = mix64(h, unsafe { (*p.add(4)).raw() } as u64);
            h = mix64(h, unsafe { (*p.add(6)).raw() } as u64);
            // SAFETY: `p` is the header word of the live imported fun `hdr`.
            unsafe { hash_fun_env(p, hdr, atoms, h) }
        }
        FunKind::External => {
            let mut h = mix64(h, TAG_FUN_EXTERNAL);
            // SAFETY: an external fun's descriptor words 1 and 2 hold its module and
            // function atoms.
            h = hash_term(unsafe { *p.add(1) }, atoms, h);
            h = hash_term(unsafe { *p.add(2) }, atoms, h);
            mix64(h, fun_arity(hdr) as u64)
        }
    }
}

/// Hash a local-kind fun's environment: `num_free`, then the `num_free`
/// elements — the same tail [`crate::compare::cmp`] walks.
///
/// # Safety
///
/// `p` must be the header word of the live local-kind fun `hdr`, whose whole
/// block (header + descriptor + `num_free` environment words) must be live.
unsafe fn hash_fun_env(p: *mut Eterm, hdr: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let num_free = fun_num_free(hdr);
    h = mix64(h, num_free as u64);
    let offset = 1 + fun_desc_words(fun_kind_of_header(hdr));
    for i in 0..num_free {
        // SAFETY: environment word `i` is within the fun's payload.
        let elem = unsafe { *p.add(offset + i) };
        h = hash_term(elem, atoms, h);
    }
    h
}

/// Hash a map structurally, over its pairs in the canonical flat-map key order
/// ([`crate::map::map_pairs_flatmap_order`]) — the same order [`crate::compare`]
/// uses, so `eq` implies equal hashes.
///
/// A malformed map (only reachable if the term was forged outside this crate)
/// falls back to the opaque raw-word hash rather than panicking.
fn hash_map(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let pairs = match crate::map::map_pairs_flatmap_order(t, atoms) {
        Ok(pairs) => pairs,
        Err(_) => return unsafe { hash_words(t, h) },
    };
    h = mix64(h, pairs.len() as u64);
    for (key, value) in pairs {
        h = hash_term(key, atoms, h);
        h = hash_term(value, atoms, h);
    }
    h
}

/// Hash an atom by its name bytes (falling back to the index when the name is
/// unknown to `atoms`).
fn hash_atom(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let index = atom_val(t);
    match atoms.name(index) {
        Some(name) => {
            for &byte in name.as_bytes() {
                h = mix64(h, byte as u64);
            }
            mix64(h, name.len() as u64)
        }
        None => mix64(h, index.raw() as u64),
    }
}

/// Hash a bignum by sign and limbs.
///
/// # Safety
///
/// `t` must be a live bignum term.
unsafe fn hash_big(t: Eterm, mut h: u64) -> u64 {
    // SAFETY: `t` is a boxed bignum.
    let p = boxed_val(t);
    let hdr = unsafe { *p };
    h = mix64(h, u64::from(hdr.raw() & BIG_SIGN_BIT != 0));
    let arity = header_arity(hdr);
    h = mix64(h, arity as u64);
    for i in 0..arity {
        // SAFETY: limb `i` is within the bignum payload.
        h = mix64(h, unsafe { (*p.add(1 + i)).raw() } as u64);
    }
    h
}

/// Hash a tuple by arity and elements.
///
/// # Safety
///
/// `t` must be a live tuple term.
unsafe fn hash_tuple(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    // SAFETY: `t` is a tuple.
    let arity = unsafe { tuple_size(t) };
    h = mix64(h, arity as u64);
    for i in 0..arity {
        // SAFETY: `i < arity`.
        let elem = unsafe { tuple_element(t, i) };
        h = hash_term(elem, atoms, h);
    }
    h
}

/// Hash a cons cell (and the rest of the list) cell-by-cell.
///
/// # Safety
///
/// `t` must be a live non-`NIL` cons cell.
unsafe fn hash_list(mut t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let mut cells = 0u64;
    loop {
        // SAFETY: `t` is a cons cell.
        let head = unsafe { car(t) };
        h = hash_term(head, atoms, h);
        cells += 1;
        // SAFETY: `t` is a cons cell.
        let tail = unsafe { cdr(t) };
        if is_list(tail) {
            t = tail;
        } else {
            h = mix64(h, TAG_LIST_TAIL);
            h = hash_term(tail, atoms, h);
            break;
        }
    }
    mix64(h, cells)
}

/// Hash a bitstring by bit size and (byte-normalised) contents.
///
/// # Safety
///
/// `t` must be a live bitstring term.
unsafe fn hash_bits(t: Eterm, mut h: u64) -> u64 {
    // SAFETY: `t` is a bitstring.
    let (base, offset, size) = unsafe { bitstring_parts(t) };
    h = mix64(h, size as u64);
    let bytes = size.div_ceil(8);
    for j in 0..bytes {
        let mut byte = 0u8;
        for k in 0..8 {
            let bit = j * 8 + k;
            if bit < size {
                // SAFETY: bit `offset + bit` is within the bitstring.
                byte |= unsafe {
                    (*base.add((offset + bit) >> 3) >> (7 - ((offset + bit) & 7))) & 1
                } << (7 - k);
            }
        }
        h = mix64(h, byte as u64);
    }
    h
}

/// Opaque object fallback: mix the object's raw header + payload words. See the
/// module `Known Issues`.
///
/// # Safety
///
/// `t` must be a live boxed term.
unsafe fn hash_words(t: Eterm, mut h: u64) -> u64 {
    // SAFETY: `t` is boxed.
    let p = boxed_val(t);
    let hdr = unsafe { *p };
    let arity = thing_arityval(hdr);
    for i in 0..=arity {
        // SAFETY: word `i` is within the object.
        h = mix64(h, unsafe { (*p.add(i)).raw() } as u64);
    }
    h
}

// ===========================================================================
// `make_hash2` (`erlang:phash2/1,2`) and `make_hash` (`erlang:phash/2`)
//
// Bit-exact u32-wrapping ports of `erts/emulator/beam/erl_term_hashing.c`:
// `make_hash2`/`make_hash2_helper` (the non-trapping path only — there is no
// process/reds machinery here) and `make_hash`. Unlike [`term_hash`] above,
// **these values are a compatibility contract**: the system ERTS produces them,
// so a Rust term must hash identically (Level-1 drop-in).
//
// Error handling: a well-formed term always hashes. The only *reachable* typed
// failure for a well-formed term is a [`FunKind::Local`] fun (a `make_fun3`
// closure), whose inlined 2-word descriptor carries neither the module nor
// `old_uniq` that the C's `ErlFunEntry` supplies — see [`crate::fun`]. That is
// reported as the dedicated [`crate::error::TermError::FunIdentityUnavailable`]
// (the term is fine; the identity is not carried), never as a substituted value.
// A word that is **not a term at all** — a bare header word, i.e. the C ABI's
// `THE_NON_VALUE` (`0`, or a bare `FLOAT` header in a debug build) and friends;
// see [`crate::tag::is_term_word`] — is reported as
// [`crate::error::TermError::NotATerm`] rather than dereferenced (the C is
// undefined behaviour there). Every other `Err` below is for a forged/unknown
// term layout (unreachable through the safe API) and stays
// [`crate::error::TermError::BadFun`], whose meaning — "this is not a well-formed
// fun" — is exactly right there.
//
// The shared low-level primitives live in the [`common`] submodule; the two
// ports are the [`make_hash2`](self::make_hash2) and
// [`make_hash`](self::make_hash) submodules, re-exported here so
// `beam_term::hashing::{make_hash2, make_hash, compute_record_definition_hash}`
// and `lib.rs`'s `pub use crate::hashing::*;` are unchanged.
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{make_boxed, make_header, HEADER_FLONUM};
    use crate::fun::{fun_words, write_fun_external, write_fun_imported, write_fun_local, FunPid};
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, NIL};
    use crate::tag::HEAP_BITS_SUBTAG;
    use crate::Atom;

    /// Build a [`FunKind::Local`] fun.
    ///
    /// # Safety
    ///
    /// `heap` must own the [`fun_words`]-sized block and outlive the term.
    unsafe fn local_fun(heap: &mut Heap, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
        // SAFETY: the caller keeps the block alive; the writer fills it.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::Local, env.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(Local, env.len())` words.
        unsafe { write_fun_local(hp, 0, uniq, index, env).unwrap() }
    }

    /// Build a [`FunKind::External`] fun.
    ///
    /// # Safety
    ///
    /// As [`local_fun`].
    unsafe fn external_fun(heap: &mut Heap, module: Atom, function: Atom, arity: usize) -> Eterm {
        // SAFETY: the caller keeps the block alive.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::External, 0)).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(External, 0)` words.
        unsafe { write_fun_external(hp, arity, module, function).unwrap() }
    }

    /// Build a [`FunKind::Imported`] fun.
    ///
    /// # Safety
    ///
    /// As [`local_fun`].
    unsafe fn imported_fun(
        heap: &mut Heap,
        module: Atom,
        index: u32,
        old_uniq: u32,
        uniq: [u8; 16],
        env: &[Eterm],
    ) -> Eterm {
        // SAFETY: the caller keeps the block alive.
        let hp = unsafe { heap.alloc_words(fun_words(FunKind::Imported, env.len())).unwrap() };
        // SAFETY: `hp` addresses exactly `fun_words(Imported, env.len())` words.
        unsafe {
            write_fun_imported(
                hp,
                0,
                module,
                &uniq,
                index,
                0,
                old_uniq,
                FunPid {
                    node: module,
                    number: 0,
                    serial: 0,
                    creation: 0,
                },
                env,
            )
            .unwrap()
        }
    }
    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "b"]);
        t
    }

    /// Build a 16-bit (2-byte) heap binary from a `u16`.
    unsafe fn heap_binary(heap: &mut Heap, value: u16) -> Eterm {
        let p = unsafe { heap.alloc_words(3).unwrap() };
        unsafe {
            *p = make_header(2, HEAP_BITS_SUBTAG);
            *p.add(1) = Eterm::from_raw(16);
            *p.add(2) = Eterm::from_raw(value as usize);
        }
        make_boxed(p)
    }

    fn float_term(bits: f64) -> Eterm {
        let words: Box<[Eterm; 2]> =
            Box::new([HEADER_FLONUM, Eterm::from_raw(bits.to_bits() as usize)]);
        let leaked: &'static [Eterm; 2] = Box::leak(words);
        make_boxed(leaked.as_ptr())
    }

    #[test]
    fn equal_terms_hash_equal_across_heaps() {
        let a = atoms();
        let mut h1 = Heap::new(32);
        let mut h2 = Heap::new(32);
        // Two independently-built but structurally equal nested tuples.
        let inner1 = h1.tuple(&[make_small(1), make_small(2)]).unwrap();
        let outer1 = h1.tuple(&[inner1, make_atom(a.lookup("a").unwrap())]).unwrap();
        let inner2 = h2.tuple(&[make_small(1), make_small(2)]).unwrap();
        let outer2 = h2.tuple(&[inner2, make_atom(a.lookup("a").unwrap())]).unwrap();
        assert_eq!(term_hash(outer1, &a), term_hash(outer2, &a));
        // Lists too.
        let tail1 = h1.cons(make_small(2), NIL).unwrap();
        let l1 = h1.cons(make_small(1), tail1).unwrap();
        let tail2 = h2.cons(make_small(2), NIL).unwrap();
        let l2 = h2.cons(make_small(1), tail2).unwrap();
        assert_eq!(term_hash(l1, &a), term_hash(l2, &a));
    }

    #[test]
    fn different_terms_hash_differently() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let tup = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let list_tail = heap.cons(make_small(2), NIL).unwrap();
        let list = heap.cons(make_small(1), list_tail).unwrap();
        let other = heap.tuple(&[make_small(1), make_small(3)]).unwrap();
        let hashes = vec![
            term_hash(make_small(1), &a),
            term_hash(make_small(2), &a),
            term_hash(NIL, &a),
            term_hash(make_atom(a.lookup("a").unwrap()), &a),
            term_hash(make_atom(a.lookup("b").unwrap()), &a),
            term_hash(tup, &a),
            term_hash(list, &a),
            term_hash(other, &a),
        ];
        let n = hashes.len();
        let mut sorted = hashes.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "expected distinct hashes: {hashes:?}");
    }

    #[test]
    fn atoms_hash_by_name_not_index() {
        let mut t1 = AtomTable::new();
        t1.seed(["x", "a"]);
        let mut t2 = AtomTable::new();
        t2.seed(["a"]);
        // "a" is index 1 in t1 but index 0 in t2; the hash must still match.
        let a1 = make_atom(t1.lookup("a").unwrap());
        let a2 = make_atom(t2.lookup("a").unwrap());
        assert_eq!(term_hash(a1, &t1), term_hash(a2, &t2));
    }

    #[test]
    fn numeric_hashes_are_class_sensitive() {
        let a = atoms();
        // 1 and 1.0 are `==` but not `=:=`; they must hash differently.
        assert_ne!(
            term_hash(make_small(1), &a),
            term_hash(float_term(1.0), &a)
        );
        // Same bits hash the same.
        assert_eq!(
            term_hash(float_term(2.5), &a),
            term_hash(float_term(2.5), &a)
        );
    }

    #[test]
    fn binaries_hash_by_content() {
        let a = atoms();
        let mut heap = Heap::new(32);
        // SAFETY: the heap owns the binaries.
        let (b1, b2, b3) = unsafe {
            (
                heap_binary(&mut heap, 0x0102),
                heap_binary(&mut heap, 0x0102),
                heap_binary(&mut heap, 0x0103),
            )
        };
        assert_eq!(term_hash(b1, &a), term_hash(b2, &a));
        assert_ne!(term_hash(b1, &a), term_hash(b3, &a));
    }

    #[test]
    fn funs_hash_by_their_compared_fields() {
        let a = atoms();
        let mut heap = Heap::new(256);
        let m = a.lookup("a").unwrap();
        let n = a.lookup("b").unwrap();
        // SAFETY: the heap owns every block built here.
        let (l1, l1_copy, l_uniq, l_index, l_env, e1, e1_copy, e_arity, i1, i1_copy, i_index, i_uniq_bytes) =
            unsafe {
                (
                    local_fun(&mut heap, 7, 3, &[]),
                    local_fun(&mut heap, 7, 3, &[]),
                    local_fun(&mut heap, 8, 3, &[]),
                    local_fun(&mut heap, 7, 4, &[]),
                    local_fun(&mut heap, 7, 3, &[make_small(1)]),
                    external_fun(&mut heap, m, n, 2),
                    external_fun(&mut heap, m, n, 2),
                    external_fun(&mut heap, m, n, 3),
                    imported_fun(&mut heap, m, 1, 0, [1u8; 16], &[]),
                    imported_fun(&mut heap, m, 1, 0, [1u8; 16], &[]),
                    imported_fun(&mut heap, m, 2, 0, [1u8; 16], &[]),
                    // Only the `uniq` bytes differ: the comparison ignores them, so
                    // the hash must too.
                    imported_fun(&mut heap, m, 1, 0, [2u8; 16], &[]),
                )
            };

        // Equal funs hash equal, for every kind.
        assert_eq!(term_hash(l1, &a), term_hash(l1_copy, &a));
        assert_eq!(term_hash(e1, &a), term_hash(e1_copy, &a));
        assert_eq!(term_hash(i1, &a), term_hash(i1_copy, &a));
        // ... including an imported fun that differs only in the ignored `uniq`.
        assert!(crate::eq(i1, i_uniq_bytes, &a));
        assert_eq!(term_hash(i1, &a), term_hash(i_uniq_bytes, &a));

        // Structurally different funs hash differently.
        let distinct = [
            term_hash(l1, &a),
            term_hash(l_uniq, &a),
            term_hash(l_index, &a),
            term_hash(l_env, &a),
            term_hash(e1, &a),
            term_hash(e_arity, &a),
            term_hash(i1, &a),
            term_hash(i_index, &a),
        ];
        let n = distinct.len();
        let mut sorted = distinct.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), n, "expected distinct hashes: {distinct:?}");
    }

    #[test]
    fn fun_hash_agrees_with_eq_across_every_pair() {
        let a = atoms();
        let mut heap = Heap::new(256);
        let m = a.lookup("a").unwrap();
        let n = a.lookup("b").unwrap();
        // SAFETY: the heap owns every block built here.
        let samples = unsafe {
            vec![
                local_fun(&mut heap, 1, 1, &[]),
                local_fun(&mut heap, 1, 1, &[]),
                local_fun(&mut heap, 2, 1, &[make_small(0)]),
                external_fun(&mut heap, m, n, 1),
                external_fun(&mut heap, m, n, 1),
                external_fun(&mut heap, n, m, 1),
                imported_fun(&mut heap, m, 1, 0, [0u8; 16], &[]),
                imported_fun(&mut heap, m, 1, 0, [0u8; 16], &[]),
                imported_fun(&mut heap, m, 1, 5, [0u8; 16], &[]),
            ]
        };
        for x in &samples {
            for y in &samples {
                if crate::eq(*x, *y, &a) {
                    assert_eq!(
                        term_hash(*x, &a),
                        term_hash(*y, &a),
                        "eq terms must hash equal: {x:?} {y:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn hash_agrees_with_eq_over_samples() {
        let a = atoms();
        let mut heap = Heap::new(64);
        let t = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        let t2 = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
        // `eq` => equal hashes.
        assert!(crate::eq(t, t2, &a));
        assert_eq!(term_hash(t, &a), term_hash(t2, &a));
    }
}
