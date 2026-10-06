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

// `make_hash2` / `make_hash` support (see the section comment further down).
use crate::boxed::{
    arityval, header_subtag, internal_ref_numbers, is_arity_value, is_boxed, make_boxed,
    make_header, map_header_val, ERL_BIN_REF_SIZE,
};
use crate::tuple::tuple_val;
use crate::error::{TermError, TermResult};
use crate::fun::{fun_env, fun_function, fun_index, fun_module, fun_old_uniq};
use crate::immediate::{is_byte, is_small, pid_number, unsigned_value};
use crate::list::list_val;
use crate::record::{
    record_declaration_keys, record_definition, record_definition_hash, record_definition_is_exported,
    record_definition_module, record_definition_name, record_field_count, record_values,
};
use crate::tag::{
    primary_tag, PrimaryTag, ARITYVAL_SUBTAG, BIN_REF_SUBTAG, EXTERNAL_PID_SUBTAG,
    EXTERNAL_PORT_SUBTAG, EXTERNAL_REF_SUBTAG, FLOAT_SUBTAG, FUN_SUBTAG, HAMT_SUBTAG_HEAD_ARRAY,
    HAMT_SUBTAG_HEAD_BITMAP, HAMT_SUBTAG_HEAD_FLATMAP, HAMT_SUBTAG_NODE_BITMAP,
    HEADER_MAP_SUBTAG_MASK, HEAP_BITS_SUBTAG, MAP_SUBTAG, NEG_BIG_SUBTAG, POS_BIG_SUBTAG,
    RECORD_SUBTAG, REF_SUBTAG, SUB_BITS_SUBTAG, TAG_IMMED1_IMMED2, TAG_IMMED1_MASK, TAG_IMMED1_PID,
    TAG_IMMED1_PORT, TAG_IMMED1_SMALL, TAG_IMMED2_ATOM, TAG_IMMED2_MASK, TAG_IMMED2_NIL,
};
use crate::thing::{
    big_parts, external_pid_parts, external_port_parts, external_ref_parts, flatmap_parts,
};

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
pub fn term_hash(a: Eterm, atoms: &AtomTable) -> u64 {
    hash_term(a, atoms, FNV_OFFSET)
}

/// Recursively hash a term into the running hash `h`.
fn hash_term(t: Eterm, atoms: &AtomTable, h: u64) -> u64 {
    // SAFETY: `class` only dereferences the header word of a live boxed term.
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
// failure is a [`FunKind::Local`] fun (a `make_fun3` closure), whose inlined
// 2-word descriptor carries neither the module nor `old_uniq` that the C's
// `ErlFunEntry` supplies — see [`crate::fun`]. Every other `Err` below is for a
// forged/unknown term layout (unreachable through the safe API). `TermError` has
// no dedicated "malformed term" variant and this crate may not add one from here,
// so `TermError::BadFun` is used as the documented stand-in (reported upstream).
// ===========================================================================

// `FUNNY_NUMBER1..15` (`erl_term_hashing.c:98-112`) — the primes just above 2^28
// used by `make_hash`.
const FN1: u32 = 268_440_163;
const FN2: u32 = 268_439_161;
const FN3: u32 = 268_435_459;
const FN4: u32 = 268_436_141;
const FN5: u32 = 268_438_633;
const FN6: u32 = 268_437_017;
const FN8: u32 = 268_437_511;
const FN9: u32 = 268_439_627;
const FN10: u32 = 268_440_479;
const FN11: u32 = 268_440_577;
const FN12: u32 = 268_440_581;
const FN13: u32 = 268_440_593;
const FN14: u32 = 268_440_611;
const FN15: u32 = 268_440_629;

/// `HCONST` (`erl_term_hashing.c:441`).
const HCONST: u32 = 0x9e37_79b9;
/// `HCONST_n` = `(HCONST * n) mod 2^32` (`erl_term_hashing.c:867-888`). Only the
/// ones the two algorithms actually use are defined.
const HCONST_2: u32 = 0x3c6e_f372;
const HCONST_3: u32 = 0xdaa6_6d2b;
const HCONST_4: u32 = 0x78dd_e6e4;
const HCONST_5: u32 = 0x1715_609d;
const HCONST_6: u32 = 0xb54c_da56;
const HCONST_7: u32 = 0x5384_540f;
const HCONST_9: u32 = 0x8ff3_4781;
const HCONST_10: u32 = 0x2e2a_c13a;
const HCONST_11: u32 = 0xcc62_3af3;
const HCONST_12: u32 = 0x6a99_b4ac;
const HCONST_13: u32 = 0x08d1_2e65;
const HCONST_14: u32 = 0xa708_a81e;
const HCONST_15: u32 = 0x4540_21d7;
const HCONST_16: u32 = 0xe377_9b90;
const HCONST_19: u32 = 0xbe1e_08bb;
const HCONST_23: u32 = 0x36fb_ef9f;

/// `NIL_DEF` (`erl_term.h:1443`) — the virtual tag mixed for `nil`.
const NIL_DEF: u32 = 2;
/// The `hash == 0` fast-path value for `nil` (`erl_term_hashing.c:1522`).
const NIL_HASH: u32 = 3_468_870_702;

/// `HASH_MAP_TAIL` (`erl_term_hashing.c:890`) — the map xor-protocol sentinel.
const HASH_MAP_TAIL: Eterm = make_header(1, REF_SUBTAG);
/// `HASH_MAP_PAIR` (`erl_term_hashing.c:891`).
const HASH_MAP_PAIR: Eterm = make_header(2, REF_SUBTAG);

/// `MIX(a, b, c)` (Bob Jenkins, `erl_term_hashing.c:428-439`), returning `c`.
#[inline]
fn mix32(a: u32, b: u32, c: u32) -> u32 {
    mix3(a, b, c).2
}

/// [`mix`] returning all three working variables (`block_hash_buffer` carries
/// `a`/`b`/`c` across iterations).
#[inline]
fn mix3(mut a: u32, mut b: u32, mut c: u32) -> (u32, u32, u32) {
    a = a.wrapping_sub(b).wrapping_sub(c);
    a ^= c >> 13;
    b = b.wrapping_sub(c).wrapping_sub(a);
    b ^= a << 8;
    c = c.wrapping_sub(a).wrapping_sub(b);
    c ^= b >> 13;

    a = a.wrapping_sub(b).wrapping_sub(c);
    a ^= c >> 12;
    b = b.wrapping_sub(c).wrapping_sub(a);
    b ^= a << 16;
    c = c.wrapping_sub(a).wrapping_sub(b);
    c ^= b >> 5;

    a = a.wrapping_sub(b).wrapping_sub(c);
    a ^= c >> 3;
    b = b.wrapping_sub(c).wrapping_sub(a);
    b ^= a << 10;
    c = c.wrapping_sub(a).wrapping_sub(b);
    c ^= b >> 15;
    (a, b, c)
}

/// `UINT32_HASH_2(e1, e2, AConst)` (`erl_term_hashing.c:894-900`).
#[inline]
fn uint32_hash_2(hash: u32, e1: u32, e2: u32, k: u32) -> u32 {
    mix32(k.wrapping_add(e1), k.wrapping_add(e2), hash)
}

/// `UINT32_HASH(e, AConst)`.
#[inline]
fn uint32_hash(hash: u32, e: u32, k: u32) -> u32 {
    uint32_hash_2(hash, e, 0, k)
}

/// `SINT32_HASH(y, AConst)` (`erl_term_hashing.c:904-912`) — a negative value is
/// mixed twice (once with `-y`, once with `y`).
#[inline]
fn sint32_hash(mut hash: u32, y: i32, k: u32) -> u32 {
    if y < 0 {
        hash = uint32_hash(hash, y.unsigned_abs(), k);
    }
    uint32_hash(hash, y as u32, k)
}

/// `IS_SSMALL28(x)` (`erl_term_hashing.c:914`) — `-2^27 <= x < 2^27`.
#[inline]
const fn is_ssmall28(x: i64) -> bool {
    ((x >> 27) as u64).wrapping_add(1) < 2
}

/// `NOT_SSMALL28_HASH` (`erl_term_hashing.c:916-931`) — the two-word form for a
/// 60-bit small that does not fit the 28-bit fast path.
#[inline]
fn not_ssmall28_hash(hash: u32, small: i64) -> u32 {
    let (con, t) = if small < 0 {
        (HCONST_10, small.unsigned_abs())
    } else {
        (HCONST_11, small as u64)
    };
    let x = (t & 0xffff_ffff) as u32;
    let y = (t >> 32) as u32;
    uint32_hash_2(hash, x, y, con)
}

/// `UINT32_HASH_STEP(x, Prime1)` (`erl_term_hashing.c:184-192`) — one bytewise
/// multiply-accumulate step, least-significant byte first.
#[inline]
fn uint32_hash_step(hash: u32, x: u32, prime: u32) -> u32 {
    let h = hash
        .wrapping_mul(prime)
        .wrapping_add(x & 0xff)
        .wrapping_mul(prime)
        .wrapping_add((x >> 8) & 0xff)
        .wrapping_mul(prime)
        .wrapping_add((x >> 16) & 0xff)
        .wrapping_mul(prime);
    h.wrapping_add(x >> 24)
}

/// The four little-endian bytes at `k[..4]` as a `u32`.
#[inline]
fn le32(k: &[u8]) -> u32 {
    u32::from(k[0])
        | (u32::from(k[1]) << 8)
        | (u32::from(k[2]) << 16)
        | (u32::from(k[3]) << 24)
}

/// `block_hash(block, block_length, initval)` (`erl_term_hashing.c:503-521`) —
/// the Jenkins 12-byte block hash, exactly as written (note the shift layout of
/// the final `c` contribution: its low byte is reserved for the length).
fn block_hash(bytes: &[u8], initval: u32) -> u32 {
    const BPI: usize = 12;
    let block_length = bytes.len();
    let not_in_loop = block_length % BPI;
    let in_loop = block_length - not_in_loop;

    let (mut a, mut b, mut c) = (HCONST, HCONST, initval);
    let mut k = &bytes[..in_loop];
    while k.len() >= BPI {
        let (na, nb, nc) = mix3(
            a.wrapping_add(le32(&k[0..])),
            b.wrapping_add(le32(&k[4..])),
            c.wrapping_add(le32(&k[8..])),
        );
        a = na;
        b = nb;
        c = nc;
        k = &k[BPI..];
    }

    let fb = &bytes[in_loop..];
    let len = fb.len();
    c = c.wrapping_add(block_length as u32);
    if len >= 1 {
        a = a.wrapping_add(u32::from(fb[0]));
    }
    if len >= 2 {
        a = a.wrapping_add(u32::from(fb[1]) << 8);
    }
    if len >= 3 {
        a = a.wrapping_add(u32::from(fb[2]) << 16);
    }
    if len >= 4 {
        a = a.wrapping_add(u32::from(fb[3]) << 24);
    }
    if len >= 5 {
        b = b.wrapping_add(u32::from(fb[4]));
    }
    if len >= 6 {
        b = b.wrapping_add(u32::from(fb[5]) << 8);
    }
    if len >= 7 {
        b = b.wrapping_add(u32::from(fb[6]) << 16);
    }
    if len >= 8 {
        b = b.wrapping_add(u32::from(fb[7]) << 24);
    }
    if len >= 9 {
        c = c.wrapping_add(u32::from(fb[8]) << 8);
    }
    if len >= 10 {
        c = c.wrapping_add(u32::from(fb[9]) << 16);
    }
    if len >= 11 {
        c = c.wrapping_add(u32::from(fb[10]) << 24);
    }
    mix3(a, b, c).2
}

/// The `(data base, bit offset, bit size)` of any bitstring term, resolving a
/// `BIN_REF` root to the `ErlSubBits` that follows it
/// (`erl_bits.c:2672-2712`: the root of a refc binary is the sub-bits).
///
/// # Safety
///
/// `t` must be a live bitstring (heap bits, sub bits, or a bin ref).
unsafe fn bitstring_parts_any(t: Eterm) -> (*const u8, usize, usize) {
    // SAFETY: `t` is a live bitstring per the caller's contract.
    let hdr = unsafe { *boxed_val(t) };
    if hdr == crate::boxed::HEADER_BIN_REF {
        // SAFETY: the ErlSubBits of a refc binary sits `ERL_BIN_REF_SIZE` words
        // above the BinRef this term points at.
        unsafe { bitstring_parts(make_boxed(boxed_val(t).add(ERL_BIN_REF_SIZE))) }
    } else {
        // SAFETY: as above.
        unsafe { bitstring_parts(t) }
    }
}

/// `make_hash2(term)` (`erl_term_hashing.c:1597-1601`, `erlang:phash2/1,2`).
///
/// Bit-exact for every term whose fun kind is not `make_fun3`-local; see the
/// section comment for the error contract.
pub fn make_hash2(t: Eterm, atoms: &AtomTable) -> TermResult<u32> {
    // "Optimization. Simple cases before declaration of estack."
    if primary_tag(t) == PrimaryTag::Immed1 {
        let m1 = t.raw() & TAG_IMMED1_MASK;
        if m1 == TAG_IMMED1_IMMED2 && (t.raw() & TAG_IMMED2_MASK) == TAG_IMMED2_ATOM {
            // `return atom_tab(...)->hvalue;`
            return Ok(atoms.hvalue(atom_val(t)));
        }
        if m1 == TAG_IMMED1_SMALL {
            let small = signed_value(t);
            return Ok(if !is_ssmall28(small) {
                not_ssmall28_hash(0, small)
            } else {
                sint32_hash(0, small as i32, HCONST)
            });
        }
    }
    make_hash2_from(t, 0, atoms)
}

/// The `make_hash2` body with an explicit running hash and an empty work stack —
/// how a caller hashes a *synthetic* term (e.g. the record-definition tuple)
/// without materializing it. The entry fast-paths above are deliberately not
/// re-applied: the C's `ESTACK` machinery never applies them mid-stream.
fn make_hash2_from(term_arg: Eterm, hash_arg: u32, atoms: &AtomTable) -> TermResult<u32> {
    let mut hash: u32 = hash_arg;
    let mut hash_xor_pairs: u32 = 0;
    let mut term = term_arg;
    let mut stack: Vec<Eterm> = Vec::new();

    'outer: loop {
        // `to_common` mirrors the C's `goto hash2_common`; the LIST/ARITYVAL/
        // RECORD/FUN-local arms instead fall out of the switch and re-process
        // `term` on the next iteration.
        let to_common;
        match primary_tag(term) {
            PrimaryTag::List => {
                let mut ptr = list_val(term);
                let mut c: u32 = 0;
                let mut sh: u32 = 0;
                loop {
                    // SAFETY: `ptr` is the CAR of a live cons cell.
                    let head = unsafe { *ptr };
                    if !is_byte(head) {
                        break;
                    }
                    sh = (sh << 8).wrapping_add(unsigned_value(head) as u32);
                    if c == 3 {
                        hash = uint32_hash(hash, sh, HCONST_4);
                        c = 0;
                        sh = 0;
                    } else {
                        c += 1;
                    }
                    // SAFETY: `ptr` is a live cons cell, so word 1 is its CDR.
                    term = unsafe { *ptr.add(1) };
                    if !is_list(term) {
                        break;
                    }
                    // SAFETY: `term` is a cons cell.
                    ptr = list_val(term);
                }
                if c > 0 {
                    hash = uint32_hash(hash, sh, HCONST_4);
                }
                if is_list(term) {
                    // SAFETY: `ptr` is the last processed cons cell.
                    stack.push(unsafe { *ptr.add(1) }); // CDR
                    term = unsafe { *ptr }; // CAR
                }
                to_common = false;
            }
            PrimaryTag::Boxed => {
                // SAFETY: `term` is boxed.
                let p = boxed_val(term);
                let hdr = unsafe { *p };
                match header_subtag(hdr) as usize {
                    RECORD_SUBTAG => {
                        let field_count = unsafe { record_field_count(term)? };
                        let def = unsafe { record_definition(term)? };
                        let hash_term = unsafe { record_definition_hash(def)? };
                        if !is_small(hash_term) {
                            return Err(TermError::BadRecord);
                        }
                        let def_hash_val = unsigned_value(hash_term) as u32;
                        let values = unsafe { record_values(term)? };
                        hash = uint32_hash_2(hash, field_count as u32, def_hash_val, HCONST_23);
                        if field_count == 0 {
                            to_common = true;
                        } else {
                            // `values[arity-1] .. values[1]` are pushed in that
                            // order (so they pop in forward order); `values[0]` is
                            // processed now.
                            for i in (1..field_count).rev() {
                                stack.push(values[i]);
                            }
                            term = values[0];
                            to_common = false;
                        }
                    }
                    ARITYVAL_SUBTAG => {
                        let arity = header_arity(hdr);
                        hash = uint32_hash(hash, arity as u32, HCONST_9);
                        if arity == 0 {
                            to_common = true;
                        } else {
                            // `tuple_val(term)[i]` is element `i-1`.
                            // SAFETY: `term` is a tuple of `arity` elements.
                            let base = tuple_val(term);
                            for i in (2..=arity).rev() {
                                // SAFETY: `i <= arity`.
                                stack.push(unsafe { *base.add(i) });
                            }
                            // SAFETY: element 0 lives at word 1.
                            term = unsafe { *base.add(1) };
                            to_common = false;
                        }
                    }
                    MAP_SUBTAG => {
                        let map_tag = hdr.raw() & HEADER_MAP_SUBTAG_MASK;
                        if map_tag == HAMT_SUBTAG_HEAD_FLATMAP {
                            // SAFETY: the header says flat map.
                            let (ks, vs) = unsafe { flatmap_parts(term) };
                            let size = ks.len();
                            hash = uint32_hash(hash, size as u32, HCONST_16);
                            if size == 0 {
                                to_common = true;
                            } else {
                                stack.push(Eterm::from_raw(hash_xor_pairs as usize));
                                stack.push(Eterm::from_raw(hash as usize));
                                stack.push(HASH_MAP_TAIL);
                                hash = 0;
                                hash_xor_pairs = 0;
                                for i in (0..size).rev() {
                                    stack.push(HASH_MAP_PAIR);
                                    stack.push(vs[i]);
                                    stack.push(ks[i]);
                                }
                                to_common = true;
                            }
                        } else {
                            // `ctx.ptr = boxed_val(term) + 1`.
                            let mut ptr = unsafe { p.add(1) };
                            let mut empty = false;
                            if map_tag == HAMT_SUBTAG_HEAD_ARRAY
                                || map_tag == HAMT_SUBTAG_HEAD_BITMAP
                            {
                                // HEAD_ARRAY/HEAD_BITMAP nodes carry a size word.
                                // SAFETY: node layout `[hdr][size][slots..]`.
                                let size = unsafe { *ptr }.raw() as u32;
                                // SAFETY: as above.
                                ptr = unsafe { ptr.add(1) };
                                hash = uint32_hash(hash, size, HCONST_16);
                                if size == 0 {
                                    empty = true;
                                } else {
                                    stack.push(Eterm::from_raw(hash_xor_pairs as usize));
                                    stack.push(Eterm::from_raw(hash as usize));
                                    stack.push(HASH_MAP_TAIL);
                                    hash = 0;
                                    hash_xor_pairs = 0;
                                }
                            }
                            if empty {
                                to_common = true;
                            } else {
                                let count = match map_tag {
                                    HAMT_SUBTAG_HEAD_ARRAY => 16usize,
                                    HAMT_SUBTAG_HEAD_BITMAP | HAMT_SUBTAG_NODE_BITMAP => {
                                        map_header_val(hdr).count_ones() as usize
                                    }
                                    _ => return Err(TermError::BadMap),
                                };
                                for _ in 0..count {
                                    // SAFETY: `ptr` walks the node's slots.
                                    let slot = unsafe { *ptr };
                                    if is_list(slot) {
                                        // SAFETY: `slot` is a leaf cons cell.
                                        let cons = list_val(slot);
                                        stack.push(HASH_MAP_PAIR);
                                        // SAFETY: word 1 of the cons is its CDR.
                                        stack.push(unsafe { *cons.add(1) });
                                        // SAFETY: word 0 of the cons is its CAR.
                                        stack.push(unsafe { *cons });
                                    } else if is_boxed(slot) {
                                        // SAFETY: a map node slot is a term.
                                        let shdr = unsafe { *boxed_val(slot) };
                                        if is_arity_value(shdr) {
                                            // Collision node: a tuple of cons cells.
                                            let mut n = arityval(shdr);
                                            // SAFETY: the collision tuple's elements
                                            // start at word 1.
                                            let mut cp = unsafe { boxed_val(slot).add(1) };
                                            while n > 0 {
                                                // SAFETY: `cp` is a live cons cell.
                                                let cons = list_val(unsafe { *cp });
                                                stack.push(HASH_MAP_PAIR);
                                                // SAFETY: as above.
                                                stack.push(unsafe { *cons.add(1) });
                                                // SAFETY: as above.
                                                stack.push(unsafe { *cons });
                                                // SAFETY: as above.
                                                cp = unsafe { cp.add(1) };
                                                n -= 1;
                                            }
                                        } else {
                                            // A sub-node: re-processed as a term.
                                            stack.push(slot);
                                        }
                                    } else {
                                        return Err(TermError::BadMap);
                                    }
                                    // SAFETY: as above.
                                    ptr = unsafe { ptr.add(1) };
                                }
                                to_common = true;
                            }
                        }
                    }
                    FUN_SUBTAG => {
                        match fun_kind_of_header(hdr) {
                            // The C's two "local" fun kinds use `ErlFunEntry`. Our
                            // `Imported` kind (a `NEW_FUN_EXT` fun) carries the same
                            // fields, so it computes.
                            FunKind::Imported => {
                                let num_free = fun_num_free(hdr);
                                let module = unsafe { fun_module(term)? };
                                let index = unsafe { fun_index(term)? };
                                let old_uniq = unsafe { fun_old_uniq(term)? };
                                hash = uint32_hash_2(
                                    hash,
                                    num_free as u32,
                                    atoms.hvalue(module),
                                    HCONST,
                                );
                                hash = uint32_hash_2(hash, index, old_uniq, HCONST);
                                if num_free == 0 {
                                    to_common = true;
                                } else {
                                    let env = unsafe { fun_env(term)? };
                                    for i in (1..num_free).rev() {
                                        stack.push(env[i]);
                                    }
                                    term = env[0];
                                    to_common = false;
                                }
                            }
                            FunKind::External => {
                                let arity = fun_arity(hdr);
                                let module = unsafe { fun_module(term)? };
                                let function = unsafe { fun_function(term)? };
                                hash = uint32_hash_2(
                                    hash,
                                    arity as u32,
                                    atoms.hvalue(module),
                                    HCONST,
                                );
                                hash = uint32_hash(hash, atoms.hvalue(function), HCONST_14);
                                to_common = true;
                            }
                            // `make_fun3`'s inlined descriptor is only
                            // `[uniq:u64][index:u32]` — no module, no `old_uniq`.
                            // Never substitute a wrong value.
                            FunKind::Local => return Err(TermError::BadFun),
                        }
                    }
                    BIN_REF_SUBTAG | HEAP_BITS_SUBTAG | SUB_BITS_SUBTAG => {
                        // SAFETY: the header says bitstring.
                        let (ptr, offset, size) = unsafe { bitstring_parts_any(term) };
                        let con = HCONST_13.wrapping_add(hash);
                        let bytesize = size / 8;
                        let bitoffs = offset & 7;
                        let bitsize = size & 7;
                        // The C truncates the byte count to 32 bits for
                        // compatibility with older releases.
                        let sz = bytesize & 0xffff_ffff;
                        // SAFETY: `ptr` is the bitstring's first byte.
                        let base = unsafe { ptr.add(offset / 8) };
                        if sz == 0 && bitsize == 0 {
                            hash = con;
                        } else if bitoffs == 0 {
                            // SAFETY: `sz <= bytesize` bytes are readable.
                            let bytes = unsafe { core::slice::from_raw_parts(base, sz) };
                            hash = block_hash(bytes, con);
                            if bitsize > 0 {
                                // SAFETY: the partial byte follows the whole bytes.
                                let last = unsafe { *base.add(sz) };
                                hash = uint32_hash_2(
                                    hash,
                                    bitsize as u32,
                                    u32::from(last >> (8 - bitsize)),
                                    HCONST_15,
                                );
                            }
                        } else {
                            // Non-byte-aligned: copy the bits into a flat buffer.
                            let nbits = sz * 8 + bitsize;
                            let mut buf = vec![0u8; sz + 1];
                            for i in 0..nbits {
                                let b = (base_bits(base, bitoffs, i) != 0) as u8;
                                buf[i >> 3] |= b << (7 - (i & 7));
                            }
                            hash = block_hash(&buf[..sz], con);
                            if bitsize > 0 {
                                hash = uint32_hash_2(
                                    hash,
                                    bitsize as u32,
                                    u32::from(buf[sz] >> (8 - bitsize)),
                                    HCONST_15,
                                );
                            }
                        }
                        to_common = true;
                    }
                    POS_BIG_SUBTAG | NEG_BIG_SUBTAG => {
                        // SAFETY: the header says bignum.
                        let (neg, digits) = unsafe { big_parts(term) };
                        let con = if neg { HCONST_10 } else { HCONST_11 };
                        for &d in digits {
                            let x = (d & 0xffff_ffff) as u32;
                            let y = (d >> 32) as u32;
                            hash = uint32_hash_2(hash, x, y, con);
                        }
                        to_common = true;
                    }
                    REF_SUBTAG => {
                        // SAFETY: the header says internal reference.
                        let nums = unsafe { internal_ref_numbers(term) };
                        hash = uint32_hash(hash, nums[0], HCONST_7);
                        to_common = true;
                    }
                    EXTERNAL_REF_SUBTAG => {
                        // SAFETY: the header says external reference.
                        let (_, ids, _) = unsafe { external_ref_parts(term) };
                        let first = *ids.first().ok_or(TermError::BadFun)?;
                        hash = uint32_hash(hash, first, HCONST_7);
                        to_common = true;
                    }
                    EXTERNAL_PID_SUBTAG => {
                        // SAFETY: the header says external pid.
                        let (_, number, _, _) = unsafe { external_pid_parts(term) };
                        hash = uint32_hash(hash, number, HCONST_5);
                        to_common = true;
                    }
                    EXTERNAL_PORT_SUBTAG => {
                        // SAFETY: the header says external port.
                        let (_, number, _) = unsafe { external_port_parts(term) };
                        let low = (number & 0xffff_ffff) as u32;
                        let high = (number >> 32) as u32;
                        hash = uint32_hash_2(hash, low, high, HCONST_6);
                        to_common = true;
                    }
                    FLOAT_SUBTAG => {
                        // SAFETY: the header says float.
                        let mut bits = unsafe { float_bits(term) };
                        if f64::from_bits(bits) == 0.0 {
                            // Ensure positive 0.0 (the C replaces `-0.0`).
                            bits = 0f64.to_bits();
                        }
                        // Little-endian: `UINT32_HASH_2(ff.fw[1], ff.fw[0], ..)`.
                        let low = (bits & 0xffff_ffff) as u32;
                        let high = (bits >> 32) as u32;
                        hash = uint32_hash_2(hash, high, low, HCONST_12);
                        to_common = true;
                    }
                    _ => return Err(TermError::BadFun),
                }
            }
            PrimaryTag::Immed1 => {
                let m1 = term.raw() & TAG_IMMED1_MASK;
                if m1 == TAG_IMMED1_PID {
                    hash = uint32_hash(hash, pid_number(term) as u32, HCONST_5);
                } else if m1 == TAG_IMMED1_PORT {
                    let number = internal_port_data(term);
                    let low = (number & 0xffff_ffff) as u32;
                    let high = (number >> 32) as u32;
                    hash = uint32_hash_2(hash, low, high, HCONST_6);
                } else if m1 == TAG_IMMED1_IMMED2 {
                    match term.raw() & TAG_IMMED2_MASK {
                        a if a == TAG_IMMED2_ATOM => {
                            let hv = atoms.hvalue(atom_val(term));
                            hash = if hash == 0 {
                                hv
                            } else {
                                uint32_hash(hash, hv, HCONST_3)
                            };
                        }
                        n if n == TAG_IMMED2_NIL => {
                            hash = if hash == 0 {
                                NIL_HASH
                            } else {
                                uint32_hash(hash, NIL_DEF, HCONST_2)
                            };
                        }
                        _ => return Err(TermError::BadFun),
                    }
                } else if m1 == TAG_IMMED1_SMALL {
                    let small = signed_value(term);
                    hash = if !is_ssmall28(small) {
                        not_ssmall28_hash(hash, small)
                    } else {
                        sint32_hash(hash, small as i32, HCONST)
                    };
                } else {
                    return Err(TermError::BadFun);
                }
                to_common = true;
            }
            PrimaryTag::Header => return Err(TermError::BadFun),
        }

        if !to_common {
            continue 'outer;
        }

        // `hash2_common:` — pop the saved continuation.
        loop {
            match stack.pop() {
                None => return Ok(hash),
                Some(v) if v == HASH_MAP_TAIL => {
                    hash = pop_term(&mut stack)?.raw() as u32;
                    hash = uint32_hash(hash, hash_xor_pairs, HCONST_19);
                    hash_xor_pairs = pop_term(&mut stack)?.raw() as u32;
                }
                Some(v) if v == HASH_MAP_PAIR => {
                    hash_xor_pairs ^= hash;
                    hash = 0;
                }
                Some(v) => {
                    term = v;
                    break;
                }
            }
        }
    }
}

/// The bit `i` of a bit string whose first bit is the MSB of `base[bitoffs/8]`
/// (`erts_copy_bits_fwd`, ground truth `erl_bits.c`).
#[inline]
fn base_bits(base: *const u8, bitoffs: usize, i: usize) -> u8 {
    let bit = bitoffs + i;
    // SAFETY: the caller only asks for bits inside the string.
    (unsafe { *base.add(bit >> 3) } >> (7 - (bit & 7))) & 1
}

/// Pop a term off the `make_hash2` work stack.
fn pop_term(stack: &mut Vec<Eterm>) -> TermResult<Eterm> {
    stack.pop().ok_or(TermError::BadFun)
}

/// `hash_binary_bytes(bin, hash)` (`erl_term_hashing.c:114-163`) — `make_hash`'s
/// byte-wise binary hash.
fn hash_binary_bytes(t: Eterm, mut hash: u32) -> TermResult<u32> {
    // SAFETY: `t` is a live bitstring (the caller checked the tag).
    let (ptr, offset, size) = unsafe { bitstring_parts_any(t) };
    let bytesize = size / 8;
    let bitoffs = offset & 7;
    let bitsize = size & 7;
    // SAFETY: the first bit lives in `ptr[offset/8]`.
    let p = unsafe { ptr.add(offset / 8) };
    if bitoffs == 0 {
        for i in 0..bytesize {
            // SAFETY: `i < bytesize` whole bytes are readable.
            hash = hash.wrapping_mul(FN1).wrapping_add(u32::from(unsafe { *p.add(i) }));
        }
        if bitsize > 0 {
            // SAFETY: the partial byte follows the whole bytes.
            let b = u32::from(unsafe { *p.add(bytesize) } >> (8 - bitsize));
            hash = hash
                .wrapping_mul(FN1)
                .wrapping_add(b)
                .wrapping_mul(FN12)
                .wrapping_add(bitsize as u32);
        }
    } else {
        // `Uint previous = *ptr++;`
        let mut previous = u32::from(unsafe { *p });
        // SAFETY: `q` starts at the byte after the first.
        let q = unsafe { p.add(1) };
        let lshift = bitoffs;
        let rshift = 8 - lshift;
        for i in 0..bytesize {
            let mut b = (previous << lshift) & 0xff;
            // SAFETY: `i < bytesize`.
            previous = u32::from(unsafe { *q.add(i) });
            b |= previous >> rshift;
            hash = hash.wrapping_mul(FN1).wrapping_add(b);
        }
        if bitsize > 0 {
            let mut b = (previous << lshift) & 0xff;
            // SAFETY: the partial byte follows the whole bytes.
            previous = u32::from(unsafe { *q.add(bytesize) });
            b |= previous >> rshift;
            b >>= 8 - bitsize;
            hash = hash
                .wrapping_mul(FN1)
                .wrapping_add(b)
                .wrapping_mul(FN12)
                .wrapping_add(bitsize as u32);
        }
    }
    Ok(hash.wrapping_mul(FN4).wrapping_add(bytesize as u32))
}

/// The virtual tag `tag_val_def` returns (`erl_term.h:1441-1458`), plus the
/// `MAKE_HASH_*_OP` pseudo-ops `make_hash` pushes on its work stack.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HOp {
    Bitstring,
    List,
    Nil,
    Map,
    RecordInit,
    TupleInit,
    Pid,
    ExternalPid,
    Port,
    ExternalPort,
    Fun,
    Ref,
    ExternalRef,
    Atom,
    Float,
    Big,
    Small,
    /// `MAKE_HASH_TUPLE_OP`.
    Tuple,
    /// `MAKE_HASH_RECORD_OP`.
    Record,
    /// `MAKE_HASH_TERM_ARRAY_OP`.
    TermArray,
    /// `MAKE_HASH_CDR_PRE_OP`.
    CdrPre,
    /// `MAKE_HASH_CDR_POST_OP`.
    CdrPost,
}

/// An entry on `make_hash`'s work stack (the C pushes untyped words).
#[derive(Clone, Copy)]
enum HItem {
    Op(HOp),
    Term(Eterm),
    Ptr(*const Eterm),
    Count(usize),
}

/// `tag_val_def(term)` restricted to the classes `make_hash` handles.
fn hash_tag_def(t: Eterm) -> TermResult<HOp> {
    match primary_tag(t) {
        PrimaryTag::List => Ok(HOp::List),
        PrimaryTag::Boxed => {
            // SAFETY: `t` is boxed.
            let hdr = unsafe { *boxed_val(t) };
            Ok(match header_subtag(hdr) as usize {
                ARITYVAL_SUBTAG => HOp::TupleInit,
                RECORD_SUBTAG => HOp::RecordInit,
                POS_BIG_SUBTAG | NEG_BIG_SUBTAG => HOp::Big,
                REF_SUBTAG => HOp::Ref,
                FLOAT_SUBTAG => HOp::Float,
                FUN_SUBTAG => HOp::Fun,
                EXTERNAL_PID_SUBTAG => HOp::ExternalPid,
                EXTERNAL_PORT_SUBTAG => HOp::ExternalPort,
                EXTERNAL_REF_SUBTAG => HOp::ExternalRef,
                MAP_SUBTAG => HOp::Map,
                HEAP_BITS_SUBTAG | SUB_BITS_SUBTAG => HOp::Bitstring,
                // `BIN_REF_DEF` is "not a real term" and has no `make_hash` case.
                _ => return Err(TermError::BadFun),
            })
        }
        PrimaryTag::Immed1 => match t.raw() & TAG_IMMED1_MASK {
            m if m == TAG_IMMED1_PID => Ok(HOp::Pid),
            m if m == TAG_IMMED1_PORT => Ok(HOp::Port),
            m if m == TAG_IMMED1_IMMED2 => match t.raw() & TAG_IMMED2_MASK {
                a if a == TAG_IMMED2_ATOM => Ok(HOp::Atom),
                n if n == TAG_IMMED2_NIL => Ok(HOp::Nil),
                _ => Err(TermError::BadFun),
            },
            m if m == TAG_IMMED1_SMALL => Ok(HOp::Small),
            _ => Err(TermError::BadFun),
        },
        PrimaryTag::Header => Err(TermError::BadFun),
    }
}

/// `make_hash(term)` (`erl_term_hashing.c:165-425`, `erlang:phash/2`).
///
/// `erlang:phash(T, R)` is `1 + (make_hash(T) % R)` for `R < 2^32`, and the raw
/// `make_hash(T) + 1` for `R = 2^32` (`bif.c:5370-5420`).
pub fn make_hash(t: Eterm, atoms: &AtomTable) -> TermResult<u32> {
    let mut hash: u32 = 0;
    let mut stack: Vec<HItem> = Vec::new();
    let mut term = t;
    let mut op = hash_tag_def(term)?;

    loop {
        match op {
            HOp::Nil => hash = hash.wrapping_mul(FN3).wrapping_add(1),
            HOp::Atom => {
                hash = hash
                    .wrapping_mul(FN1)
                    .wrapping_add(atoms.hvalue(atom_val(term)))
            }
            HOp::Small => {
                let y1 = signed_value(term);
                let y2 = y1.unsigned_abs();
                hash = uint32_hash_step(hash, (y2 & 0xffff_ffff) as u32, FN2);
                if (y2 >> 32) != 0 {
                    hash = uint32_hash_step(hash, (y2 >> 32) as u32, FN2);
                }
                hash = hash.wrapping_mul(if y1 < 0 { FN4 } else { FN3 });
            }
            HOp::Bitstring => hash = hash_binary_bytes(term, hash)?,
            HOp::Fun => {
                // SAFETY: `term` is a fun.
                let hdr = unsafe { *boxed_val(term) };
                match fun_kind_of_header(hdr) {
                    FunKind::Imported => {
                        let num_free = fun_num_free(hdr);
                        hash = hash.wrapping_mul(FN10).wrapping_add(num_free as u32);
                        hash = hash
                            .wrapping_mul(FN1)
                            .wrapping_add(atoms.hvalue(unsafe { fun_module(term)? }));
                        hash = hash
                            .wrapping_mul(FN2)
                            .wrapping_add(unsafe { fun_index(term)? });
                        hash = hash
                            .wrapping_mul(FN2)
                            .wrapping_add(unsafe { fun_old_uniq(term)? });
                        if num_free > 0 {
                            let env = unsafe { fun_env(term)? };
                            if num_free > 1 {
                                stack.push(HItem::Ptr(env.as_ptr().wrapping_add(1)));
                                stack.push(HItem::Count(num_free - 1));
                                stack.push(HItem::Op(HOp::TermArray));
                            }
                            term = env[0];
                            op = hash_tag_def(term)?;
                            continue;
                        }
                    }
                    FunKind::External => {
                        let arity = fun_arity(hdr);
                        hash = hash.wrapping_mul(FN11).wrapping_add(arity as u32);
                        hash = hash
                            .wrapping_mul(FN1)
                            .wrapping_add(atoms.hvalue(unsafe { fun_module(term)? }));
                        hash = hash
                            .wrapping_mul(FN1)
                            .wrapping_add(atoms.hvalue(unsafe { fun_function(term)? }));
                    }
                    // As in `make_hash2`: a `make_fun3` fun lacks module/`old_uniq`.
                    FunKind::Local => return Err(TermError::BadFun),
                }
            }
            HOp::Pid | HOp::ExternalPid => {
                let number = if op == HOp::Pid {
                    pid_number(term) as u32
                } else {
                    // SAFETY: the tag says external pid.
                    unsafe { external_pid_parts(term) }.1
                };
                hash = uint32_hash_step(hash, number, FN5);
                hash = hash.wrapping_mul(FN6);
            }
            HOp::Port | HOp::ExternalPort => {
                let number = if op == HOp::Port {
                    internal_port_data(term)
                } else {
                    // SAFETY: the tag says external port.
                    unsafe { external_port_parts(term) }.1
                };
                let low = (number & 0xffff_ffff) as u32;
                let high = (number >> 32) as u32;
                if high != 0 {
                    hash = uint32_hash_step(hash, high, FN11);
                }
                hash = uint32_hash_step(hash, low, FN9);
                hash = hash.wrapping_mul(FN10);
            }
            HOp::Ref | HOp::ExternalRef => {
                let first = if op == HOp::Ref {
                    // SAFETY: the tag says internal reference.
                    unsafe { internal_ref_numbers(term)[0] }
                } else {
                    // SAFETY: the tag says external reference.
                    match unsafe { external_ref_parts(term) }.1.first() {
                        Some(v) => *v,
                        None => return Err(TermError::BadFun),
                    }
                };
                hash = uint32_hash_step(hash, first, FN9);
                hash = hash.wrapping_mul(FN10);
            }
            HOp::Float => {
                // SAFETY: `term` is a float.
                let mut bits = unsafe { float_bits(term) };
                if f64::from_bits(bits) == 0.0 {
                    bits = 0f64.to_bits();
                }
                hash = hash
                    .wrapping_mul(FN6)
                    .wrapping_add(((bits & 0xffff_ffff) as u32) ^ ((bits >> 32) as u32));
            }
            HOp::Map => {
                hash = hash
                    .wrapping_mul(FN13)
                    .wrapping_add(FN14)
                    .wrapping_add(make_hash2(term, atoms)?);
            }
            HOp::TupleInit | HOp::RecordInit => {
                // SAFETY: `term` is a tuple / record instance.
                let base = boxed_val(term);
                // SAFETY: word 0 is the (live) header.
                let arity = arityval(unsafe { *base });
                stack.push(HItem::Count(arity));
                // SAFETY: an arityval block has `arity` elements at words 1..=arity.
                stack.push(HItem::Ptr(unsafe { base.add(1) }));
                stack.push(HItem::Count(arity));
                op = if op == HOp::TupleInit {
                    HOp::Tuple
                } else {
                    HOp::Record
                };
                continue;
            }
            HOp::Tuple | HOp::Record | HOp::TermArray => {
                let i = pop_count(&mut stack)?;
                let ptr = pop_ptr(&mut stack)?;
                if i != 0 {
                    // SAFETY: `ptr` is inside a live term array with `i` elements.
                    term = unsafe { *ptr };
                    // SAFETY: one element fewer from the next word.
                    stack.push(HItem::Ptr(unsafe { ptr.add(1) }));
                    stack.push(HItem::Count(i - 1));
                    stack.push(HItem::Op(op));
                    op = hash_tag_def(term)?;
                    continue;
                }
                if op == HOp::Tuple {
                    let arity = pop_count(&mut stack)?;
                    hash = hash.wrapping_mul(FN9).wrapping_add(arity as u32);
                } else if op == HOp::Record {
                    let arity = pop_count(&mut stack)?;
                    hash = hash.wrapping_mul(FN15).wrapping_add(arity as u32);
                }
            }
            HOp::CdrPost => hash = hash.wrapping_mul(FN8),
            HOp::CdrPre | HOp::List => {
                if op == HOp::CdrPre {
                    let t2 = pop_term_item(&mut stack)?;
                    if !is_list(t2) {
                        stack.push(HItem::Op(HOp::CdrPost));
                        term = t2;
                        op = hash_tag_def(term)?;
                        continue;
                    }
                    term = t2;
                }
                // SAFETY: `term` is a cons cell.
                let mut list = list_val(term);
                let mut recur_tail = None;
                loop {
                    // SAFETY: `list` is a live cons cell.
                    let head = unsafe { *list };
                    if !is_byte(head) {
                        break;
                    }
                    // "Note that this hash is different from a 'small' hash."
                    hash = hash
                        .wrapping_mul(FN2)
                        .wrapping_add(unsigned_value(head) as u32);
                    // SAFETY: word 1 is the CDR.
                    let tl = unsafe { *list.add(1) };
                    if !is_list(tl) {
                        recur_tail = Some(tl);
                        break;
                    }
                    // SAFETY: `tl` is a cons cell.
                    list = list_val(tl);
                }
                if let Some(tl) = recur_tail {
                    stack.push(HItem::Op(HOp::CdrPost));
                    term = tl;
                } else {
                    // SAFETY: word 1 is the CDR (the non-byte tail).
                    stack.push(HItem::Term(unsafe { *list.add(1) }));
                    stack.push(HItem::Op(HOp::CdrPre));
                    // SAFETY: word 0 is the CAR.
                    term = unsafe { *list };
                }
                op = hash_tag_def(term)?;
                continue;
            }
            HOp::Big => {
                // SAFETY: the tag says bignum.
                let (neg, digits) = unsafe { big_parts(term) };
                let n = digits.len();
                for &limb in &digits[..n - 1] {
                    let mut d = limb;
                    for _ in 0..8 {
                        hash = hash.wrapping_mul(FN2).wrapping_add((d & 0xff) as u32);
                        d >>= 8;
                    }
                }
                let mut d = digits[n - 1];
                let klimbs = if (d >> 32) == 0 { 4 } else { 8 };
                for _ in 0..klimbs {
                    hash = hash.wrapping_mul(FN2).wrapping_add((d & 0xff) as u32);
                    d >>= 8;
                }
                hash = hash.wrapping_mul(if neg { FN4 } else { FN3 });
            }
        }

        // `if (WSTACK_ISEMPTY(stack)) break; op = WSTACK_POP(stack);`
        if stack.is_empty() {
            return Ok(hash);
        }
        op = pop_op(&mut stack)?;
    }
}

/// Pop an operation continuation.
fn pop_op(stack: &mut Vec<HItem>) -> TermResult<HOp> {
    match stack.pop() {
        Some(HItem::Op(op)) => Ok(op),
        _ => Err(TermError::BadFun),
    }
}

/// Pop an element count.
fn pop_count(stack: &mut Vec<HItem>) -> TermResult<usize> {
    match stack.pop() {
        Some(HItem::Count(n)) => Ok(n),
        _ => Err(TermError::BadFun),
    }
}

/// Pop an element pointer.
fn pop_ptr(stack: &mut Vec<HItem>) -> TermResult<*const Eterm> {
    match stack.pop() {
        Some(HItem::Ptr(p)) => Ok(p),
        _ => Err(TermError::BadFun),
    }
}

/// Pop a term.
fn pop_term_item(stack: &mut Vec<HItem>) -> TermResult<Eterm> {
    match stack.pop() {
        Some(HItem::Term(t)) => Ok(t),
        _ => Err(TermError::BadFun),
    }
}

/// The record-definition hash `records:create/4` stores in a definition
/// (`erl_record.c:1041-1046`): `make_hash2` of the tuple
/// `{module, name, is_exported, keys-in-declaration-order}`.
///
/// The tuple is **not materialized** — this crate cannot allocate — so the
/// equivalent running hash is computed directly (arity mix, then one
/// [`make_hash2_from`] step per element), which is exactly what the C's
/// `ESTACK` walk does.
///
/// (Named `compute_record_definition_hash` rather than the obvious
/// `record_definition_hash` because [`crate::record::record_definition_hash`]
/// already owns that name and `lib.rs` glob-re-exports both modules — the
/// collision is a `ambiguous_glob_reexports` warning we may not fix from here.)
pub fn compute_record_definition_hash(def: Eterm, atoms: &AtomTable) -> TermResult<u32> {
    let module = unsafe { record_definition_module(def)? };
    let name = unsafe { record_definition_name(def)? };
    let is_exported = unsafe { record_definition_is_exported(def)? };
    let keys = unsafe { record_declaration_keys(def)? };
    let arity = 3 + keys.len();
    let mut hash = uint32_hash(0, arity as u32, HCONST_9);
    hash = make_hash2_from(module, hash, atoms)?;
    hash = make_hash2_from(name, hash, atoms)?;
    hash = make_hash2_from(is_exported, hash, atoms)?;
    for key in keys {
        hash = make_hash2_from(key, hash, atoms)?;
    }
    Ok(hash)
}

#[cfg(test)]
mod exact_hash_tests {
    //! Vectors pinning the exact `make_hash2`/`make_hash` algorithm. The
    //! end-to-end cross-check against the real runtime lives in
    //! `tests/oracle.rs`; here we pin the values that can be derived from the C
    //! by inspection plus structural/consistency properties.

    use super::*;
    use crate::atom::Atom;
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, MAX_SMALL, MIN_SMALL, NIL};
    use crate::list::{car, cdr};
    use crate::map::{map_words_capacity, write_map};

    fn atoms() -> AtomTable {
        let mut t = AtomTable::new();
        t.seed(["a", "abc", "b"]);
        t
    }

    #[test]
    fn atom_hvalue_is_the_c_hashpjw() {
        // `atom.c:82`: h = (h << 4) + byte, with the dragon-book fold.
        // "" -> 0; "a" -> 97; "abc" -> ((97*16 + 98)*16) + 99 = 26499.
        let t = atoms();
        assert_eq!(t.hvalue(t.lookup("").unwrap_or(Atom::from_raw(999))), 999);
        let mut t2 = AtomTable::new();
        t2.seed(["", "a", "abc"]);
        assert_eq!(t2.hvalue(t2.lookup("").unwrap()), 0);
        assert_eq!(t2.hvalue(t2.lookup("a").unwrap()), 97);
        assert_eq!(t2.hvalue(t2.lookup("abc").unwrap()), 26499);
        // An index that is not in the table has no name: documented fallback.
        assert_eq!(t2.hvalue(Atom::from_raw(1234)), 1234);
    }

    #[test]
    fn make_hash2_fast_paths() {
        let t = atoms();
        // nil with a running hash of 0 is the hard-coded C constant.
        assert_eq!(make_hash2(NIL, &t).unwrap(), NIL_HASH);
        // An atom with a running hash of 0 returns `hvalue` directly.
        let abc = t.lookup("abc").unwrap();
        assert_eq!(make_hash2(make_atom(abc), &t).unwrap(), 26499);
        // Small ints: both sides of the 28-bit threshold must compute.
        for v in [
            0i64,
            1,
            -1,
            255,
            1 << 27,
            -(1 << 27),
            1 << 40,
            -(1 << 40),
            MAX_SMALL,
            MIN_SMALL,
        ] {
            assert!(make_hash2(make_small(v), &t).is_ok(), "small {v}");
        }
        // `is_ssmall28` reproduces `~((x >> 27) + 1) < 2`.
        assert!(is_ssmall28(0) && is_ssmall28(-(1 << 27)) && is_ssmall28((1 << 27) - 1));
        assert!(!is_ssmall28(1 << 27) && !is_ssmall28(-(1 << 27) - 1));
    }

    #[test]
    fn make_hash2_byte_lists_pin_the_batching() {
        // One byte-headed cons cell, tail `nil`: `UINT32_HASH(sh, HCONST_4)`
        // then the `nil` non-zero path. Values verified against the system ERTS
        // (`erlang:phash2([B], 1 bsl 32)`).
        let t = atoms();
        let expected = [
            (0i64, 3768589414u32),
            (1, 2768963718),
            (2, 3124094002),
            (3, 1984398927),
            (255, 717906935),
        ];
        for (b, want) in expected {
            let mut heap = Heap::new(8);
            let l = heap.cons(make_small(b), NIL).unwrap();
            assert_eq!(make_hash2(l, &t).unwrap(), want, "list [{b}]");
        }
    }

    #[test]
    fn make_hash2_flat_map_is_pair_order_independent() {
        let t = atoms();
        let ka = make_atom(t.lookup("a").unwrap());
        let kb = make_atom(t.lookup("b").unwrap());
        let kc = make_atom(t.lookup("abc").unwrap());
        let pairs: Vec<(Eterm, Eterm)> = vec![
            (ka, make_small(1)),
            (kb, make_small(2)),
            (kc, make_small(3)),
        ];
        let mut rev = pairs.clone();
        rev.reverse();
        let cap = map_words_capacity(pairs.len());
        let mut h1 = Heap::new(cap + 8);
        let mut h2 = Heap::new(cap + 8);
        // SAFETY: the heaps own the blocks; `cap` words are enough.
        let (m1, m2) = unsafe {
            let p1 = heap_alloc(&mut h1, cap);
            let m1 = write_map(p1, &pairs, &t).unwrap();
            let p2 = heap_alloc(&mut h2, cap);
            let m2 = write_map(p2, &rev, &t).unwrap();
            (m1, m2)
        };
        // The xor protocol makes the pair hash order-independent.
        assert_eq!(make_hash2(m1, &t).unwrap(), make_hash2(m2, &t).unwrap());
        // ... and it still sees the values.
        let mut h3 = Heap::new(cap + 8);
        // SAFETY: as above.
        let m3 = unsafe {
            let p3 = heap_alloc(&mut h3, cap);
            let alt = vec![(ka, make_small(1)), (kb, make_small(9)), (kc, make_small(3))];
            write_map(p3, &alt, &t).unwrap()
        };
        assert_ne!(make_hash2(m1, &t).unwrap(), make_hash2(m3, &t).unwrap());
    }

    /// Allocate `n` words in `heap` (test helper).
    ///
    /// # Safety
    ///
    /// `heap` must outlive the returned block.
    unsafe fn heap_alloc(heap: &mut Heap, n: usize) -> *mut Eterm {
        // SAFETY: the caller keeps the heap alive.
        unsafe { heap.alloc_words(n).unwrap() }
    }

    #[test]
    fn make_hash_pins_the_nil_small_and_atom_paths() {
        let t = atoms();
        // `nil`: `hash = 0*FUNNY_NUMBER3 + 1` = 1.
        assert_eq!(make_hash(NIL, &t).unwrap(), 1);
        // `0`: every `UINT32_HASH_STEP` keeps 0, and `0 * FUNNY_NUMBER3` is 0.
        assert_eq!(make_hash(make_small(0), &t).unwrap(), 0);
        // An atom adds only `hvalue` to `hash * FUNNY_NUMBER1`.
        let abc = t.lookup("abc").unwrap();
        assert_eq!(make_hash(make_atom(abc), &t).unwrap(), 26499);
        // A list of bytes: `... * FUNNY_NUMBER2 + byte`, then `* FUNNY_NUMBER8`
        // for the non-list tail and `* FUNNY_NUMBER3 + 1` for `nil`.
        let mut heap = Heap::new(8);
        let l = heap.cons(make_small(1), NIL).unwrap();
        let want = (1u32
            .wrapping_mul(FN3)
            .wrapping_add(1))
        .wrapping_mul(FN8);
        assert_eq!(make_hash(l, &t).unwrap(), want);
        // The list really is [1].
        // SAFETY: `l` is a live cons cell.
        unsafe {
            assert_eq!(car(l), make_small(1));
            assert_eq!(cdr(l), NIL);
        }
    }

    #[test]
    fn make_hash_and_make_hash2_disagree_for_the_same_term() {
        // Two different algorithms; only `phash2` equals `make_hash2`.
        let t = atoms();
        let mut heap = Heap::new(8);
        let l = heap.cons(make_small(200), NIL).unwrap();
        let h2 = make_hash2(l, &t).unwrap();
        let h = make_hash(l, &t).unwrap();
        assert_ne!(h2, h);
    }

    #[test]
    fn block_hash_matches_the_jk_reference_shape() {
        // `block_hash` with an empty block mixes `HCONST, HCONST, initval`.
        assert_eq!(block_hash(&[], 0), mix32(HCONST, HCONST, 0));
        // A 12-byte block runs exactly one loop iteration (no final adds).
        let b: [u8; 12] = [1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0];
        let expect = mix3(
            HCONST.wrapping_add(1),
            HCONST.wrapping_add(2),
            HCONST.wrapping_add(3),
        )
        .2;
        assert_eq!(block_hash(&b, 0), expect);
    }
}
