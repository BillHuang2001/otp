// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! Integration tests for the crate's term ordering ([`cmp`], [`eq`], [`cmp_arith`]).
//!
//! These exercise the public `erts_cmp` surface only: the documented class order,
//! name-based atom ordering, numeric exactness, structural ordering of
//! tuples/lists/binaries/pids/ports/funs, and the non-term hardening. They live
//! here (rather than in `src/compare.rs`'s in-module test module) to keep the
//! production source file under the ~1000-line guideline.

use core::cmp::Ordering;

use beam_term::boxed::{make_boxed, make_header, HEADER_FLONUM};
use beam_term::fun::{
    fun_words, write_fun_external, write_fun_imported, write_fun_local, FunKind, FunPid,
};
use beam_term::heap::Heap;
use beam_term::immediate::{
    make_atom, make_internal_pid, make_internal_port, make_pid_data, make_port_data, make_small, NIL,
};
use beam_term::ops::THE_NON_VALUE;
use beam_term::tag::{is_term_word, HAMT_SUBTAG_HEAD_FLATMAP, HEAP_BITS_SUBTAG, REF_SUBTAG};
use beam_term::{cmp, cmp_arith, eq, Atom, AtomTable, Eterm};

fn atoms() -> AtomTable {
    let mut t = AtomTable::new();
    t.seed(["a", "b", "c", "z"]);
    t
}

/// Build a [`FunKind::Local`] fun with `env` as its free variables.
///
/// # Safety
///
/// `heap` must own the [`fun_words`]-sized block and stay alive (and unmoved)
/// for as long as the returned term is used.
unsafe fn local_fun(heap: &mut Heap, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    // SAFETY: the caller keeps the block alive; `write_fun_local` writes it all.
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

/// Build a [`FunKind::Imported`] fun with `env` as its free variables.
///
/// # Safety
///
/// As [`local_fun`].
unsafe fn imported_fun(
    heap: &mut Heap,
    module: Atom,
    index: u32,
    old_uniq: u32,
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
            &[0u8; 16],
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

/// Build `#{Atom => Value}` as a flatmap (see `erl_map.h`).
unsafe fn flatmap(heap: &mut Heap, key: Eterm, value: Eterm) -> Eterm {
    let keys = heap.tuple(&[key]).unwrap();
    // SAFETY: the 4 words are fully written before the map term escapes.
    let p = unsafe { heap.alloc_words(4).unwrap() };
    unsafe {
        *p = make_header(0, HAMT_SUBTAG_HEAD_FLATMAP);
        *p.add(1) = Eterm::from_raw(1); // flatmap size
        *p.add(2) = keys;
        *p.add(3) = value;
    }
    make_boxed(p)
}

/// Build an internal ordinary ref thing (`ErtsORefThing`).
unsafe fn internal_ref(heap: &mut Heap) -> Eterm {
    let p = unsafe { heap.alloc_words(3).unwrap() };
    unsafe {
        *p = make_header(2, REF_SUBTAG);
        *p.add(1) = Eterm::from_raw(0xffff_ffff_0000_0000);
        *p.add(2) = Eterm::from_raw(0x0000_0002_0000_0001);
    }
    make_boxed(p)
}

/// Build a 16-bit (2-byte) heap binary from a `u16`.
unsafe fn heap_binary(heap: &mut Heap, value: u16) -> Eterm {
    let p = unsafe { heap.alloc_words(3).unwrap() };
    unsafe {
        *p = make_header(2, HEAP_BITS_SUBTAG);
        *p.add(1) = Eterm::from_raw(16); // bit size
        *p.add(2) = Eterm::from_raw(value as usize);
    }
    make_boxed(p)
}

fn float_term(bits: f64) -> Eterm {
    let words: Box<[Eterm; 2]> = Box::new([HEADER_FLONUM, Eterm::from_raw(bits.to_bits() as usize)]);
    let leaked: &'static [Eterm; 2] = Box::leak(words);
    make_boxed(leaked.as_ptr())
}

#[test]
fn ordering_matches_the_documented_class_order() {
    let a = atoms();
    let mut heap = Heap::new(64);
    let atom = make_atom(a.lookup("a").unwrap());
    // SAFETY: the heap owns every boxed word built here.
    let (tup, map, list, bin, r, pid, port) = unsafe {
        (
            heap.tuple(&[make_small(1)]).unwrap(),
            flatmap(&mut heap, make_small(1), make_small(2)),
            heap.cons(make_small(1), NIL).unwrap(),
            heap_binary(&mut heap, 0x0102),
            internal_ref(&mut heap),
            make_internal_pid(make_pid_data(0, 7)),
            make_internal_port(make_port_data(7)),
        )
    };
    let number = make_small(42);

    // number < atom < ref < port < pid < tuple < map < nil < list < binary
    let ascending = [number, atom, r, port, pid, tup, map, NIL, list, bin];
    for i in 0..ascending.len() {
        for j in 0..ascending.len() {
            let ord = cmp(ascending[i], ascending[j], &a);
            if i < j {
                assert_eq!(ord, Ordering::Less, "expected {i} < {j}: {ord:?}");
            } else if i == j {
                assert_eq!(ord, Ordering::Equal, "expected {i} == {j}");
            } else {
                assert_eq!(ord, Ordering::Greater, "expected {i} > {j}");
            }
        }
    }
}

#[test]
fn atoms_compare_by_name_not_index() {
    let a = atoms();
    let (ia, ib, iz) = (
        make_atom(a.lookup("a").unwrap()),
        make_atom(a.lookup("b").unwrap()),
        make_atom(a.lookup("z").unwrap()),
    );
    assert_eq!(cmp(ia, ib, &a), Ordering::Less);
    assert_eq!(cmp(ib, ia, &a), Ordering::Greater);
    assert_eq!(cmp(ia, ia, &a), Ordering::Equal);
    assert_eq!(cmp(iz, ia, &a), Ordering::Greater);
    assert!(eq(ia, ia, &a));
    assert!(!eq(ia, ib, &a));
}

#[test]
fn unknown_atom_name_falls_back_to_index() {
    let empty = AtomTable::new();
    let a0 = make_atom(Atom::from_raw(0));
    let a1 = make_atom(Atom::from_raw(1));
    // No names known: deterministic index comparison, never a panic.
    assert_eq!(cmp(a0, a1, &empty), Ordering::Less);
    assert_eq!(cmp(a1, a0, &empty), Ordering::Greater);
    assert_eq!(cmp(a0, a0, &empty), Ordering::Equal);
}

#[test]
fn small_integers_compare_numerically() {
    let a = atoms();
    for (x, y) in [(-5i64, 5i64), (0, 1), (-100, -99), (1_000_000, -1_000_000)] {
        assert_eq!(cmp(make_small(x), make_small(y), &a), x.cmp(&y));
    }
}

#[test]
fn tuples_compare_elementwise_then_by_arity() {
    let a = atoms();
    let mut heap = Heap::new(64);
    let t1 = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    let t2 = heap.tuple(&[make_small(1), make_small(3)]).unwrap();
    let t3 = heap
        .tuple(&[make_small(1), make_small(2), make_small(0)])
        .unwrap();
    let t4 = heap.tuple(&[make_small(1), make_small(2)]).unwrap();
    assert_eq!(cmp(t1, t2, &a), Ordering::Less); // second element
    assert_eq!(cmp(t2, t1, &a), Ordering::Greater);
    assert_eq!(cmp(t1, t4, &a), Ordering::Equal); // structurally equal
    assert_eq!(cmp(t1, t3, &a), Ordering::Less); // shorter arity sorts first
    assert!(eq(t1, t4, &a));
}

#[test]
fn lists_compare_elementwise_then_by_tail() {
    let a = atoms();
    let mut heap = Heap::new(64);
    let nil_list = NIL;
    let one = heap.cons(make_small(1), NIL).unwrap();
    let two = heap.cons(make_small(2), NIL).unwrap();
    let two_copy = heap.cons(make_small(2), NIL).unwrap();
    let one_two = heap.cons(make_small(1), two_copy).unwrap();
    let one_copy = heap.cons(make_small(1), NIL).unwrap();
    assert_eq!(cmp(one, two, &a), Ordering::Less);
    assert_eq!(cmp(one, one_copy, &a), Ordering::Equal);
    // `[1]` is a prefix of `[1,2]` => shorter sorts first.
    assert_eq!(cmp(one, one_two, &a), Ordering::Less);
    assert_eq!(cmp(one_two, one, &a), Ordering::Greater);
    // `[]` < `[..]`
    assert_eq!(cmp(nil_list, one, &a), Ordering::Less);
}

#[test]
fn binaries_compare_bitwise_then_by_size() {
    let a = atoms();
    let mut heap = Heap::new(64);
    // SAFETY: heap owns these binaries.
    let (b1, b2, b3) = unsafe {
        (
            heap_binary(&mut heap, 0x0102),
            heap_binary(&mut heap, 0x0103),
            heap_binary(&mut heap, 0x0102),
        )
    };
    assert_eq!(cmp(b1, b2, &a), Ordering::Less);
    assert_eq!(cmp(b2, b1, &a), Ordering::Greater);
    assert_eq!(cmp(b1, b3, &a), Ordering::Equal);
}

#[test]
fn internal_pids_and_ports_compare_by_payload() {
    let a = atoms();
    let p1 = make_internal_pid(make_pid_data(0, 1));
    let p2 = make_internal_pid(make_pid_data(0, 2));
    assert_eq!(cmp(p1, p2, &a), Ordering::Less);
    let o1 = make_internal_port(make_port_data(10));
    let o2 = make_internal_port(make_port_data(20));
    assert_eq!(cmp(o1, o2, &a), Ordering::Less);
    assert_eq!(cmp(o1, o1, &a), Ordering::Equal);
}

#[test]
fn floats_use_exact_total_order_in_exact_mode() {
    let a = atoms();
    let zero = float_term(0.0);
    let neg_zero = float_term(-0.0);
    let one = float_term(1.0);
    // exact (`=:=`): -0.0 < +0.0
    assert_eq!(cmp(neg_zero, zero, &a), Ordering::Less);
    assert_eq!(cmp(one, zero, &a), Ordering::Greater);
    assert!(!eq(neg_zero, zero, &a));
    // arithmetic (`==`): -0.0 == +0.0
    assert_eq!(cmp_arith(neg_zero, zero, &a), Ordering::Equal);
    assert_eq!(
        cmp_arith(float_term(1.5), float_term(1.25), &a),
        Ordering::Greater
    );
}

#[test]
fn integer_float_exactness_matches_erts_cmp() {
    let a = atoms();
    let one = make_small(1);
    let one_f = float_term(1.0);
    // exact (`=:=`): different classes order by class rank => number < float,
    // but small vs float is *not* numeric equality.
    assert!(!eq(one, one_f, &a));
    assert_eq!(cmp(one, one_f, &a), Ordering::Less);
    // arithmetic (`==`): numeric comparison => equal.
    assert_eq!(cmp_arith(one, one_f, &a), Ordering::Equal);
}

#[test]
fn local_funs_compare_by_uniq_then_index_then_env() {
    let a = atoms();
    let mut heap = Heap::new(128);
    // SAFETY: the heap owns every block built here.
    let (l1, l2, l3, l4, l5, l1_copy) = unsafe {
        (
            local_fun(&mut heap, 1, 2, &[]),
            local_fun(&mut heap, 5, 2, &[]),
            local_fun(&mut heap, 1, 9, &[]),
            local_fun(&mut heap, 1, 2, &[make_small(0)]),
            local_fun(&mut heap, 1, 2, &[make_small(1)]),
            local_fun(&mut heap, 1, 2, &[]),
        )
    };
    assert_eq!(cmp(l1, l2, &a), Ordering::Less, "uniq decides");
    assert_eq!(cmp(l1, l3, &a), Ordering::Less, "index decides");
    assert_eq!(cmp(l1, l4, &a), Ordering::Less, "fewer free vars first");
    assert_eq!(cmp(l4, l5, &a), Ordering::Less, "environment element-wise");
    assert_eq!(cmp(l1, l1_copy, &a), Ordering::Equal);
    assert!(eq(l1, l1_copy, &a));
}

#[test]
fn external_funs_compare_by_mfa_and_sort_after_local_kinds() {
    let a = atoms();
    let mut heap = Heap::new(128);
    let m = a.lookup("a").unwrap();
    let n = a.lookup("b").unwrap();
    // SAFETY: the heap owns every block built here.
    let (e1, e2, e3, e1_copy, l, i) = unsafe {
        (
            external_fun(&mut heap, m, n, 2),
            external_fun(&mut heap, n, m, 2),
            external_fun(&mut heap, m, n, 3),
            external_fun(&mut heap, m, n, 2),
            local_fun(&mut heap, 0, 0, &[]),
            imported_fun(&mut heap, m, 0, 0, &[]),
        )
    };
    assert_eq!(cmp(e1, e2, &a), Ordering::Less, "module decides");
    assert_eq!(cmp(e1, e3, &a), Ordering::Less, "arity decides");
    assert_eq!(cmp(e1, e1_copy, &a), Ordering::Equal);
    assert_eq!(cmp(l, e1, &a), Ordering::Less, "external sorts after any local");
    assert_eq!(cmp(i, e1, &a), Ordering::Less);
    assert_eq!(cmp(e1, l, &a), Ordering::Greater);
}

#[test]
fn imported_funs_compare_by_module_index_old_uniq_then_env() {
    let a = atoms();
    let mut heap = Heap::new(128);
    let m = a.lookup("a").unwrap();
    let n = a.lookup("b").unwrap();
    // SAFETY: the heap owns every block built here.
    let (i1, i2, i3, i4, i5) = unsafe {
        (
            imported_fun(&mut heap, m, 1, 0, &[]),
            imported_fun(&mut heap, n, 1, 0, &[]),
            imported_fun(&mut heap, m, 2, 0, &[]),
            imported_fun(&mut heap, m, 1, 7, &[]),
            imported_fun(&mut heap, m, 1, 0, &[make_small(0)]),
        )
    };
    assert_eq!(cmp(i1, i2, &a), Ordering::Less, "module decides");
    assert_eq!(cmp(i1, i3, &a), Ordering::Less, "index decides");
    assert_eq!(cmp(i1, i4, &a), Ordering::Less, "old_uniq decides");
    assert_eq!(cmp(i1, i5, &a), Ordering::Less, "num_free decides");
    // The M1 local descriptor sorts before the imported one.
    // SAFETY: the heap owns the block.
    let l = unsafe { local_fun(&mut heap, 0, 0, &[]) };
    assert_eq!(cmp(l, i1, &a), Ordering::Less);
    assert_eq!(cmp(i1, l, &a), Ordering::Greater);
}

#[test]
fn fun_ordering_is_total_over_mixed_kinds() {
    // Leading with the kind is what keeps the order transitive: ordering a
    // mixed Local/Imported pair by `index` alone could cycle.
    let a = atoms();
    let mut heap = Heap::new(128);
    // SAFETY: the heap owns every block built here.
    let (l_hi, l_lo, imp) = unsafe {
        (
            local_fun(&mut heap, 9, 2, &[]),
            local_fun(&mut heap, 1, 5, &[]),
            imported_fun(&mut heap, a.lookup("a").unwrap(), 2, 0, &[]),
        )
    };
    let mut sorted = [l_hi, imp, l_lo];
    sorted.sort_by(|x, y| cmp(*x, *y, &a));
    assert_eq!(sorted, [l_lo, l_hi, imp]);
    // The relation is antisymmetric across the kind boundary too.
    assert_eq!(cmp(l_hi, imp, &a), Ordering::Less);
    assert_eq!(cmp(imp, l_hi, &a), Ordering::Greater);
}

/// A word that is not a term at all must never be dereferenced: it sorts after
/// every real term (so it never ties with one) and two non-terms order by their
/// raw word. See the module docs.
#[test]
fn a_non_term_is_never_dereferenced_and_sorts_after_every_term() {
    let a = atoms();
    let mut heap = Heap::new(64);
    let samples = [
        make_small(0),
        make_small(-1),
        make_atom(a.lookup("a").unwrap()),
        NIL,
        heap.tuple(&[make_small(1)]).unwrap(),
        heap.cons(make_small(1), NIL).unwrap(),
    ];
    // The ABI's `THE_NON_VALUE`, a bare `FLOAT` header word (what a 1-word
    // "flonum" header looks like) and an arbitrary other header word.
    for nt in [THE_NON_VALUE, HEADER_FLONUM, Eterm::from_raw(0x3c)] {
        assert!(!is_term_word(nt));
        // The identical word still compares equal (it *is* the same word)...
        assert_eq!(cmp(nt, nt, &a), Ordering::Equal);
        assert_eq!(cmp_arith(nt, nt, &a), Ordering::Equal);
        assert!(eq(nt, nt, &a));
        for t in &samples {
            // ... but a non-term never ties with, and never equals, a real term.
            assert_eq!(cmp(nt, *t, &a), Ordering::Greater, "{nt:?} vs {t:?}");
            assert_eq!(cmp(*t, nt, &a), Ordering::Less, "{t:?} vs {nt:?}");
            assert_eq!(cmp_arith(nt, *t, &a), Ordering::Greater);
            assert_eq!(cmp_arith(*t, nt, &a), Ordering::Less);
            assert!(!eq(nt, *t, &a));
            assert!(!eq(*t, nt, &a));
        }
    }
    // Two *different* non-term words order deterministically by their raw word
    // (`THE_NON_VALUE` is `0` in release and `0x18` in a debug build; both are
    // below the `0x58` bare flonum header).
    let (lo, hi) = (THE_NON_VALUE, HEADER_FLONUM);
    assert!(lo.raw() < hi.raw());
    assert_eq!(cmp(lo, hi, &a), Ordering::Less);
    assert_eq!(cmp(hi, lo, &a), Ordering::Greater);
}

/// The non-term guard must be a no-op for valid input: every well-formed term
/// still compares by the documented term order
/// (`number < atom < ... < tuple < ... < nil < list`).
#[test]
fn valid_terms_keep_their_documented_order() {
    let a = atoms();
    let mut heap = Heap::new(128);
    let list = heap.cons(make_small(1), NIL).unwrap();
    let ordered = [
        make_small(0),
        make_small(1),
        make_atom(a.lookup("a").unwrap()),
        heap.tuple(&[make_small(1)]).unwrap(),
        NIL,
        list,
    ];
    for (i, x) in ordered.iter().enumerate() {
        for (j, y) in ordered.iter().enumerate() {
            assert_eq!(cmp(*x, *y, &a), i.cmp(&j), "sample {i} vs {j}");
            assert_eq!(eq(*x, *y, &a), i == j, "sample {i} vs {j}");
        }
    }
}

#[test]
fn comparison_is_antisymmetric_over_samples() {
    let a = atoms();
    let mut heap = Heap::new(128);
    // SAFETY: the heap owns every block built here.
    let (fun_local, fun_ext) = unsafe {
        (
            local_fun(&mut heap, 3, 1, &[make_small(0)]),
            external_fun(&mut heap, a.lookup("a").unwrap(), a.lookup("b").unwrap(), 1),
        )
    };
    let samples = vec![
        make_small(0),
        make_small(7),
        make_atom(a.lookup("a").unwrap()),
        NIL,
        heap.tuple(&[make_small(1)]).unwrap(),
        heap.cons(make_small(1), NIL).unwrap(),
        fun_local,
        fun_ext,
    ];
    for x in &samples {
        for y in &samples {
            let xy = cmp(*x, *y, &a);
            let yx = cmp(*y, *x, &a);
            assert_eq!(xy, yx.reverse(), "antisymmetry violated for {x:?} {y:?}");
            assert_eq!(eq(*x, *y, &a), xy.is_eq());
        }
    }
}
