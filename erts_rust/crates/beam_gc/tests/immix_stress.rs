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

//! **Stress evidence for the GenImmix path** — the long-running behaviour of
//! [`ImmixSpace`] / [`GenImmixCollector`] / [`AnyCollector`] that the single-collection
//! tests (`immix_partial_block.rs`, `gen_immix_collector.rs`, `immix_parity.rs`) do not
//! reach. Five independent stress cases, all through the **public** API:
//!
//! 1. **Large-object promotion.** Objects larger than one [`BLOCK_WORDS`] allocate a run of
//!    whole blocks and their first block is fully covered by their own marks, so they are
//!    always pinned. Promoted through a minor, they survive a major untouched, and a *mixed*
//!    set of live and dead large objects is reclaimed correctly (the dead run's lines come
//!    back, the live run keeps its address).
//! 2. **Fragmentation/reuse over repeated collections.** A space is driven round after round
//!    through *many* collections; each round reclaims every non-live line and then re-fills
//!    the space exactly, so `free_lines() == 0` after the fill proves the freed lines were
//!    handed back out. `used_lines() + free_lines() == line_count()` and the `ImmixCounters`
//!    view stay consistent, and the survivors are never corrupted.
//! 3. **Repeated minors with mature promotion.** A nursery term promotes into the mature
//!    space on a minor and then survives several further minors unchanged (a minor never
//!    touches the mature space); the mature garbage it leaves when it is dropped is *not*
//!    reclaimed by a minor either — only a major reclaims it.
//! 4. **Mixed workload across both strategies.** A graph mixing cons cells, nested tuples, a
//!    `>32`-key HAMT map, an on-heap binary and bitstring and all three fun kinds is driven
//!    through an identical sequence of minors and majors under [`AnyCollector::Copying`] and
//!    [`AnyCollector::Immix`]; every root must stay `compare::eq` to its baseline under both,
//!    and the two strategies must agree on the collection kinds.
//! 5. **Memory accounting.** The retained footprint (`used_lines() * LINE_WORDS`) is asserted
//!    with exact/derived arithmetic against the live set — never below it, never more than
//!    the documented per-object line-rounding slack above it — and dropping live objects and
//!    collecting again is asserted to *reduce* the footprint by exactly the dropped lines.
//!
//! `unsafe` below only dereferences pointers this test allocated and wrote itself (or that a
//! collection just wrote); every block carries a `// SAFETY:` rationale.

use beam_alloc::Heap;
use beam_gc::prelude::{
    lines_for, AnyCollector, CollectionKind, GcConfig, GenImmixCollector, HeapPolicy, ImmixSpace,
    MatureKind, BLOCK_WORDS, LINES_PER_BLOCK, LINE_WORDS,
};
use beam_term::compare;
use beam_term::prelude::{
    arityval, boxed_val, fun_words, heap_bits_size_words, is_arity_value, is_list, is_small,
    list_val, make_atom, make_small, map_words_capacity, signed_value, write_fun_external,
    write_fun_imported, write_fun_local, write_heap_bits, write_map, Atom, AtomTable, Eterm,
    FunKind, FunPid, NIL,
};

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Words of an [`Eterm`].
const WORD: usize = core::mem::size_of::<Eterm>();

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

/// Element `i` of the live tuple `term`.
fn tuple_elem(term: Eterm, i: usize) -> Eterm {
    // SAFETY: `term` is a live tuple built by this test (or relocated by a collection); the
    // element index is asserted to be in range.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "not a tuple header");
    assert!(i < arityval(header), "element {i} is inside the tuple");
    unsafe { *boxed_val(term).add(1 + i) }
}

/// The whole blocks `words` term words need — `find_run`'s `k` for the request.
fn run_blocks(words: usize) -> usize {
    lines_for(words).div_ceil(LINES_PER_BLOCK)
}

/// The word offset of a boxed/list term from the start of `space`'s region.
fn block_offset(space: &ImmixSpace, term: Eterm) -> usize {
    (boxed_val(term) as usize - space.base() as usize) / WORD
}

/// Asserts `root` is a multi-block object resident in `space`: an `arity`-element tuple whose
/// element `i` is the small integer `i`, starting on a block boundary (the whole-block run
/// `find_run` gives a large request — which is also why its first block is fully marked).
fn assert_multi_block(space: &ImmixSpace, root: Eterm, arity: usize) {
    assert!(
        space.contains_term(root),
        "the multi-block object is resident in the space"
    );
    // SAFETY: `contains_term` established that `root` is a boxed/list term pointing into this
    // space; the header check below establishes it is a live tuple.
    let header = unsafe { *boxed_val(root) };
    assert!(is_arity_value(header), "the object is a tuple");
    assert_eq!(arityval(header), arity, "the tuple keeps its arity");
    // SAFETY: elements 0..arity follow the validated header of a live tuple.
    for i in 0..arity {
        let elem = unsafe { *boxed_val(root).add(1 + i) };
        assert!(is_small(elem), "element {i} is an immediate");
        assert_eq!(signed_value(elem), i as i64, "element {i} survives");
    }
    assert_eq!(
        block_offset(space, root) % BLOCK_WORDS,
        0,
        "a multi-block object starts on a block boundary"
    );
}

/// Asserts the live cons cell `root` is resident in `space` and holds `[value, NIL]`.
fn assert_small_cell(space: &ImmixSpace, root: Eterm, value: i64) {
    assert!(space.contains_term(root), "the cell is resident in the space");
    let cell = list_val(root);
    // SAFETY: `root` is a live cons cell (validated by `contains_term`/`list_val`).
    assert_eq!(signed_value(unsafe { *cell }), value);
    // SAFETY: the second word of a read cons cell.
    assert_eq!(unsafe { *cell.add(1) }, NIL);
}

// ---------------------------------------------------------------------------
// 1: large-object promotion and reclamation of a mixed live/dead set
// ---------------------------------------------------------------------------

/// Element count of the two-block fixture: `BLOCK_WORDS` elements occupy `BLOCK_WORDS + 1`
/// words — 17 lines, one whole line more than a block, so it needs a two-block run.
const LARGE_ARITY: usize = BLOCK_WORDS;

/// The words the [`LARGE_ARITY`]-element tuple occupies (its header plus its elements).
const LARGE_WORDS: usize = LARGE_ARITY + 1;

/// Element count of the three-block fixture: `2 * BLOCK_WORDS + 1` words, i.e. 33 lines — a
/// three-block run, a *different* run length from [`LARGE_ARITY`].
const WIDE_ARITY: usize = 2 * BLOCK_WORDS;

/// The words the [`WIDE_ARITY`]-element tuple occupies.
const WIDE_WORDS: usize = WIDE_ARITY + 1;

/// The one-line live cons cells promoted alongside the runs.
const SMALL_LIVE: usize = 6;

/// A policy whose nursery holds the fixtures and whose mature region is whole blocks (so a
/// multi-block run has room), with a long fullsweep cadence so each test picks the collection
/// it wants to drive.
///
/// `min_heap_words` equals the mature capacity, which makes `mature_after_fullsweep` return
/// the current capacity (`should_shrink` cannot fire, and the grow branch is floored back to
/// the minimum), so an explicit `collect_major` reclaims the mature generation **in place**
/// and `last_immix_stats()` is observable. It also stops the post-collection nursery resize
/// from shrinking the nursery.
fn large_policy() -> HeapPolicy {
    HeapPolicy {
        initial_young_words: 4096,
        initial_mature_words: 8 * BLOCK_WORDS,
        min_heap_words: 8 * BLOCK_WORDS,
        fullsweep_after: 65535,
        ..HeapPolicy::default()
    }
}

/// Builds a tuple of `arity` elements (element `i` is the small integer `i`) in the nursery.
fn build_tuple(c: &mut GenImmixCollector, arity: usize) -> Eterm {
    let elems: Vec<Eterm> = (0..arity).map(|i| make_small(i as i64)).collect();
    c.young_mut()
        .heap_mut()
        .tuple(&elems)
        .expect("the tuple fits the nursery")
}

/// Promotes a two-block and a three-block run plus [`SMALL_LIVE`] one-line cells, and asserts
/// the exact placement and accounting of the resulting mature generation.
fn promote_two_runs(c: &mut GenImmixCollector) -> Vec<Eterm> {
    let narrow = build_tuple(c, LARGE_ARITY);
    let wide = build_tuple(c, WIDE_ARITY);
    let mut roots: Vec<Eterm> = vec![narrow, wide];
    for i in 0..SMALL_LIVE as i64 {
        roots.push(
            c.young_mut()
                .heap_mut()
                .cons(make_small(i), NIL)
                .expect("live cell"),
        );
    }
    // Deliberate nursery garbage that must never reach the mature space.
    for i in 0..4i64 {
        c.young_mut()
            .heap_mut()
            .cons(make_small(900 + i), make_small(i))
            .expect("garbage cell");
    }

    let minor = c.collect_minor(&mut roots).expect("the two runs fit the mature region");
    assert_eq!(
        minor.objects_copied,
        2 + SMALL_LIVE,
        "the two runs plus the live cells were promoted"
    );
    assert_eq!(
        minor.words_copied,
        LARGE_WORDS + WIDE_WORDS + 2 * SMALL_LIVE,
        "only the live words moved"
    );
    assert_eq!(c.young().used_words(), 0, "the nursery is emptied");

    // A request larger than a block claims a whole run of blocks, so the two runs land on
    // block boundaries and the small cells take the remaining holes.
    assert_eq!(run_blocks(LARGE_WORDS), 2);
    assert_eq!(run_blocks(WIDE_WORDS), 3);
    assert_eq!(block_offset(c.mature(), roots[0]), 0, "the first run starts at the region base");
    assert_eq!(
        block_offset(c.mature(), roots[1]),
        2 * BLOCK_WORDS,
        "the three-block run skips the two blocks the first run occupies"
    );
    assert_multi_block(c.mature(), roots[0], LARGE_ARITY);
    assert_multi_block(c.mature(), roots[1], WIDE_ARITY);
    for (i, &cell) in roots[2..].iter().enumerate() {
        assert_small_cell(c.mature(), cell, i as i64);
    }

    // Mature lines: 17 for the two-block run + 33 for the three-block run + one per cell.
    let expected_lines =
        lines_for(LARGE_WORDS) + lines_for(WIDE_WORDS) + SMALL_LIVE;
    assert_eq!(expected_lines, 56);
    assert_eq!(
        c.mature().used_lines(),
        expected_lines,
        "only live objects claim mature lines (the nursery garbage did not)"
    );
    roots
}

/// A promoted multi-block run is pinned by the *following* fullsweep: its start block is
/// wholly covered by its own marks (`AllMarked`), so it is never evacuated and keeps its
/// address; the one-line cells around it are reclaimed in place.
#[test]
fn large_objects_promote_into_block_runs_and_a_major_pins_them() {
    let mut c = GenImmixCollector::from_policy(large_policy()).expect("collector");
    assert_eq!(c.mature().block_count(), 8, "the mature region is whole blocks");

    let mut roots = promote_two_runs(&mut c);
    let before: Vec<Eterm> = roots.clone();
    let lines_before = c.mature().used_lines();

    // Nothing new in the nursery, so the fullsweep only reclaims the mature generation.
    let major = c.collect_major(&mut roots).expect("the fullsweep keeps both runs");
    assert_eq!(major.objects_copied, 0, "the nursery had nothing to promote");

    // Both runs are pinned: their start blocks are fully covered by their own marks.
    assert_eq!(
        roots[0].raw(),
        before[0].raw(),
        "the two-block run keeps its address"
    );
    assert_eq!(
        roots[1].raw(),
        before[1].raw(),
        "the three-block run keeps its address"
    );
    assert_multi_block(c.mature(), roots[0], LARGE_ARITY);
    assert_multi_block(c.mature(), roots[1], WIDE_ARITY);

    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(immix.objects_pinned, 2, "both runs are pinned, neither is evacuated");
    assert_eq!(immix.objects_evacuated, SMALL_LIVE, "the one-line cells moved");
    assert_eq!(
        immix.lines_pinned,
        lines_for(LARGE_WORDS) + lines_for(WIDE_WORDS)
    );
    assert_eq!(immix.words_pinned, LARGE_WORDS + WIDE_WORDS);
    assert_eq!(immix.blocks_reclaimed, 0, "every block still holds live data");
    assert_eq!(major.words_reclaimed, 0, "nothing died");
    assert_eq!(c.mature().used_lines(), lines_before, "the live set is unchanged");

    // The runs' start blocks (and the second block of the three-block run) are all fully
    // marked — the "first block is AllMarked" pinning rule, observed through the counters.
    assert!(
        c.mature().counters().blocks_all_marked >= 3,
        "each run start block is AllMarked"
    );
    assert_eq!(
        c.mature().used_lines() + c.mature().free_lines(),
        c.mature().line_count(),
        "the occupancy view is consistent"
    );

    for (i, &cell) in roots[2..].iter().enumerate() {
        assert_small_cell(c.mature(), cell, i as i64);
    }

    eprintln!(
        "large_objects: promoted lines={lines_before} pinned={} pinned_lines={} evacuated={} \
         all_marked_blocks={}",
        immix.objects_pinned,
        immix.lines_pinned,
        immix.objects_evacuated,
        c.mature().counters().blocks_all_marked
    );
}

/// A **mixed** set of large objects: one lives on, one is dropped. The following fullsweep
/// pins the live run in place and reclaims the dropped run's whole blocks, so the retained
/// footprint falls by exactly the dropped run's line count (minus the fresh promotion).
#[test]
fn a_major_reclaims_a_dead_large_object_and_pins_the_live_one() {
    let mut c = GenImmixCollector::from_policy(large_policy()).expect("collector");
    let mut roots = promote_two_runs(&mut c);
    let lines_after_minor = c.mature().used_lines();
    assert_eq!(lines_after_minor, 56);

    let narrow_addr = roots[0];
    // Drop the three-block run; it becomes mature garbage.
    let dropped = roots.remove(1);
    let dropped_lines = lines_for(WIDE_WORDS);
    assert_eq!(dropped_lines, 33);

    // Two fresh live one-line cells in the nursery, promoted by the fullsweep.
    for i in 0..2i64 {
        roots.push(
            c.young_mut()
                .heap_mut()
                .cons(make_small(500 + i), NIL)
                .expect("fresh cell"),
        );
    }

    let major = c.collect_major(&mut roots).expect("the fullsweep keeps every live object");
    assert_eq!(major.objects_copied, 2, "the two fresh cells were promoted");

    // The live run is pinned; the dropped run's blocks are reclaimed.
    assert_eq!(
        roots[0].raw(),
        narrow_addr.raw(),
        "the live run keeps its address"
    );
    assert_multi_block(c.mature(), roots[0], LARGE_ARITY);
    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(immix.objects_pinned, 1, "only the live run is pinned");
    assert_eq!(immix.lines_pinned, lines_for(LARGE_WORDS));
    assert_eq!(
        immix.blocks_reclaimed, 3,
        "the three whole blocks the dropped run owned were reclaimed"
    );

    // Retained footprint: the live run + the six cells + the two fresh cells, all one line
    // each except the 17-line run. The dropped 33 lines are gone.
    let expected_lines = lines_for(LARGE_WORDS) + SMALL_LIVE + 2;
    assert_eq!(expected_lines, 25);
    assert_eq!(
        c.mature().used_lines(),
        expected_lines,
        "the dropped run's 33 lines were reclaimed, the fresh cells added 2"
    );
    assert!(c.mature().used_lines() < lines_after_minor);

    // The dropped object is unreachable, so its lines came back and now serve the live cells
    // (`blocks_reclaimed == 3` above) — an address-range test cannot show "unreachable", so the
    // proof is the accounting: the retained line count dropped by exactly the run's lines.
    assert!(!roots.iter().any(|r| r.raw() == dropped.raw()));
    for (i, &cell) in roots[1..1 + SMALL_LIVE].iter().enumerate() {
        assert_small_cell(c.mature(), cell, i as i64);
    }
    assert_small_cell(c.mature(), roots[1 + SMALL_LIVE], 500);
    assert_small_cell(c.mature(), roots[2 + SMALL_LIVE], 501);

    eprintln!(
        "mixed_large: lines after minor={lines_after_minor} after major={} pinned={} \
         reclaimed_blocks={} reclaimed_words={}",
        c.mature().used_lines(),
        immix.objects_pinned,
        immix.blocks_reclaimed,
        immix.words_reclaimed
    );
}

// ---------------------------------------------------------------------------
// 2: fragmentation / hole reuse over repeated collections
// ---------------------------------------------------------------------------

/// Interleaved live/dead patterns rebuilt round after round: each round reclaims every
/// non-live line and then re-fills the **whole** space, so `free_lines() == 0` after the fill
/// is the tight proof that the freed lines were handed back out. The occupancy view stays
/// consistent and the survivors are never corrupted.
#[test]
fn repeated_collections_reuse_freed_lines() {
    // Four blocks = 64 lines. The persistent "anchor" set is 8 one-line objects; each round
    // adds 8 live transients and 48 dead ones, i.e. 56 objects — exactly the 56 lines a
    // reclaim frees.
    let mut s = ImmixSpace::new(4 * BLOCK_WORDS).expect("immix space");
    assert_eq!(s.line_count(), 64);

    let anchors = 8usize;
    let transients = 8usize;
    let dead = 64 - anchors - transients;
    assert_eq!(anchors + transients + dead, 64);

    let mut anchor_roots: Vec<Eterm> = (0..anchors as i64)
        .map(|i| {
            s.tuple(&[make_small(i), make_small(100 + i)])
                .expect("anchor")
        })
        .collect();
    let mut promoted: [Eterm; 0] = [];

    for round in 0..6usize {
        // (1) Reclaim: only the anchors are live, so every transient/dead line from the
        //     previous round is freed. `used_lines` settles to exactly the anchor count.
        s.collect(&mut anchor_roots, &mut promoted).expect("reclaim");
        assert_eq!(
            s.used_lines(),
            anchors,
            "round {round}: only the anchors survive the reclaim"
        );
        assert_eq!(s.free_lines(), 64 - anchors);
        assert_eq!(s.used_lines() + s.free_lines(), s.line_count());

        // (2) Fill: the live transients plus the dead objects occupy every free line.
        let mut transient_roots: Vec<Eterm> = (0..transients as i64)
            .map(|i| {
                s.tuple(&[make_small(200 + round as i64 * 10 + i)])
                    .expect("transient")
            })
            .collect();
        for i in 0..dead as i64 {
            let _ = s
                .tuple(&[make_small(round as i64 * 100 + i)])
                .expect("dead object");
        }
        assert_eq!(
            s.free_lines(),
            0,
            "round {round}: every line the reclaim freed was handed out again"
        );
        assert_eq!(s.used_lines(), 64);
        assert_eq!(s.counters().lines_free, 0);

        // (3) Steady state: anchors + this round's transients are the live set.
        let mut live: Vec<Eterm> = anchor_roots.clone();
        live.extend(transient_roots.iter().copied());
        s.collect(&mut live, &mut promoted).expect("steady state");
        assert_eq!(
            s.used_lines(),
            anchors + transients,
            "round {round}: only the live objects keep lines"
        );
        assert_eq!(s.free_lines(), 64 - anchors - transients);

        // The occupancy view and the survivors.
        assert_eq!(s.used_lines() + s.free_lines(), s.line_count());
        let counters = s.counters();
        assert_eq!(counters.lines_free, s.free_lines());
        assert_eq!(counters.lines_allocated(), s.used_lines());
        assert_eq!(counters.capacity_words, 64 * LINE_WORDS);

        // Adopt the relocated addresses and check every survivor's contents.
        anchor_roots = live[..anchors].to_vec();
        transient_roots = live[anchors..].to_vec();
        for (i, &a) in anchor_roots.iter().enumerate() {
            assert_eq!(
                tuple_to_smalls(a),
                vec![i as i64, 100 + i as i64],
                "round {round}: anchor {i} kept its contents"
            );
        }
        for (i, &t) in transient_roots.iter().enumerate() {
            assert_eq!(
                tuple_to_smalls(t),
                vec![200 + round as i64 * 10 + i as i64],
                "round {round}: transient {i} kept its contents"
            );
        }

        // The space is immediately refillable from whatever the collection left free.
        eprintln!(
            "reuse round {round}: used={} free={} partial_blocks={} free_blocks={}",
            s.used_lines(),
            s.free_lines(),
            counters.blocks_partial,
            counters.blocks_free
        );
    }

    // A final, empty-collection sweep leaves the space reusable from scratch.
    anchor_roots.clear();
    s.collect(&mut anchor_roots, &mut promoted).expect("final sweep");
    assert_eq!(s.used_lines(), 0, "an empty live set retains nothing");
    assert_eq!(s.free_lines(), s.line_count());
}

// ---------------------------------------------------------------------------
// 3: repeated minors with mature promotion
// ---------------------------------------------------------------------------

/// A policy whose minimum equals the mature capacity, so `mature_after_fullsweep` always
/// returns the current capacity (no relocation) and an explicit `collect_major` runs the
/// in-place mark-region reclaim.
fn minor_stress_policy() -> HeapPolicy {
    HeapPolicy {
        initial_young_words: 4096,
        initial_mature_words: 8 * BLOCK_WORDS,
        min_heap_words: 8 * BLOCK_WORDS,
        fullsweep_after: 65535,
        ..HeapPolicy::default()
    }
}

/// Builds `{atom, [1,2,3], {4,5}}` — five objects — in `heap`; returns the outer term.
fn build_small_graph(heap: &mut Heap) -> Eterm {
    let atom = make_atom(Atom::from_raw(7));
    let cell3 = heap.cons(make_small(3), NIL).expect("cons cell");
    let cell2 = heap.cons(make_small(2), cell3).expect("cons cell");
    let list = heap.cons(make_small(1), cell2).expect("cons cell");
    let inner = heap
        .tuple(&[make_small(4), make_small(5)])
        .expect("inner tuple");
    heap.tuple(&[atom, list, inner]).expect("outer tuple")
}

/// The lines `build_small_graph`'s five objects occupy (all one line each).
const SMALL_GRAPH_LINES: usize = 5;

/// A promoted term survives several further **minors** unchanged (a minor never touches the
/// mature space), the mature garbage it becomes when dropped is *not* reclaimed by a minor
/// either — only a **major** reclaims it — and a promoted multi-block run keeps its address
/// until that major pins it.
#[test]
fn a_minor_promotion_survives_repeated_minors_until_a_major_reclaims_it() {
    let table = AtomTable::new();
    let mut baseline_heap = Heap::new(64).expect("baseline heap");
    let baseline = build_small_graph(&mut baseline_heap);

    let mut c = GenImmixCollector::from_policy(minor_stress_policy()).expect("collector");

    // Round 0: build the graph in the nursery and promote it with a minor.
    let mut roots = vec![build_small_graph(c.young_mut().heap_mut())];
    let minor = c.collect_minor(&mut roots).expect("promote the graph");
    assert_eq!(minor.objects_copied, SMALL_GRAPH_LINES);
    let promoted = roots[0];
    assert!(c.mature().contains_term(promoted));
    assert_eq!(c.mature().used_lines(), SMALL_GRAPH_LINES);
    assert!(
        compare::eq(roots[0], baseline, &table),
        "the promoted graph matches its baseline"
    );

    // Round 1: drop it — the promoted graph is now mature garbage — and run a minor. A minor
    // never rescans the mature space, so the garbage keeps its lines.
    roots.clear();
    for i in 0..4i64 {
        c.young_mut()
            .heap_mut()
            .cons(make_small(700 + i), NIL)
            .expect("nursery garbage");
    }
    let minor = c.collect_minor(&mut roots).expect("minor with an empty live set");
    assert_eq!(minor.objects_copied, 0, "nothing was live in the nursery");
    assert_eq!(
        c.mature().used_lines(),
        SMALL_GRAPH_LINES,
        "a minor never reclaims mature garbage"
    );

    // Round 2: promote a multi-block run and keep it live.
    let large = build_tuple(&mut c, LARGE_ARITY);
    roots.push(large);
    c.collect_minor(&mut roots).expect("promote the run");
    let large_mature = roots[0];
    assert_multi_block(c.mature(), large_mature, LARGE_ARITY);
    let lines_with_run = SMALL_GRAPH_LINES + lines_for(LARGE_WORDS);
    assert_eq!(c.mature().used_lines(), lines_with_run);

    // Rounds 3..=5: further minors, each promoting one fresh one-line cell. The mature run's
    // address is stable — a minor never moves a mature object — and the mature garbage from
    // round 0 is still retained.
    for round in 3..=5usize {
        let cell = c
            .young_mut()
            .heap_mut()
            .cons(make_small(round as i64), NIL)
            .expect("fresh cell");
        roots.push(cell);
        for i in 0..3i64 {
            c.young_mut()
                .heap_mut()
                .cons(make_small(800 + i), NIL)
                .expect("nursery garbage");
        }
        c.collect_minor(&mut roots).expect("minor");
        assert_eq!(
            roots[0].raw(),
            large_mature.raw(),
            "round {round}: a minor never moves a mature object"
        );
        assert_multi_block(c.mature(), roots[0], LARGE_ARITY);
        assert_eq!(
            c.mature().used_lines(),
            lines_with_run + (round - 2),
            "round {round}: only the fresh cells added lines; the round-0 garbage is retained"
        );
    }

    // The fullsweep now reclaims everything unreachable: the round-0 graph (dropped in
    // round 1) and the nursery garbage; the run and the three cells survive.
    let lines_before_major = c.mature().used_lines();
    let major = c.collect_major(&mut roots).expect("the fullsweep keeps the live set");
    assert_eq!(major.objects_copied, 0, "the nursery was empty of live terms");
    assert_eq!(
        roots[0].raw(),
        large_mature.raw(),
        "the run is pinned by the major"
    );
    assert_multi_block(c.mature(), roots[0], LARGE_ARITY);
    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(immix.objects_pinned, 1, "only the multi-block run is pinned");
    assert_eq!(immix.lines_pinned, lines_for(LARGE_WORDS));
    assert_eq!(immix.objects_evacuated, 3, "the three fresh cells moved");
    assert_eq!(
        c.mature().used_lines(),
        lines_before_major - SMALL_GRAPH_LINES,
        "the major reclaimed exactly the dropped graph's lines"
    );

    eprintln!(
        "repeated_minors: lines before major={lines_before_major} after={} pinned={} \
         washed_lines={}",
        c.mature().used_lines(),
        immix.objects_pinned,
        immix.words_reclaimed
    );
}

// ---------------------------------------------------------------------------
// 4: mixed workload across both strategies
// ---------------------------------------------------------------------------

/// Rounds of "add a subgraph, then collect". With `fullsweep_after = 3` the kind sequence is
/// Minor, Minor, Minor, Major, Minor, Minor, Minor, Major.
const MIXED_ROUNDS: usize = 8;

/// Keys of the HAMT map (`> MAP_SMALL_MAP_LIMIT`), so the hash-map interior is exercised.
const HAMT_KEYS: usize = 40;

/// The on-heap binary payload and the non-byte-aligned bitstring.
const BIN_BYTES: &[u8] = b"immix stress heap binary payload";
const BITS_LEN: usize = 13;
const BITS_BYTES: &[u8] = &[0b1010_1100, 0b0101_1000];

/// A policy for both strategies. Identical sizes mean neither path is capacity-tight, so the
/// collection kinds are driven purely by the fullsweep cadence, and `min_heap_words` equals
/// the mature capacity so an Immix fullsweep reclaims in place rather than relocating.
fn mixed_policy() -> HeapPolicy {
    HeapPolicy {
        fullsweep_after: 3,
        initial_young_words: 4096,
        initial_mature_words: 4096,
        min_heap_words: 4096,
        ..HeapPolicy::default()
    }
}

/// Reserves [`map_words_capacity`]`(n)` words and writes the `i -> 3*i` map there.
fn write_map_n(heap: &mut Heap, atoms: &AtomTable, n: usize) -> Eterm {
    let cap = map_words_capacity(n);
    let input: Vec<(Eterm, Eterm)> = (0..n as i64)
        .map(|i| (make_small(i), make_small(i * 3)))
        .collect();
    // SAFETY: `cap` exclusively-owned, writable words are reserved; `write_map` writes a
    // contiguous prefix of them with well-formed terms and returns a root into that prefix.
    unsafe {
        let hp = heap.alloc_words(cap).expect("reserve the map block");
        write_map(hp, &input, atoms).expect("build the map")
    }
}

/// Writes an on-heap bitstring of `n_bits` bits drawn from `bytes`.
fn write_bits(heap: &mut Heap, bytes: &[u8], n_bits: usize) -> Eterm {
    let words = heap_bits_size_words(n_bits);
    // SAFETY: `words` exclusively-owned, writable words are reserved and all of them are
    // written by `write_heap_bits`.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve heap bits");
        write_heap_bits(hp, bytes, n_bits).expect("write heap bits")
    }
}

/// Writes a `Local` fun with `env` free variables.
fn write_local(heap: &mut Heap, arity: usize, uniq: u64, index: u32, env: &[Eterm]) -> Eterm {
    let words = fun_words(FunKind::Local, env.len());
    // SAFETY: `words` exclusively-owned, writable words are reserved and all of them are
    // written by `write_fun_local`; every `env` entry is a live term of this heap.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve local fun");
        write_fun_local(hp, arity, uniq, index, env).expect("write local fun")
    }
}

/// Writes an `External` fun (`[header][module][function][arity]`).
fn write_external(heap: &mut Heap, arity: usize, module: Atom, function: Atom) -> Eterm {
    let words = fun_words(FunKind::External, 0);
    // SAFETY: the 4 words `write_fun_external` writes are exclusively owned and reserved.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve external fun");
        write_fun_external(hp, arity, module, function).expect("write external fun")
    }
}

/// Writes an `Imported` fun (the whole `NEW_FUN_EXT` descriptor).
fn write_imported(heap: &mut Heap, arity: usize, module: Atom, creator: FunPid, env: &[Eterm]) -> Eterm {
    let words = fun_words(FunKind::Imported, env.len());
    let uniq = [
        0x11u8, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF,
        0x00,
    ];
    // SAFETY: `words` exclusively-owned, writable words are reserved and all of them are
    // written by `write_fun_imported`; every `env` entry is a live term of this heap.
    unsafe {
        let hp = heap.alloc_words(words).expect("reserve imported fun");
        write_fun_imported(hp, arity, module, &uniq, 21, 0, 0, creator, env)
            .expect("write imported fun")
    }
}

/// Builds one round's subgraph in `heap`: round 0 is the full mixed corpus (cons cells,
/// nested tuples, a HAMT map, an on-heap binary and bitstring and all three fun kinds);
/// later rounds add a light tuple/list/local-fun graph plus deliberate garbage.
fn build_mixed(heap: &mut Heap, atoms: &AtomTable, round: usize) -> Vec<Eterm> {
    if round == 0 {
        let mut roots: Vec<Eterm> = Vec::new();

        // (a) nested tuples + a cons list + a boxed leaf.
        roots.push(build_small_graph(heap));

        // (b) a shared subterm and (c) a HAMT map.
        let shared = heap.tuple(&[make_small(41), make_small(42)]).expect("shared");
        roots.push(heap.tuple(&[shared, make_small(10)]).expect("tupled shared"));
        roots.push(write_map_n(heap, atoms, HAMT_KEYS));

        // (d) an on-heap binary and a non-byte-aligned bitstring.
        roots.push(write_bits(heap, BIN_BYTES, BIN_BYTES.len() * 8));
        roots.push(write_bits(heap, BITS_BYTES, BITS_LEN));

        // (e) all three fun kinds, with compound free variables.
        let module = Atom::from_raw(1);
        let function = Atom::from_raw(2);
        let free_tuple = heap.tuple(&[make_small(1), make_small(2)]).expect("free tuple");
        let free_cons = heap.cons(make_small(3), NIL).expect("free cons");
        roots.push(write_local(
            heap,
            3,
            0xDEAD_BEEF,
            11,
            &[free_tuple, free_cons, make_small(5)],
        ));
        roots.push(write_external(heap, 2, module, function));
        let imported_env = heap
            .tuple(&[make_small(6), make_small(7), make_small(8)])
            .expect("imported env");
        let creator = FunPid {
            node: Atom::from_raw(3),
            number: 4,
            serial: 5,
            creation: 6,
        };
        roots.push(write_imported(heap, 1, module, creator, &[imported_env]));

        // deliberate garbage that no root reaches.
        for i in 0..6i64 {
            let _ = heap.tuple(&[make_small(900 + i)]).expect("garbage");
        }
        return roots;
    }

    let r = round as i64;
    let t = heap.tuple(&[make_small(r), make_small(100 + r)]).expect("round tuple");
    let list = heap.cons(t, NIL).expect("round list");
    let f = write_local(heap, 1, round as u64, round as u32, &[t, list]);
    let _ = heap.tuple(&[make_small(500 + r)]).expect("garbage");
    vec![t, list, f]
}

/// One strategy's observable result.
struct MixedRun {
    kinds: Vec<CollectionKind>,
    roots: Vec<Eterm>,
    /// The collector **must outlive the roots**: every `roots` entry is an address inside this
    /// collector's mature space, so dropping it before the roots are compared would dangle them
    /// (and the next strategy's allocation would silently reuse the freed region). Kept here so
    /// both runs stay live at once.
    _collector: AnyCollector,
}

/// Builds the mixed corpus round by round against `kind`'s collector and collects after each
/// round, asserting every root stays structurally equal to its baseline copy.
fn run_mixed(kind: MatureKind, atoms: &AtomTable, baseline: &[Eterm]) -> MixedRun {
    let config = GcConfig::copying(mixed_policy()).with_mature(kind);
    let mut c = AnyCollector::from_config(&config).expect("collector");
    assert_eq!(c.kind(), kind, "the config selected the requested strategy");

    let mut roots: Vec<Eterm> = Vec::new();
    let mut kinds: Vec<CollectionKind> = Vec::new();

    for round in 0..MIXED_ROUNDS {
        let fresh = build_mixed(c.young_mut().heap_mut(), atoms, round);
        roots.extend(fresh);

        let collection = c.collect(&mut roots).expect("collect");
        kinds.push(collection.kind);

        assert_eq!(c.young().used_words(), 0, "{kind:?}: round {round}: nursery emptied");
        // Every root must be relocated into the mature generation and structurally unchanged.
        for (i, root) in roots.iter().enumerate() {
            assert!(
                !c.young().contains_term(*root),
                "{kind:?}: round {round}: root {i} stayed in the nursery"
            );
            assert!(
                compare::eq(*root, baseline[i], atoms),
                "{kind:?}: round {round}: root {i} drifted from its baseline"
            );
        }
    }

    MixedRun {
        kinds,
        roots,
        _collector: c,
    }
}

/// The mixed corpus survives an identical minor/major sequence under **both** strategies, and
/// the reachable graph is structurally equal under the two (`compare::eq`).
#[test]
fn mixed_workload_agrees_across_both_strategies() {
    let atoms = AtomTable::new();

    // The persistent, never-collected baseline (a private heap).
    let mut baseline_heap = Heap::new(8192).expect("baseline heap");
    let mut baseline: Vec<Eterm> = Vec::new();
    for round in 0..MIXED_ROUNDS {
        baseline.extend(build_mixed(&mut baseline_heap, &atoms, round));
    }

    let copying = run_mixed(MatureKind::Copying, &atoms, &baseline);
    let immix = run_mixed(MatureKind::Immix, &atoms, &baseline);

    // Both strategies pick the same collections (Minor,Minor,Minor,Major,...), so the
    // comparison below is over identical collection points.
    let expected: Vec<CollectionKind> = (0..MIXED_ROUNDS)
        .map(|r| {
            if r % 4 == 3 {
                CollectionKind::Major
            } else {
                CollectionKind::Minor
            }
        })
        .collect();
    assert_eq!(copying.kinds, expected, "copying collection kinds");
    assert_eq!(immix.kinds, expected, "immix collection kinds");
    assert_eq!(copying.kinds, immix.kinds);

    // The reachable graph is structurally equal under both strategies, root by root.
    assert_eq!(copying.roots.len(), immix.roots.len());
    for (i, (a, b)) in copying.roots.iter().zip(immix.roots.iter()).enumerate() {
        assert!(
            compare::eq(*a, *b, &atoms),
            "root {i} must be structurally equal under both strategies"
        );
    }

    // A positional spot-check that the compare above is not trivially true: the round-0 graph's
    // cons list `[1, 2, 3]` (its second element) survived intact under **both** strategies.
    for run in [&copying, &immix] {
        let list = tuple_elem(run.roots[0], 1);
        assert_eq!(list_to_smalls(list), vec![1, 2, 3]);
    }

    eprintln!(
        "mixed_workload: roots={} kinds={:?}",
        copying.roots.len(),
        copying.kinds
    );
}

// ---------------------------------------------------------------------------
// 5: memory-accounting assertions
// ---------------------------------------------------------------------------

/// The retained footprint is bounded by the live set — never below it and never more than the
/// documented per-object line-rounding slack above it — and dropping live objects and
/// collecting again reduces it by exactly the dropped objects' line count.
#[test]
fn retained_footprint_tracks_the_live_set() {
    // Four blocks = 64 lines. Every fixture is a one-line object, so the line-rounded live
    // size is exact and the footprint assertions are tight.
    let mut s = ImmixSpace::new(4 * BLOCK_WORDS).expect("immix space");
    const TWO_ELEM_WORDS: usize = 3;
    assert_eq!(lines_for(TWO_ELEM_WORDS), 1, "a 2-element tuple is one line");
    assert_eq!(lines_for(2), 1, "a 1-element tuple is one line");

    let live = 40usize;
    let mut roots: Vec<Eterm> = (0..live as i64)
        .map(|i| {
            s.tuple(&[make_small(i), make_small(i + 1000)])
                .expect("live object")
        })
        .collect();
    for i in 0..20i64 {
        let _ = s.tuple(&[make_small(5000 + i)]).expect("dead object");
    }
    assert_eq!(s.used_lines(), live + 20, "everything is allocated before the collection");
    let mut promoted: [Eterm; 0] = [];

    s.collect(&mut roots, &mut promoted).expect("collect");

    // Exact accounting: one line per live object, no garbage retained.
    assert_eq!(s.used_lines(), live, "the footprint is exactly the live line count");
    let retained = s.used_words();
    let live_words = live * TWO_ELEM_WORDS;
    assert_eq!(retained, live * LINE_WORDS);

    // (1) The footprint is never below the live set.
    assert!(retained >= live_words, "{retained} < live {live_words}");
    // (2) ... and never more than the documented per-object slack (`LINE_WORDS - 1` words).
    assert!(
        retained - live_words <= live * (LINE_WORDS - 1),
        "line-rounding slack exceeded: {retained} - {live_words}"
    );
    // ... which the exact one-line fixtures make tight: the whole gap is line rounding.
    assert_eq!(retained - live_words, live * (LINE_WORDS - TWO_ELEM_WORDS));
    // (3) The space stays within its capacity.
    assert!(retained <= s.capacity_words());
    assert_eq!(s.used_lines() + s.free_lines(), s.line_count());

    // Churn: drop half the live objects, add six fresh ones, plus dead garbage. The retained
    // footprint follows the live set.
    roots.truncate(live / 2);
    for i in 0..6i64 {
        roots.push(s.tuple(&[make_small(9000 + i)]).expect("fresh live"));
    }
    for i in 0..10i64 {
        let _ = s.tuple(&[make_small(7000 + i)]).expect("churn dead");
    }
    s.collect(&mut roots, &mut promoted).expect("collect");
    assert_eq!(
        s.used_lines(),
        live / 2 + 6,
        "the retained footprint tracks the live set after the churn"
    );
    assert!(s.used_words() <= s.capacity_words());

    // Dropping live objects reduces the footprint, collection for collection.
    let before = s.used_lines();
    roots.truncate(8);
    s.collect(&mut roots, &mut promoted).expect("collect");
    assert_eq!(s.used_lines(), 8, "only the eight remaining live objects keep lines");
    assert!(s.used_lines() < before);

    // The surviving objects are intact (indices 0..8 of the original live set).
    for (i, &root) in roots.iter().enumerate() {
        assert_eq!(
            tuple_to_smalls(root),
            vec![i as i64, i as i64 + 1000],
            "survivor {i} kept its contents"
        );
    }

    // An empty live set retains nothing at all.
    roots.clear();
    s.collect(&mut roots, &mut promoted).expect("collect");
    assert_eq!(s.used_lines(), 0);
    assert_eq!(s.free_lines(), s.line_count());

    eprintln!(
        "footprint: live_words={live_words} retained_words={retained} slack={} lines_per_object=1",
        retained - live_words
    );
}
