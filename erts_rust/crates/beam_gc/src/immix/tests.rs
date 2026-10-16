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

//! Unit tests for the GenImmix-style mark-region mature space
//! ([`ImmixSpace`](super::ImmixSpace)).
//!
//! These are in-crate tests (the module is a child of `immix`, so it sees the private
//! block metadata, the line bitmaps and the geometry helpers). They cover, in order:
//!
//! * **geometry** — `BLOCK_WORDS`/`LINE_WORDS` rounding, `lines_for`, `base`/`contains`,
//!   the line/word accounting accessors;
//! * **line-aware allocation** — sequential line placement, whole-line consumption, a
//!   request larger than a block claiming a block run, exhaustion, and the **reuse of a
//!   hole a collection created**;
//! * **releasing a reservation** — the `release_words`/`release_lines` inverse of `claim`
//!   (the promotion-rollback primitive): a released run becomes free and allocatable again at
//!   its original address, a block whose last line is released returns to `Free`, a
//!   multi-block run returns whole blocks, and a zero-length release is a no-op;
//! * **marking** — a nested tuple/cons/boxed graph is marked line by line from a flat
//!   root slice, and the `promoted` incoming set is treated as additional roots;
//! * **block classification** — `Free` / `Partial` / `AllMarked` from the marks;
//! * **evacuation** — a partially-live block is evacuated per object and its roots are
//!   rewritten, while a fully-live (`AllMarked`) block is **pinned** (its words do not
//!   move); a large multi-block object is pinned too;
//! * **out-of-space references** — a literal and another space's term are left
//!   byte-identical;
//! * **the shared object-sizing rules** — a fun (an inlined-descriptor thing) is sized by
//!   `move_size` and its environment words are visited, proving `immix` reuses `copy.rs`'s
//!   primitive instead of a private copy;
//! * **accounting** — the [`ImmixStats`]/[`ImmixCounters`] counters and `collections()`.
//!
//! `unsafe` below only dereferences pointers this test allocated itself (through the
//! space's own builders) or that a collection just wrote; every block carries a
//! `// SAFETY:` rationale.

use beam_alloc::default_allocator;
use beam_common::{AllocKind, BeamError};
use beam_term::prelude::{
    boxed_val, fun_env, fun_num_free, fun_words, list_val, make_arityval, make_boxed, make_small,
    signed_value, write_fun_local, Eterm, FunKind, NIL, TAG_LITERAL_PTR,
};

use super::*;

/// An `ImmixSpace` of at least `words` term words over the default allocator.
fn space(words: usize) -> ImmixSpace {
    ImmixSpace::with_allocator(default_allocator().clone(), words).expect("immix space")
}

/// Walks a proper list of small integers, returning them in order.
fn list_to_smalls(mut term: Eterm) -> Vec<i64> {
    let mut values = Vec::new();
    while is_list(term) {
        // SAFETY: `term` is a live cons cell (established by `is_list`), so both words
        // are in-bounds and initialised.
        let cell = list_val(term);
        values.push(signed_value(unsafe { *cell }));
        term = unsafe { *cell.add(1) };
    }
    assert_eq!(term, NIL, "the list must be proper");
    values
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

#[test]
fn geometry_is_block_and_line_granular() {
    // The documented geometry: 128-byte lines, 16 lines per 2 KiB block.
    assert_eq!(LINE_WORDS, 16);
    assert_eq!(LINES_PER_BLOCK, 16);
    assert_eq!(BLOCK_WORDS, LINE_WORDS * LINES_PER_BLOCK);

    // `lines_for` rounds up, and a zero-word request occupies no line.
    assert_eq!(lines_for(0), 0);
    assert_eq!(lines_for(1), 1);
    assert_eq!(lines_for(LINE_WORDS), 1);
    assert_eq!(lines_for(LINE_WORDS + 1), 2);
    assert_eq!(lines_for(BLOCK_WORDS), LINES_PER_BLOCK);
    assert_eq!(lines_for(BLOCK_WORDS + 1), LINES_PER_BLOCK + 1);

    // A sub-block request rounds up to exactly one block.
    let s = space(1);
    assert_eq!(s.block_count(), 1);
    assert_eq!(s.capacity_words(), BLOCK_WORDS);
    assert_eq!(s.line_count(), LINES_PER_BLOCK);
    assert_eq!(s.used_lines(), 0);
    assert_eq!(s.free_lines(), LINES_PER_BLOCK);
    assert_eq!(s.used_words(), 0);
    assert_eq!(s.available_words(), BLOCK_WORDS);
    assert_eq!(s.collections(), 0);

    // A multi-block request rounds up too.
    let s = space(BLOCK_WORDS + 1);
    assert_eq!(s.block_count(), 2);
    assert_eq!(s.capacity_words(), 2 * BLOCK_WORDS);
    assert_eq!(s.line_count(), 2 * LINES_PER_BLOCK);
}

#[test]
fn contains_covers_exactly_the_region() {
    let s = space(BLOCK_WORDS);
    let base = s.base();
    assert!(s.contains(base));
    assert!(s.contains(base.wrapping_add(BLOCK_WORDS - 1)));
    assert!(!s.contains(base.wrapping_add(BLOCK_WORDS)), "one past the end");
    assert!(!s.contains(base.wrapping_sub(1)), "one before the base");
    // `contains_term` only accepts boxed/list terms that land inside.
    assert!(!s.contains_term(make_small(1)));
}

// ---------------------------------------------------------------------------
// Allocation
// ---------------------------------------------------------------------------

#[test]
fn allocation_is_line_granular_and_sequential() {
    let mut s = space(BLOCK_WORDS);
    let base = s.base();

    // Every request consumes whole lines and returns a line-aligned pointer.
    let p0 = unsafe { s.alloc_words(2).unwrap() };
    assert_eq!(p0, base);
    let p1 = unsafe { s.alloc_words(3).unwrap() };
    assert_eq!(p1, base.wrapping_add(LINE_WORDS));
    let p2 = unsafe { s.alloc_words(LINE_WORDS + 1).unwrap() };
    assert_eq!(p2, base.wrapping_add(2 * LINE_WORDS));
    assert_eq!(s.used_lines(), 4);
    let p3 = unsafe { s.alloc_words(1).unwrap() };
    assert_eq!(p3, base.wrapping_add(4 * LINE_WORDS));
    assert_eq!(s.used_lines(), 5);
    assert_eq!(s.used_words(), 5 * LINE_WORDS);
    assert_eq!(s.available_words(), (LINES_PER_BLOCK - 5) * LINE_WORDS);

    // A zero-word request claims nothing.
    let pz = unsafe { s.alloc_words(0).unwrap() };
    assert_eq!(pz, base);
    assert_eq!(s.used_lines(), 5);
}

#[test]
fn can_reserve_never_over_reports() {
    let mut s = space(BLOCK_WORDS);
    assert!(s.can_reserve(1));
    assert!(s.can_reserve(BLOCK_WORDS));
    assert!(
        !s.can_reserve(BLOCK_WORDS + 1),
        "a single block cannot hold a two-block object"
    );

    // Fill the whole space; nothing more can be reserved or allocated.
    for _ in 0..LINES_PER_BLOCK {
        let _p = unsafe { s.alloc_words(1).unwrap() };
    }
    assert_eq!(s.free_lines(), 0);
    assert_eq!(s.available_words(), 0);
    assert!(!s.can_reserve(1));
}

#[test]
fn a_full_space_reports_heap_exhaustion() {
    let mut s = space(BLOCK_WORDS);
    for _ in 0..LINES_PER_BLOCK {
        let _p = unsafe { s.alloc_words(1).unwrap() };
    }

    let err = unsafe { s.alloc_words(1).unwrap_err() };
    match err {
        BeamError::Alloc(failure) => {
            assert_eq!(failure.kind, AllocKind::Heap);
            assert_eq!(failure.requested, core::mem::size_of::<Eterm>());
        }
        other => panic!("expected heap exhaustion, got {other:?}"),
    }
}

#[test]
fn cons_and_tuple_builders_write_line_aligned_objects() {
    let mut s = space(BLOCK_WORDS);
    let base = s.base();

    let c = s.cons(make_small(1), make_small(2)).unwrap();
    assert!(is_list(c));
    assert_eq!(list_val(c), base);
    // SAFETY: `c` is a live, two-word cons cell this test just built.
    unsafe {
        assert_eq!(*list_val(c), make_small(1));
        assert_eq!(*list_val(c).add(1), make_small(2));
    }

    let t = s.tuple(&[make_small(3), make_small(4)]).unwrap();
    assert_eq!(boxed_val(t), base.wrapping_add(LINE_WORDS));
    // SAFETY: `t` is a live, three-word tuple this test just built.
    unsafe {
        assert_eq!(*boxed_val(t), make_arityval(2));
        assert_eq!(*boxed_val(t).add(1), make_small(3));
        assert_eq!(*boxed_val(t).add(2), make_small(4));
    }
    assert_eq!(s.used_lines(), 2);
}

#[test]
fn an_object_larger_than_a_block_claims_a_run_of_blocks_and_is_pinned() {
    let mut s = space(2 * BLOCK_WORDS);
    let base = s.base();

    // A 256-element tuple is 257 words, i.e. 17 lines — more than one block.
    let elems: Vec<Eterm> = (0..BLOCK_WORDS).map(|i| make_small(i as i64)).collect();
    let big = s.tuple(&elems).unwrap();
    assert_eq!(boxed_val(big), base);
    assert_eq!(s.used_lines(), LINES_PER_BLOCK + 1);

    // Its first block is entirely covered by its marks, so it is pinned.
    let mut roots = [big];
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).unwrap();
    assert_eq!(
        roots[0].raw(),
        big.raw(),
        "a large object is pinned, not evacuated"
    );
    assert_eq!(stats.objects_pinned, 1);
    assert_eq!(stats.objects_evacuated, 0);
    assert_eq!(stats.lines_pinned, LINES_PER_BLOCK + 1);
    assert_eq!(stats.words_pinned, BLOCK_WORDS + 1);
}

// ---------------------------------------------------------------------------
// Hole reuse (the objective's "holes left by a previous collection must be reusable")
// ---------------------------------------------------------------------------

#[test]
fn a_collection_creates_a_reusable_hole() {
    let mut s = space(BLOCK_WORDS);
    let base = s.base();

    let live = s.cons(make_small(1), make_small(2)).unwrap(); // line 0
    let _garbage = s.cons(make_small(3), make_small(4)).unwrap(); // line 1
    assert_eq!(s.used_lines(), 2);

    let mut roots = [live];
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).unwrap();

    // The live cell was evacuated off line 0 into the free line 1, and the root follows.
    let moved = roots[0];
    assert_ne!(moved.raw(), live.raw(), "the live cell was evacuated");
    assert!(s.contains_term(moved));
    assert_eq!(list_val(moved), base.wrapping_add(LINE_WORDS));
    // SAFETY: `moved` is the live, copied cons cell.
    unsafe {
        assert_eq!(*list_val(moved), make_small(1));
        assert_eq!(*list_val(moved).add(1), make_small(2));
    }

    assert_eq!(stats.objects_evacuated, 1);
    assert_eq!(stats.objects_pinned, 0);
    assert_eq!(stats.blocks_reclaimed, 0, "the block still holds a live line");
    assert_eq!(stats.words_reclaimed, LINE_WORDS, "the garbage line");
    assert_eq!(s.used_lines(), 1);

    // The line the collection freed is handed out again — a collected hole is reusable.
    let reused = unsafe { s.alloc_words(1).unwrap() };
    assert_eq!(reused, base, "the hole the collection created is reusable");
    assert_eq!(s.used_lines(), 2);
}

#[test]
fn reset_releases_every_line_and_rereadies_the_allocator() {
    let mut s = space(BLOCK_WORDS);
    let _a = s.cons(make_small(1), NIL).unwrap();
    let _b = s.tuple(&[make_small(2)]).unwrap();
    assert_eq!(s.used_lines(), 2);
    assert_eq!(s.counters().blocks_free, 0, "the block now holds objects");

    s.reset();
    assert_eq!(s.used_lines(), 0);
    assert_eq!(s.free_lines(), LINES_PER_BLOCK);
    assert_eq!(s.counters().blocks_free, 1);

    let p = unsafe { s.alloc_words(1).unwrap() };
    assert_eq!(p, s.base());
}

// ---------------------------------------------------------------------------
// Releasing a reservation (the promotion-rollback primitive)
// ---------------------------------------------------------------------------

#[test]
fn release_un_claims_a_run_and_makes_it_allocatable_again() {
    let mut s = space(BLOCK_WORDS);
    let base = s.base();

    // A 100-word (7-line) object at line 0 and a second at line 7: the block's two-line
    // tail is left free, and no 7-line run remains anywhere in the block.
    let first = unsafe { s.alloc_words(100).unwrap() };
    let second = unsafe { s.alloc_words(100).unwrap() };
    assert_eq!(first, base);
    assert_eq!(second, base.wrapping_add(7 * LINE_WORDS));
    assert_eq!(s.used_lines(), 14);
    assert_eq!(s.free_lines(), 2);
    assert!(!s.can_reserve(100), "the two-line tail cannot hold a 7-line object");
    assert!(unsafe { s.alloc_words(100) }.is_err(), "no run exists");

    // Un-claim the first run: the block keeps its second object and the whole first run is
    // free again — exactly the 7-line hole the failed request above could not find.
    s.release_words(first, 100);
    assert_eq!(s.used_lines(), 7);
    assert_eq!(s.free_lines(), 9);
    assert!(s.can_reserve(100));
    assert_eq!(s.counters().lines_allocated(), 7);
    assert_eq!(s.counters().blocks_partial, 1, "the block is still occupied");

    // The released lines are handed out again, at their original address.
    let reused = unsafe { s.alloc_words(100).unwrap() };
    assert_eq!(reused, base, "the released run is allocatable again");
    assert_eq!(s.used_lines(), 14);
    assert_eq!(s.free_lines(), 2);
}

#[test]
fn release_restores_a_whole_block_to_free() {
    let mut s = space(2 * BLOCK_WORDS);
    let base = s.base();

    // Fill block 0 with sixteen one-line cells; block 1 stays empty.
    let mut cells = Vec::new();
    for _ in 0..LINES_PER_BLOCK {
        cells.push(unsafe { s.alloc_words(1).unwrap() });
    }
    assert_eq!(s.used_lines(), LINES_PER_BLOCK);
    assert_eq!(s.counters().blocks_partial, 1);
    assert_eq!(s.counters().blocks_free, 1);

    // Un-claim every one of them: the block is empty again, i.e. `Free`, the state `claim`
    // found it in.
    for (i, p) in cells.iter().enumerate() {
        assert_eq!(*p, base.wrapping_add(i * LINE_WORDS), "cell {i} is at its own line");
        s.release_words(*p, 1);
    }
    assert_eq!(s.used_lines(), 0);
    assert_eq!(s.free_lines(), 2 * LINES_PER_BLOCK);
    assert_eq!(s.counters().blocks_partial, 0);
    assert_eq!(s.counters().blocks_free, 2);

    // A whole block's worth of lines is allocatable again, from the block's first line.
    let p = unsafe { s.alloc_words(BLOCK_WORDS) }.unwrap();
    assert_eq!(p, base);
    assert_eq!(s.used_lines(), LINES_PER_BLOCK);
}

#[test]
fn release_returns_a_multi_block_run_to_free_blocks() {
    let mut s = space(3 * BLOCK_WORDS);
    let base = s.base();

    // A 257-word (17-line) object: block 0 whole plus block 1's first line.
    let big = unsafe { s.alloc_words(BLOCK_WORDS + 1).unwrap() };
    assert_eq!(big, base);
    assert_eq!(s.used_lines(), LINES_PER_BLOCK + 1);
    assert_eq!(s.counters().blocks_free, 1, "block 2 stays free");

    s.release_words(big, BLOCK_WORDS + 1);
    assert_eq!(s.used_lines(), 0);
    assert_eq!(s.free_lines(), 3 * LINES_PER_BLOCK);
    assert_eq!(s.counters().blocks_partial, 0);
    assert_eq!(s.counters().blocks_free, 3, "every block is free again");

    // The whole region — block 0 plus block 1 — is allocatable again.
    let again = unsafe { s.alloc_words(BLOCK_WORDS + 1).unwrap() };
    assert_eq!(again, base);
}

#[test]
fn releasing_nothing_is_a_no_op() {
    let mut s = space(BLOCK_WORDS);
    let p = unsafe { s.alloc_words(100).unwrap() };
    assert_eq!(s.used_lines(), 7);

    // Both zero forms claim and release nothing.
    s.release_words(p, 0);
    s.release_lines(0, 0);
    assert_eq!(s.used_lines(), 7);
    assert_eq!(s.free_lines(), LINES_PER_BLOCK - 7);
    assert_eq!(s.counters().blocks_partial, 1);
}

// ---------------------------------------------------------------------------
// Marking
// ---------------------------------------------------------------------------

#[test]
fn mark_walks_a_nested_graph_and_classifies_the_block() {
    let mut s = space(BLOCK_WORDS);
    // `{4,5}` at line 0, `[1,2]` at lines 1-2, the outer tuple at line 3, garbage at line 4.
    let inner = s.tuple(&[make_small(4), make_small(5)]).unwrap();
    let c1 = s.cons(make_small(1), NIL).unwrap();
    let c2 = s.cons(make_small(2), c1).unwrap();
    let outer = s.tuple(&[c2, inner]).unwrap();
    let _garbage = s.tuple(&[make_small(8), make_small(9)]).unwrap();

    s.mark(&[outer], &[]).unwrap();

    // Every live object covers exactly one line; the garbage line stays unmarked.
    let c = s.counters();
    assert_eq!(c.lines_marked, 4);
    assert_eq!(c.lines_total, LINES_PER_BLOCK);
    assert_eq!(c.blocks_partial, 1, "some but not all lines are live");
    assert_eq!(c.blocks_free, 0);
    assert_eq!(c.blocks_all_marked, 0);
}

#[test]
fn mark_treats_the_promoted_set_as_additional_roots() {
    let mut s = space(BLOCK_WORDS);
    let _unreachable = s.cons(make_small(1), NIL).unwrap(); // line 0
    let promoted_cell = s.cons(make_small(2), NIL).unwrap(); // line 1

    // `promoted_cell` is not in `roots`, but the incoming set makes it live.
    s.mark(&[], &[promoted_cell]).unwrap();
    assert_eq!(s.counters().lines_marked, 1);

    // A subsequent collection with no roots reclaims everything.
    let mut roots: [Eterm; 0] = [];
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).unwrap();
    assert_eq!(stats.objects_evacuated, 0);
    assert_eq!(stats.objects_pinned, 0);
    assert_eq!(s.used_lines(), 0);
}

// ---------------------------------------------------------------------------
// Block classification + evacuation (pinned vs partial)
// ---------------------------------------------------------------------------

#[test]
fn collection_classifies_blocks_and_pins_the_full_one() {
    let mut s = space(3 * BLOCK_WORDS);
    let mut roots: Vec<Eterm> = Vec::new();

    // Block 0: sixteen live cons cells — fills it completely.
    for i in 0..LINES_PER_BLOCK {
        roots.push(s.cons(make_small(i as i64), NIL).unwrap());
    }
    // Block 1: one live cell (a root) and fifteen garbage cells — fills it completely.
    let live_in_partial = s.cons(make_small(100), NIL).unwrap();
    roots.push(live_in_partial);
    for i in 0..(LINES_PER_BLOCK - 1) {
        let _ = s.cons(make_small(200 + i as i64), NIL).unwrap();
    }
    // Block 2: garbage only (and it *is* occupied, so it is a reclamation, not a no-op).
    let _garbage_free = s.cons(make_small(102), NIL).unwrap();
    assert_eq!(s.used_lines(), 2 * LINES_PER_BLOCK + 1);

    let pinned_before: Vec<Eterm> = roots[..LINES_PER_BLOCK].to_vec();
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).unwrap();

    // Block 0 is fully live → AllMarked → pinned: not one of its cells moved.
    for (before, after) in pinned_before.iter().zip(&roots[..LINES_PER_BLOCK]) {
        assert_eq!(
            before.raw(),
            after.raw(),
            "an AllMarked block is not evacuated"
        );
    }
    // Block 1 is partially live → its live cell was evacuated off its line.
    assert_ne!(roots[LINES_PER_BLOCK].raw(), live_in_partial.raw());
    assert!(s.contains_term(roots[LINES_PER_BLOCK]));
    assert_eq!(list_to_smalls(roots[LINES_PER_BLOCK]), vec![100]);

    // Both halves are accounted for.
    assert_eq!(stats.objects_pinned, LINES_PER_BLOCK);
    assert_eq!(stats.lines_pinned, LINES_PER_BLOCK);
    assert_eq!(stats.words_pinned, LINES_PER_BLOCK * 2);
    assert_eq!(stats.objects_evacuated, 1);
    assert_eq!(stats.lines_evacuated, 1);
    assert_eq!(stats.blocks_reclaimed, 1, "only the garbage-only block");
    assert_eq!(
        stats.words_reclaimed,
        16 * LINE_WORDS,
        "fifteen garbage lines in the partial block plus the garbage block"
    );
    assert_eq!(s.collections(), 1);

    // The accounting view agrees with the block classification.
    let c = s.counters();
    assert_eq!(c.blocks_total, 3);
    assert_eq!(c.blocks_all_marked, 1);
    assert_eq!(c.blocks_partial, 1);
    assert_eq!(c.blocks_free, 1);
    assert_eq!(c.lines_total, 3 * LINES_PER_BLOCK);
    assert_eq!(c.lines_free, (LINES_PER_BLOCK - 1) + LINES_PER_BLOCK);
    assert_eq!(c.lines_allocated(), LINES_PER_BLOCK + 1);
    assert_eq!(c.used_words, (LINES_PER_BLOCK + 1) * LINE_WORDS);
    assert_eq!(c.available_words(), c.lines_free * LINE_WORDS);
    let expected = c.lines_free as f64 / c.lines_total as f64;
    assert_eq!(c.reclaimable(), expected);
}

#[test]
fn a_free_block_is_reclaimed_entirely() {
    let mut s = space(BLOCK_WORDS);
    // Fill the block with garbage that no root reaches.
    for i in 0..LINES_PER_BLOCK {
        let _ = s.cons(make_small(i as i64), NIL).unwrap();
    }
    assert_eq!(s.used_lines(), LINES_PER_BLOCK);

    let mut roots: [Eterm; 0] = [];
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).unwrap();

    assert_eq!(stats.blocks_reclaimed, 1);
    assert_eq!(stats.objects_evacuated, 0);
    assert_eq!(stats.objects_pinned, 0);
    assert_eq!(stats.words_reclaimed, LINES_PER_BLOCK * LINE_WORDS);
    assert_eq!(s.used_lines(), 0);
    assert_eq!(s.counters().blocks_free, 1);
}

// ---------------------------------------------------------------------------
// Out-of-space references
// ---------------------------------------------------------------------------

#[test]
fn out_of_space_references_are_left_verbatim() {
    let mut s = space(BLOCK_WORDS);
    let mut other = space(BLOCK_WORDS);

    let _live = s.cons(make_small(1), make_small(2)).unwrap(); // live but unreachable
    let foreign = other.cons(make_small(9), make_small(8)).unwrap();

    // A boxed literal that lives outside every space, with the literal pointer bit set.
    let external = [
        make_arityval(2),
        make_small(11),
        make_small(22),
    ];
    let literal = Eterm::from_raw(make_boxed(external.as_ptr()).raw() | TAG_LITERAL_PTR);

    let mut roots = [foreign, literal];
    let before = roots;
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).unwrap();

    // Neither the other space's term nor the literal was touched.
    assert_eq!(roots[0].raw(), before[0].raw());
    assert_eq!(roots[1].raw(), before[1].raw());
    assert_eq!(stats.objects_evacuated, 0);
    assert_eq!(stats.objects_pinned, 0);
    // `_live` was unreachable and is reclaimed; `other` is untouched by this collection.
    assert_eq!(s.used_lines(), 0);
    assert_eq!(other.used_lines(), 1);
    assert!(other.contains_term(foreign));
}

// ---------------------------------------------------------------------------
// The shared object-sizing rules (funs use `copy.rs`'s `move_size`)
// ---------------------------------------------------------------------------

#[test]
fn a_fun_is_sized_and_its_environment_relocated_by_the_shared_rules() {
    let mut s = space(BLOCK_WORDS);

    // A compound free variable (line 0) that must be relocated with the fun (line 1).
    let free = s.tuple(&[make_small(1), make_small(2)]).unwrap();
    let words = fun_words(FunKind::Local, 1); // 1 + desc_words(Local) + num_free == 4
    assert_eq!(words, 4);
    assert_eq!(lines_for(words), 1);
    // SAFETY: `alloc_words` hands out the 4 writable words `write_fun_local` fills; the
    // single free variable is a live term of this space.
    let fun = unsafe {
        let hp = s.alloc_words(words).unwrap();
        write_fun_local(hp, 2, 0xFEED, 3, &[free]).unwrap()
    };

    let mut roots = [fun];
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).unwrap();

    let copied = roots[0];
    assert!(s.contains_term(copied));
    assert_ne!(copied.raw(), fun.raw(), "the fun was evacuated");

    // The fun block's inlined-descriptor size came from the shared `move_size`.
    // SAFETY: `copied` is the live, copied fun.
    assert_eq!(fun_num_free(unsafe { *boxed_val(copied) }), 1);
    let env = unsafe { fun_env(copied) }.unwrap();
    assert_eq!(env.len(), 1);
    assert!(
        s.contains_term(env[0]),
        "the fun's environment word was visited and relocated"
    );
    assert_ne!(env[0].raw(), free.raw());
    // SAFETY: `env[0]` is the live, copied `{1,2}` tuple.
    unsafe {
        assert_eq!(*boxed_val(env[0]), make_arityval(2));
        assert_eq!(*boxed_val(env[0]).add(1), make_small(1));
        assert_eq!(*boxed_val(env[0]).add(2), make_small(2));
    }

    // Exactly the fun and the tuple were evacuated, one line each.
    assert_eq!(stats.objects_evacuated, 2);
    assert_eq!(stats.objects_pinned, 0);
    assert_eq!(s.used_lines(), 2);
}
