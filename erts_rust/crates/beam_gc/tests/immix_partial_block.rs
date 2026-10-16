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

//! Targeted **partial-block reclamation** evidence for the GenImmix mature generation.
//!
//! [`ImmixSpace`] exists to reclaim a mature generation *per object* rather than copying
//! it wholesale: a block that is only partly live must have its live objects evacuated
//! (one by one, into holes) while the dead ones are reclaimed in place, and a block that
//! is *fully* live must be pinned untouched. The in-crate tests cover the happy path; this
//! file drives the **public** API directly so the block occupancy is controlled exactly,
//! and pins the claims the option rests on:
//!
//! 1. **interleaved live/dead objects** — every live object survives with its contents and
//!    every root is relocated into the space, while `ImmixStats` reports an
//!    object-by-object reclamation (`objects_evacuated` equal to the *live* object count,
//!    `words_reclaimed > 0`, `free_lines` increased) and the eviction stops short of the
//!    block's whole line count — never a wholesale block copy;
//! 2. a **fully-live block is pinned**, not evacuated — the objects' own pointers are
//!    byte-identical before and after (`objects_pinned`/`words_pinned > 0`), and a pinned
//!    object's reference *into* an evacuated block still follows the copy;
//! 3. the space is **immediately reusable** afterwards: the lines a collection freed are
//!    handed straight back out to the mutator;
//! 4. the *other* pinning rule — "no hole large enough" — retains the object in place: a
//!    live object too large for any hole stays at its own address, its references are left
//!    verbatim, and the collection still succeeds (see the last test in this file).
//!
//! `unsafe` below only dereferences pointers this test allocated itself (through the
//! space's own `cons`/`tuple` builders) or that a collection just wrote; every block
//! carries a `// SAFETY:` rationale.

use beam_gc::prelude::{ImmixSpace, BLOCK_WORDS, LINES_PER_BLOCK, LINE_WORDS};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_list, list_val, make_small, signed_value, Eterm, NIL,
};

/// An `ImmixSpace` of at least `words` term words (rounded up to whole blocks).
fn space(words: usize) -> ImmixSpace {
    ImmixSpace::new(words).expect("immix space")
}

/// The small integers of a proper list, in order.
fn list_to_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (`is_list`), so both words are readable.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

/// The small integers of a tuple `{...}`, in order.
fn tuple_to_smalls(term: Eterm) -> Vec<i64> {
    // SAFETY: `term` is a live boxed term this test (or a collection) built.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "not a tuple header");
    let n = arityval(header);
    let mut values = Vec::with_capacity(n);
    for i in 0..n {
        // SAFETY: elements `1..=n` lie inside the `n`-element tuple just validated.
        values.push(signed_value(unsafe { *boxed_val(term).add(1 + i) }));
    }
    values
}

// ---------------------------------------------------------------------------
// 1 + 2 + 3: interleaved live/dead, a pinned full block, immediate reuse
// ---------------------------------------------------------------------------

#[test]
fn interleaved_live_and_dead_objects_are_reclaimed_per_object() {
    let mut s = space(3 * BLOCK_WORDS);
    let base = s.base();

    // ---- block 0: sixteen one-line objects, alternately live and dead -----------------
    //
    // Allocation is line-granular, so each builder call below consumes exactly one line (a
    // 2-word cons cell, or a tuple of at most two elements = 3 words, rounds up to one
    // 16-word line). The line an object lands on is therefore its position in this
    // sequence: the even positions are the live roots, the odd ones are pure garbage.
    let a = s.tuple(&[make_small(1), make_small(10)]).expect("a"); // line 0, live
    let _d1 = s.tuple(&[make_small(901)]).expect("d1"); // line 1, dead
    let b = s.tuple(&[make_small(2), make_small(20)]).expect("b"); // line 2, live
    let _d2 = s.tuple(&[make_small(902)]).expect("d2"); // line 3, dead
    let holder = s.cons(a, NIL).expect("holder"); // line 4, live
    let _d3 = s.tuple(&[make_small(903)]).expect("d3"); // line 5, dead
    let c = s.tuple(&[make_small(3), make_small(30)]).expect("c"); // line 6, live
    let _d4 = s.cons(make_small(904), NIL).expect("d4"); // line 7, dead
    let d = s.tuple(&[make_small(4), make_small(40)]).expect("d"); // line 8, live
    let _d5 = s.tuple(&[make_small(905)]).expect("d5"); // line 9, dead
    let e = s.tuple(&[make_small(5), make_small(50)]).expect("e"); // line 10, live
    let _d6 = s.cons(make_small(906), NIL).expect("d6"); // line 11, dead
    let f = s.tuple(&[make_small(6), make_small(60)]).expect("f"); // line 12, live
    let _d7 = s.tuple(&[make_small(907)]).expect("d7"); // line 13, dead
    let g = s.tuple(&[make_small(7), make_small(70)]).expect("g"); // line 14, live
    let _d8 = s.tuple(&[make_small(908)]).expect("d8"); // line 15, dead

    // ---- block 1 (lines 16..32): sixteen live roots — the block fills completely ------
    let mut block_one: Vec<Eterm> = Vec::new();
    for i in 0..LINES_PER_BLOCK as i64 {
        block_one.push(
            s.tuple(&[make_small(100 + i), make_small(200 + i)])
                .expect("live tuple"),
        );
    }

    // ---- block 2 (lines 32..48): sixteen unreachable tuples — a wholly dead block -----
    for i in 0..LINES_PER_BLOCK as i64 {
        let _ = s.tuple(&[make_small(700 + i)]).expect("garbage tuple");
    }

    // All three blocks are occupied before the collection, so every reclamation below is a
    // real reclamation and not a no-op on never-used memory.
    assert_eq!(s.used_lines(), 3 * LINES_PER_BLOCK);
    assert_eq!(s.free_lines(), 0);

    let mut roots: Vec<Eterm> = vec![a, b, holder, c, d, e, f, g];
    roots.extend(block_one.iter().copied());
    let before = roots.clone();
    let first_pinned = roots.len() - LINES_PER_BLOCK; // 8

    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).expect("collect");

    // (a) Every live object survives and every root is relocated into this space.
    for (i, root) in roots.iter().enumerate() {
        assert!(
            s.contains_term(*root),
            "root {i} must be resident in the space after the collection"
        );
    }
    // The partial block's live objects moved; the full block's objects did not.
    for i in 0..first_pinned {
        assert_ne!(
            before[i].raw(),
            roots[i].raw(),
            "live object {i} of the partial block was evacuated"
        );
    }
    for i in first_pinned..roots.len() {
        assert_eq!(
            before[i].raw(),
            roots[i].raw(),
            "live object {i} of the fully-live block was pinned in place"
        );
    }

    // ... with their contents intact.
    let expected: [(usize, [i64; 2]); 7] = [
        (0, [1, 10]),
        (1, [2, 20]),
        (3, [3, 30]),
        (4, [4, 40]),
        (5, [5, 50]),
        (6, [6, 60]),
        (7, [7, 70]),
    ];
    for (i, pair) in expected {
        assert_eq!(
            tuple_to_smalls(roots[i]),
            pair.to_vec(),
            "evacuated object {i} kept its contents"
        );
    }
    // The reference *inside* the evacuated `holder` cons was rewritten to `a`'s new address,
    // so the object graph — not just the roots — followed the evacuation.
    // SAFETY: `roots[2]` is a live cons cell in this space.
    unsafe {
        assert_eq!(
            *list_val(roots[2]),
            roots[0],
            "the holder's car follows the evacuated tuple"
        );
        assert_eq!(*list_val(roots[2]).add(1), NIL);
    }
    for (i, root) in block_one.iter().enumerate() {
        assert_eq!(
            tuple_to_smalls(*root),
            vec![100 + i as i64, 200 + i as i64],
            "pinned object {i} kept its contents"
        );
    }

    // (b) Per-object reclamation: only the live objects of the partial block moved, and the
    // dead ones were reclaimed.
    assert_eq!(
        stats.objects_evacuated, 8,
        "exactly the live objects of the partially-live block moved"
    );
    assert_eq!(stats.lines_evacuated, 8);
    assert!(
        stats.lines_evacuated < LINES_PER_BLOCK,
        "a partially-live block is NOT evacuated wholesale"
    );
    assert!(stats.words_reclaimed > 0, "garbage was reclaimed");
    assert_eq!(
        stats.words_reclaimed,
        24 * LINE_WORDS,
        "8 dead lines of the partial block + the whole dead block"
    );
    assert_eq!(stats.blocks_reclaimed, 1, "only the wholly dead block");
    assert_eq!(s.used_lines(), 24, "8 evacuated + 16 pinned lines remain");
    assert_eq!(
        s.free_lines(),
        24,
        "the 8 dead lines of the partial block plus the reclaimed block"
    );

    // (c) The fully-live block was pinned, not evacuated: its objects are byte-identical.
    assert_eq!(stats.objects_pinned, LINES_PER_BLOCK);
    assert_eq!(stats.lines_pinned, LINES_PER_BLOCK);
    assert_eq!(stats.words_pinned, 3 * LINES_PER_BLOCK);

    // The accounting view classifies the three blocks exactly as expected.
    let counters = s.counters();
    assert_eq!(
        (
            counters.blocks_all_marked,
            counters.blocks_partial,
            counters.blocks_free
        ),
        (1, 1, 1)
    );
    assert_eq!(counters.lines_free, 24);

    // (3) The space is immediately reusable: the first freed line is handed straight back.
    let reused = s.cons(make_small(11), NIL).expect("reuse a hole");
    assert_eq!(
        list_val(reused),
        base,
        "the first freed line (block 0, line 0) was reused"
    );
    assert_eq!(list_to_smalls(reused), vec![11]);
    let mut reclaimed: Vec<Eterm> = Vec::new();
    for i in 0..23i64 {
        reclaimed.push(s.tuple(&[make_small(300 + i)]).expect("reclaimed line"));
    }
    assert_eq!(
        s.free_lines(),
        0,
        "every line the collection freed was handed out again"
    );
    for (i, term) in reclaimed.iter().enumerate() {
        assert_eq!(tuple_to_smalls(*term), vec![300 + i as i64]);
    }
}

// ---------------------------------------------------------------------------
// 2: a pinned object's reference to an evacuated object
// ---------------------------------------------------------------------------

#[test]
fn a_pinned_object_s_reference_is_evacuated_follows_the_copy() {
    // The hard combination: a fully-live block holds the only reference to an object in a
    // partially-live block. The reference's target is evacuated (so it moves), while the
    // holder is pinned (so it does not) — the holder's word must nevertheless be rewritten
    // to the copy's address, and the holder's own address must not change.
    let mut s = space(3 * BLOCK_WORDS);

    // ---- block 0: one live tuple and fifteen dead ones (partially live) ---------------
    let target = s.tuple(&[make_small(1), make_small(2)]).expect("target"); // line 0, live
    for i in 0..(LINES_PER_BLOCK - 1) as i64 {
        let _ = s.tuple(&[make_small(800 + i)]).expect("dead"); // lines 1..15
    }

    // ---- block 1: sixteen live objects filling it completely --------------------------
    let holder = s.cons(target, NIL).expect("holder"); // line 16, live
    let mut rest: Vec<Eterm> = Vec::new();
    for i in 0..(LINES_PER_BLOCK - 1) as i64 {
        rest.push(
            s.tuple(&[make_small(i), make_small(100 + i)])
                .expect("live tuple"),
        );
    }

    // ---- block 2: wholly dead ---------------------------------------------------------
    for i in 0..LINES_PER_BLOCK as i64 {
        let _ = s.tuple(&[make_small(600 + i)]).expect("garbage");
    }
    assert_eq!(s.used_lines(), 3 * LINES_PER_BLOCK);
    assert_eq!(s.free_lines(), 0);

    let mut roots: Vec<Eterm> = vec![target, holder];
    roots.extend(rest.iter().copied());
    let before = roots.clone();

    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).expect("collect");

    // The target moved (its block is partially live) ...
    assert!(s.contains_term(roots[0]));
    assert_ne!(roots[0].raw(), target.raw(), "the target was evacuated");
    assert_eq!(tuple_to_smalls(roots[0]), vec![1, 2]);
    // ... the holder did not (its block is fully live) ...
    assert_eq!(
        roots[1].raw(),
        holder.raw(),
        "the holder was pinned by the full-block rule"
    );
    assert_eq!(stats.objects_evacuated, 1);
    assert_eq!(stats.objects_pinned, LINES_PER_BLOCK);
    assert_eq!(stats.lines_pinned, LINES_PER_BLOCK);
    assert_eq!(
        stats.words_pinned,
        2 + 15 * 3,
        "one cons cell + fifteen tuples"
    );
    assert_eq!(stats.blocks_reclaimed, 1, "the wholly dead block");
    // ... and the pinned holder's word now names the copy, not the vacated address.
    // SAFETY: `roots[1]` is a live cons cell in this space.
    unsafe {
        assert_eq!(
            *list_val(roots[1]),
            roots[0],
            "the pinned object's reference follows the evacuated target"
        );
        assert_eq!(*list_val(roots[1]).add(1), NIL);
    }
    for (i, root) in rest.iter().enumerate() {
        assert_eq!(
            root.raw(),
            before[2 + i].raw(),
            "pinned object {i} stayed put"
        );
    }
    // 48 - 17 = 31 lines were freed: block 0's fifteen dead lines and the whole dead block,
    // plus the line the evacuated target left behind.
    assert_eq!(s.used_lines(), 17);
    assert_eq!(s.free_lines(), 31);
    assert_eq!(stats.words_reclaimed, 31 * LINE_WORDS);
}

// ---------------------------------------------------------------------------
// 4: the "no hole large enough" pinning rule
// ---------------------------------------------------------------------------

/// An object too large for any hole is **retained in place**, and references to it stay put.
///
/// A 3-line object in a block whose remaining free lines are all isolated cannot be moved
/// anywhere: the documented rule (`ImmixStats::objects_pinned`: "live objects left in place
/// … because the space had no hole large enough") is to retain it where it is and evacuate
/// the one-line objects around it into the one-line holes. The collection is perfectly
/// satisfiable: one pinned object (`objects_pinned` 1, `lines_pinned` 3, `words_pinned` 33),
/// six evacuated ones, `blocks_reclaimed` 0, nine lines retained and therefore seven free
/// lines (`words_reclaimed == 7 * LINE_WORDS`).
///
/// For that to hold, `ImmixSpace::classify_ref` (`src/immix/collect.rs`) must answer "must
/// this reference be moved?" from the `kept` bitmap that records "retained in place" *as
/// well as* the line's `allocated`/`marked` bits and the block state: a pinned line
/// classifies as `Target::Foreign`. Otherwise a hole-pinned object still classifies as
/// `Live`, and `evacuate`'s final root-and-slot forwarding pass tries to copy it a second
/// time, finds no hole (the very reason it was pinned) and fails the whole collection with
/// a spurious `BeamError::Alloc(AllocFailure { kind: Heap, .. })` — after the walk has
/// already written forwarding markers into the objects it evacuated.
#[test]
fn object_pinned_for_want_of_a_fitting_hole_is_retained_and_its_reference_is_left_verbatim() {
    // One block, sixteen lines: a 32-element tuple spans lines 0..2; one live cons cell
    // (holding the large tuple) and five live one-line tuples occupy lines 3,5,7,9,11,13,
    // and lines 4,6,8,10,12,14,15 are dead — seven free lines, none of them part of a run
    // of three.
    let mut s = space(BLOCK_WORDS);

    let elems: Vec<Eterm> = (0..32).map(|i| make_small(i as i64)).collect();
    let big = s.tuple(&elems).expect("large tuple"); // lines 0,1,2 — live
    let holder = s.cons(big, NIL).expect("holder"); // line 3, live — refers to `big`
    let _d1 = s.tuple(&[make_small(901)]).expect("d1"); // line 4, dead
    let l2 = s.tuple(&[make_small(2)]).expect("l2"); // line 5, live
    let _d2 = s.tuple(&[make_small(902)]).expect("d2"); // line 6, dead
    let l3 = s.tuple(&[make_small(3)]).expect("l3"); // line 7, live
    let _d3 = s.tuple(&[make_small(903)]).expect("d3"); // line 8, dead
    let l4 = s.tuple(&[make_small(4)]).expect("l4"); // line 9, live
    let _d4 = s.tuple(&[make_small(904)]).expect("d4"); // line 10, dead
    let l5 = s.tuple(&[make_small(5)]).expect("l5"); // line 11, live
    let _d5 = s.tuple(&[make_small(905)]).expect("d5"); // line 12, dead
    let l6 = s.tuple(&[make_small(6)]).expect("l6"); // line 13, live
    let _d6 = s.tuple(&[make_small(906)]).expect("d6"); // line 14, dead
    let _d7 = s.tuple(&[make_small(907)]).expect("d7"); // line 15, dead

    assert_eq!(s.used_lines(), LINES_PER_BLOCK, "the block is full");
    assert_eq!(s.free_lines(), 0);
    // The marks are exactly the nine live lines (three of the large tuple plus the six
    // one-line objects), so the block classifies as `Partial` — the large object is *not*
    // saved by the full-block rule.
    let mut roots: Vec<Eterm> = vec![big, holder, l2, l3, l4, l5, l6];
    let before = roots.clone();
    s.mark(&roots, &[]).expect("mark");
    assert_eq!(s.counters().lines_marked, 9);
    assert_eq!(s.counters().blocks_partial, 1);
    assert_eq!(s.counters().blocks_all_marked, 0);

    // The collection is satisfiable and must *succeed*: the large tuple is retained in
    // place and the one-line objects around it are evacuated into the one-line holes.
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).expect("collect");

    // (a) The large tuple was pinned for want of a fitting hole, not evacuated: its own
    // address is byte-identical before and after, and it is reported as pinned.
    assert_eq!(
        roots[0].raw(),
        big.raw(),
        "the object pinned for want of a fitting hole stayed in place"
    );
    assert_eq!(stats.objects_pinned, 1);
    assert_eq!(stats.lines_pinned, 3);
    assert_eq!(stats.words_pinned, 33, "a 32-element tuple is 33 words");
    assert_eq!(tuple_to_smalls(roots[0]), (0..32i64).collect::<Vec<_>>());

    // (b) Every live root still resolves inside the space, and the six one-line objects
    // were evacuated into the holes (their addresses changed).
    for (i, root) in roots.iter().enumerate() {
        assert!(
            s.contains_term(*root),
            "root {i} must be resident in the space after the collection"
        );
    }
    for i in 1..roots.len() {
        assert_ne!(
            before[i].raw(),
            roots[i].raw(),
            "one-line object {i} was evacuated into a fitting hole"
        );
    }
    assert_eq!(stats.objects_evacuated, 6);
    assert_eq!(stats.lines_evacuated, 6);
    assert_eq!(
        stats.blocks_reclaimed, 0,
        "the block still holds the pinned object"
    );

    // (c) The evacuated holder's reference to the pinned object resolves to the pinned
    // object's (unchanged) address — left verbatim, not copied a second time and not fatal.
    // SAFETY: `roots[1]` is a live cons cell in this space.
    unsafe {
        assert_eq!(
            *list_val(roots[1]),
            roots[0],
            "the reference to the pinned object is left verbatim"
        );
        assert_eq!(*list_val(roots[1]).add(1), NIL);
    }
    // The remaining one-line tuples kept their contents.
    for (i, term) in roots[2..].iter().enumerate() {
        assert_eq!(tuple_to_smalls(*term), vec![i as i64 + 2]);
    }

    // Nine lines survive (three for the pinned object + six destination lines), so seven of
    // the block's sixteen lines are free again.
    assert_eq!(s.used_lines(), 9);
    assert_eq!(s.free_lines(), 7);
    assert_eq!(stats.words_reclaimed, 7 * LINE_WORDS);
}
