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
//!   `nil`, internal pids/ports/refs).
//! * Classes without a faithful layout-aware representation (funs, maps,
//!   records, external pid/port/ref, on-heap bin refs) use an **opaque object
//!   fallback**: the object's raw words are mixed in. This is deterministic for
//!   a given runtime image but, because a fun's entry/map node words are
//!   pointers, the hash of such a term is not stable across processes/images.
//!   It is only ever used as an *internal* hash, never compared against the C.

use crate::atom::AtomTable;
use crate::boxed::{boxed_val, header_arity, thing_arityval};
use crate::compare::{bitstring_parts, float_bits};
use crate::eterm::Eterm;
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

/// Mix a word into a running FNV-1a style hash.
#[inline]
fn mix(h: u64, word: u64) -> u64 {
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
        TermClass::Small => mix(mix(h, TAG_SMALL), signed_value(t) as u64),
        TermClass::Big => unsafe { hash_big(t, mix(h, TAG_BIG)) },
        // SAFETY: `t` is a boxed float.
        TermClass::Float => mix(mix(h, TAG_FLOAT), unsafe { float_bits(t) }),
        TermClass::Atom => hash_atom(t, atoms, mix(h, TAG_ATOM)),
        TermClass::Nil => mix(h, TAG_NIL),
        // SAFETY: `t` is a cons cell / tuple.
        TermClass::List => unsafe { hash_list(t, atoms, mix(h, TAG_LIST)) },
        TermClass::Tuple => unsafe { hash_tuple(t, atoms, mix(h, TAG_TUPLE)) },
        // SAFETY: `t` is a bitstring.
        TermClass::Bitstring => unsafe { hash_bits(t, mix(h, TAG_BITS)) },
        TermClass::Pid => mix(mix(h, TAG_PID), internal_pid_data(t)),
        TermClass::Port => mix(mix(h, TAG_PORT), internal_port_data(t)),
        // Refs plus every opaque class.
        _ => unsafe { hash_words(t, mix(h, TAG_OPAQUE)) },
    }
}

/// Hash an atom by its name bytes (falling back to the index when the name is
/// unknown to `atoms`).
fn hash_atom(t: Eterm, atoms: &AtomTable, mut h: u64) -> u64 {
    let index = atom_val(t);
    match atoms.name(index) {
        Some(name) => {
            for &byte in name.as_bytes() {
                h = mix(h, byte as u64);
            }
            mix(h, name.len() as u64)
        }
        None => mix(h, index.raw() as u64),
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
    h = mix(h, u64::from(hdr.raw() & BIG_SIGN_BIT != 0));
    let arity = header_arity(hdr);
    h = mix(h, arity as u64);
    for i in 0..arity {
        // SAFETY: limb `i` is within the bignum payload.
        h = mix(h, unsafe { (*p.add(1 + i)).raw() } as u64);
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
    h = mix(h, arity as u64);
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
            h = mix(h, TAG_LIST_TAIL);
            h = hash_term(tail, atoms, h);
            break;
        }
    }
    mix(h, cells)
}

/// Hash a bitstring by bit size and (byte-normalised) contents.
///
/// # Safety
///
/// `t` must be a live bitstring term.
unsafe fn hash_bits(t: Eterm, mut h: u64) -> u64 {
    // SAFETY: `t` is a bitstring.
    let (base, offset, size) = unsafe { bitstring_parts(t) };
    h = mix(h, size as u64);
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
        h = mix(h, byte as u64);
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
        h = mix(h, unsafe { (*p.add(i)).raw() } as u64);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxed::{make_boxed, make_header, HEADER_FLONUM};
    use crate::heap::Heap;
    use crate::immediate::{make_atom, make_small, NIL};
    use crate::tag::HEAP_BITS_SUBTAG;

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
