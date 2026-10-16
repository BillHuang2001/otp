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

//! Adversarial **invariant probes** for the GenImmix evacuation/copy path.
//!
//! `tests/immix_partial_block.rs` pins the *reclamation* facts (what moves, what is pinned);
//! `tests/immix_parity.rs` pins structural agreement with the copying strategy. This file
//! attacks the two seams where a wrong *ordering* or a stale bitmap would silently corrupt a
//! live heap instead of failing loudly:
//!
//! 1. **No dangling interior pointer after a major.** A major
//!    ([`GenImmixCollector::collect_major`]) reclaims the mature generation *in place first*,
//!    then rewrites the nursery's mature references to the reclaim's final addresses, and only
//!    *then* promotes the nursery. A nursery object whose element names a mature object must
//!    end up naming that object's **final** address. Four cases are probed against the public
//!    API — an evacuated target, two nursery slots sharing one target, a deeply nested
//!    reference, and a pinned (address-unchanged) target — each of which a promote-before-
//!    rewrite ordering would turn into a stale pointer.
//! 2. **Line-bitmap accounting consistency across collections.** After a collection the
//!    `marked`/`kept` bitmaps are reset, `allocated` is exactly the surviving (kept) lines,
//!    `used_lines()` is the allocated popcount, the block classification agrees with the
//!    allocated bitmap, and a freed line is *really* reusable (filling every hole leaves every
//!    survivor byte-intact).
//! 3. **No double-free / no spurious release.** A pinned `AllMarked` object is never also
//!    released, `blocks_reclaimed` never counts a block that still holds a pinned object,
//!    `words_reclaimed` is the net reduction in occupied lines, and a repeated collection over
//!    an unchanged live set is idempotent with the pinned whole-block objects at byte-identical
//!    addresses.
//!
//! `unsafe` below only dereferences pointers this test allocated itself (through the space's /
//! collector's own builders) or that a collection just wrote; every block carries a
//! `// SAFETY:` rationale.

use beam_gc::prelude::{
    lines_for, GenImmixCollector, HeapPolicy, ImmixSpace, ImmixStats, BLOCK_WORDS, LINES_PER_BLOCK,
    LINE_WORDS,
};
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, is_list, is_small, list_val, make_small, signed_value,
    Eterm, NIL,
};

// ---------------------------------------------------------------------------
// Shared fixtures
// ---------------------------------------------------------------------------

/// A tuple of exactly one block's worth of words: [`BLOCK_WORDS`] elements occupy
/// `BLOCK_WORDS + 1` words, so subtracting the header gives a 16-line object — one whole
/// [`LINES_PER_BLOCK`]-line block, which is the shape that becomes `AllMarked` (pinned).
const FULL_BLOCK_ARITY: usize = BLOCK_WORDS - 1;

/// The words the [`FULL_BLOCK_ARITY`]-element tuple occupies (its header plus its elements).
const FULL_BLOCK_WORDS: usize = BLOCK_WORDS;

/// A one-element tuple `{v}` allocated in the collector's nursery.
fn young_small_tuple(c: &mut GenImmixCollector, v: i64) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(&[make_small(v)])
        .expect("a one-line tuple fits the nursery")
}

/// The small integer held by the one-element tuple `term`.
fn small_tuple_value(term: Eterm) -> i64 {
    // SAFETY: `term` is a live one-element tuple this test (or a collection) built.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "not a tuple header");
    assert_eq!(arityval(header), 1);
    // SAFETY: element 0 follows the validated header.
    signed_value(unsafe { *boxed_val(term).add(1) })
}

/// Asserts `term` is the [`FULL_BLOCK_ARITY`]-element tuple whose element `i` is `base + i`.
fn assert_full_block_tuple(term: Eterm, base: i64) {
    // SAFETY: `term` is a live tuple this test (or a collection) built.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "not a tuple header");
    assert_eq!(
        arityval(header),
        FULL_BLOCK_ARITY,
        "the tuple fills a block"
    );
    for i in 0..FULL_BLOCK_ARITY {
        // SAFETY: elements 0..arity follow the validated header of a live tuple.
        let e = unsafe { *boxed_val(term).add(1 + i) };
        assert!(is_small(e), "element {i} is an immediate");
        assert_eq!(signed_value(e), base + i as i64, "element {i} survives");
    }
}

/// The element list of a [`FULL_BLOCK_ARITY`]-element tuple with base `base`.
fn full_block_elems(base: i64) -> Vec<Eterm> {
    (0..FULL_BLOCK_ARITY)
        .map(|i| make_small(base + i as i64))
        .collect()
}

/// A `GenImmixCollector` with the given initial nursery and mature sizes (the remaining
/// policy fields at their defaults).
fn collector(initial_young_words: usize, initial_mature_words: usize) -> GenImmixCollector {
    let policy = HeapPolicy {
        initial_young_words,
        initial_mature_words,
        ..HeapPolicy::default()
    };
    GenImmixCollector::from_policy(policy).expect("collector")
}

// ---------------------------------------------------------------------------
// 1. No dangling interior pointer after a major
// ---------------------------------------------------------------------------

/// (a) The referenced mature object is **evacuated** out of a `Partial` block, so its address
/// changes and the nursery reference must be rewritten to the new one.
///
/// `m` is also kept as a separate root so the test can read the reclaim's final address
/// independently of the nursery slot: the two must agree. A major that promoted the nursery
/// *before* rewriting its mature references would leave the promoted holder naming `m`'s old
/// (now marker-overwritten) address, and this equality would fail.
#[test]
fn a_nursery_reference_to_an_evacuated_mature_object_follows_it() {
    assert_eq!(lines_for(FULL_BLOCK_WORDS), LINES_PER_BLOCK);
    // One 256-word mature block: `m` occupies one of its sixteen lines.
    let mut c = collector(512, 233);

    // Step 1: get `m` into the mature generation with a minor.
    let m = young_small_tuple(&mut c, 42);
    let mut roots = [m];
    let minor = c.collect_minor(&mut roots).expect("the minor fits");
    assert_eq!(minor.objects_copied, 1);
    let m = roots[0];
    assert!(
        c.mature().contains_term(m),
        "m was promoted into the mature space"
    );
    assert_eq!(
        c.mature().used_lines(),
        1,
        "a Partial block: 1 of 16 lines live"
    );
    assert_eq!(c.mature().counters().blocks_partial, 1);

    // Step 2: a nursery holder whose element names the mature object — the young->mature
    // edge a naive in-place reclaim would leave dangling.
    let holder = c.young_mut().heap_mut().tuple(&[m]).expect("holder");

    // Step 3: a major, with `m` also a root so its final address is observable.
    let mut roots = [holder, m];
    c.collect_major(&mut roots).expect("the major fits");

    let (promoted, m_after) = (roots[0], roots[1]);
    assert!(
        c.mature().contains_term(promoted),
        "the holder was promoted"
    );
    assert!(
        c.mature().contains_term(m_after),
        "the mature target survived"
    );
    assert_ne!(
        m_after.raw(),
        m.raw(),
        "the target was evacuated out of its Partial block (its address changed)"
    );

    // The holder's element must name the target's FINAL address.
    // SAFETY: `promoted` is a live one-element tuple in the mature space.
    let element = unsafe { *boxed_val(promoted).add(1) };
    assert_eq!(
        element, m_after,
        "the nursery slot was rewritten to the mature object's final address"
    );
    assert_eq!(
        small_tuple_value(element),
        42,
        "the target kept its content"
    );

    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(
        immix.objects_evacuated, 1,
        "the target was the one evacuated object"
    );
    assert_eq!(c.mature().counters().lines_marked, 0, "marked is reset");
}

/// (b) **Two/three nursery slots share one mature object** — every slot (and every holder)
/// must be rewritten to the same final address, and the shared target must still be copied
/// (evacuated) exactly once.
#[test]
fn several_nursery_slots_referencing_one_mature_object_are_all_rewritten() {
    let mut c = collector(512, 233);

    let m = young_small_tuple(&mut c, 7);
    let mut roots = [m];
    c.collect_minor(&mut roots).expect("minor");
    let m = roots[0];
    assert!(c.mature().contains_term(m));

    // Three holders naming `m`: `pair` twice, `single` once, and a **cons cell** (the
    // header-less LIST shape, whose reference words are walked differently) in its car —
    // four nursery reference words resolving to the same mature object.
    let heap = c.young_mut().heap_mut();
    let pair = heap.tuple(&[m, m]).expect("pair");
    let single = heap.tuple(&[m]).expect("single");
    let cell = heap.cons(m, NIL).expect("cell");

    let mut roots = [pair, single, cell, m];
    c.collect_major(&mut roots).expect("major");

    let (pair_after, single_after, cell_after, m_after) = (roots[0], roots[1], roots[2], roots[3]);
    assert!(c.mature().contains_term(pair_after));
    assert!(c.mature().contains_term(single_after));
    assert!(c.mature().contains_term(cell_after));
    assert_ne!(
        m_after.raw(),
        m.raw(),
        "the shared target was evacuated once"
    );

    // SAFETY: `pair_after` is a live two-element tuple in the mature space.
    let (e0, e1) = unsafe { (*boxed_val(pair_after).add(1), *boxed_val(pair_after).add(2)) };
    assert_eq!(e0, m_after, "the pair's first slot names the final address");
    assert_eq!(
        e1, m_after,
        "the pair's second slot names the same final address"
    );
    // SAFETY: `single_after` is a live one-element tuple in the mature space.
    let e2 = unsafe { *boxed_val(single_after).add(1) };
    assert_eq!(
        e2, m_after,
        "the second holder's slot names the same final address"
    );
    // SAFETY: `cell_after` is a live cons cell in the mature space.
    assert!(is_list(cell_after), "the cons holder is LIST-tagged");
    let car = unsafe { *list_val(cell_after) };
    assert_eq!(
        car, m_after,
        "the cons cell's car was rewritten to the same final address"
    );
    // SAFETY: the second word of the read cons cell is its cdr.
    assert_eq!(unsafe { *list_val(cell_after).add(1) }, NIL);

    // The shared target was evacuated exactly once, not once per referencing slot.
    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(
        immix.objects_evacuated, 1,
        "copied once, shared by every slot"
    );
    assert_eq!(
        small_tuple_value(m_after),
        7,
        "the shared target kept its content"
    );
}

/// (c) The reference is **nested several levels deep** in the nursery graph: only the deepest
/// holder names the mature object, so the rewrite must reach the interior slot of a nursery
/// object that is itself reached through two more hops.
#[test]
fn a_deeply_nested_nursery_reference_follows_the_evacuation() {
    let mut c = collector(512, 233);

    let m = young_small_tuple(&mut c, 99);
    let mut roots = [m];
    c.collect_minor(&mut roots).expect("minor");
    let m = roots[0];
    assert!(c.mature().contains_term(m));

    // w1 -> m lives deepest; w2 -> w1; w3 -> w2 is the root.
    let heap = c.young_mut().heap_mut();
    let w1 = heap.tuple(&[m]).expect("w1");
    let w2 = heap.tuple(&[w1]).expect("w2");
    let w3 = heap.tuple(&[w2]).expect("w3");

    let mut roots = [w3, m];
    c.collect_major(&mut roots).expect("major");

    let (w3_after, m_after) = (roots[0], roots[1]);
    assert!(c.mature().contains_term(w3_after));
    assert_ne!(m_after.raw(), m.raw(), "the deeply referenced target moved");

    // <root>[0][0][0] must resolve to the target's final address.
    // SAFETY: `w3_after` is a live one-element tuple; so are the tuples it chains to.
    let w2_after = unsafe { *boxed_val(w3_after).add(1) };
    assert!(c.mature().contains_term(w2_after), "w2 was promoted");
    // SAFETY: `w2_after` is the live one-element tuple just identified.
    let w1_after = unsafe { *boxed_val(w2_after).add(1) };
    assert!(c.mature().contains_term(w1_after), "w1 was promoted");
    // SAFETY: `w1_after` is the live one-element tuple just identified.
    let leaf = unsafe { *boxed_val(w1_after).add(1) };
    assert_eq!(
        leaf, m_after,
        "the deepest nursery slot was rewritten to the target's final address"
    );
    assert_eq!(small_tuple_value(leaf), 99);
}

/// (d) The referenced mature object is **pinned** (`AllMarked` block): its address does not
/// change, and the nursery reference is left naming that unchanged address — no second copy,
/// no spurious exhaustion.
#[test]
fn a_nursery_reference_to_a_pinned_mature_object_is_left_verbatim() {
    // A two-block mature region (512 words); the fixture fills the first block entirely, so
    // its marks cover every line and it classifies `AllMarked` — the full-block pin rule.
    let mut c = collector(1024, 2 * BLOCK_WORDS);

    let elems = full_block_elems(0);
    let big = c
        .young_mut()
        .heap_mut()
        .tuple(&elems)
        .expect("the fixture fits the nursery");
    assert_eq!(
        c.young().used_words(),
        FULL_BLOCK_WORDS,
        "only the fixture is live"
    );

    let mut roots = [big];
    let minor = c.collect_minor(&mut roots).expect("the block run fits");
    assert_eq!(minor.objects_copied, 1);
    let big = roots[0];
    assert!(c.mature().contains_term(big));
    assert_eq!(
        c.mature().used_lines(),
        LINES_PER_BLOCK,
        "the whole block is live"
    );

    // A nursery holder naming the pinned mature object.
    let holder = c.young_mut().heap_mut().tuple(&[big]).expect("holder");

    let mut roots = [holder, big];
    c.collect_major(&mut roots)
        .expect("the major keeps the fixture");

    let (holder_after, big_after) = (roots[0], roots[1]);
    assert!(
        c.mature().contains_term(holder_after),
        "the holder was promoted"
    );
    assert_eq!(
        big_after.raw(),
        big.raw(),
        "a pinned AllMarked object keeps its address"
    );

    let immix = c.last_immix_stats().expect("the in-place reclaim ran");
    assert_eq!(
        immix.objects_pinned, 1,
        "the fixture is pinned, not evacuated"
    );
    assert_eq!(immix.objects_evacuated, 0);
    assert_eq!(c.mature().counters().blocks_all_marked, 1);

    // SAFETY: `holder_after` is a live one-element tuple in the mature space.
    let element = unsafe { *boxed_val(holder_after).add(1) };
    assert_eq!(
        element, big_after,
        "the reference to the pinned object resolves to its unchanged address"
    );
    assert_full_block_tuple(element, 0);
}

/// (e) The *other* major path: a **relocating** fullsweep (the policy resizes the mature
/// region) copies the live set — nursery and mature together — into a fresh mark-region
/// space in a single pass. A nursery reference to a mature object must still resolve, in the
/// **new** region, to the target's final address there.
///
/// This is the complement of the in-place case: `relocate_major` never touches the
/// nursery-slot rewrite machinery (the whole graph moves at once), so it must not depend on
/// it either.
#[test]
fn a_nursery_reference_survives_a_relocating_fullsweep() {
    // A large mature region with a tiny live set makes `mature_after_fullsweep` *shrink* it,
    // which is the relocating path rather than the in-place reclaim.
    let mut c = collector(512, 1024);

    let m = young_small_tuple(&mut c, 21);
    let mut roots = [m];
    c.collect_minor(&mut roots).expect("minor");
    let m = roots[0];
    assert!(c.mature().contains_term(m));
    let base_before = c.mature().base() as usize;
    let capacity_before = c.mature().capacity_words();

    let holder = c.young_mut().heap_mut().tuple(&[m]).expect("holder");

    let mut roots = [holder, m];
    c.collect_major(&mut roots)
        .expect("the relocating fullsweep keeps the live set");

    // The mature region was replaced, not reclaimed in place.
    assert_ne!(
        c.mature().base() as usize,
        base_before,
        "the relocating fullsweep built a fresh mature region"
    );
    assert_ne!(
        c.mature().capacity_words(),
        capacity_before,
        "the mature region was resized"
    );
    assert!(
        c.last_immix_stats().is_none(),
        "no in-place reclaim ran, so there is nothing to report"
    );

    let (holder_after, m_after) = (roots[0], roots[1]);
    assert!(
        c.mature().contains_term(holder_after),
        "the holder is in the new region"
    );
    assert!(
        c.mature().contains_term(m_after),
        "the target is in the new region"
    );
    // SAFETY: `holder_after` is a live one-element tuple in the new mature region.
    let element = unsafe { *boxed_val(holder_after).add(1) };
    assert_eq!(
        element, m_after,
        "the nursery reference resolves in the new region, not a stale one"
    );
    assert_eq!(
        small_tuple_value(element),
        21,
        "the target kept its content"
    );
}

// ---------------------------------------------------------------------------
// 2. Line-bitmap accounting consistency across collections
// ---------------------------------------------------------------------------

/// After a collection: `marked`/`kept` are reset, `allocated` is exactly the surviving lines,
/// `used_lines()` is the allocated popcount, and the block classification agrees with the
/// allocated bitmap. Then every hole the collection left is really free — filling them all
/// with fresh objects leaves every survivor byte-intact.
#[test]
fn line_bitmaps_are_consistent_and_freed_lines_are_reusable() {
    let mut s = ImmixSpace::new(3 * BLOCK_WORDS).expect("space");
    assert_eq!(s.line_count(), 3 * LINES_PER_BLOCK);

    // Block 0 (lines 0..16): one whole-block object — the pinned `AllMarked` block.
    let big = s.tuple(&full_block_elems(0)).expect("big");
    // Block 1 (lines 16..32): sixteen one-line tuples, alternating live/dead.
    let mut live: Vec<Eterm> = Vec::new();
    for i in 0..LINES_PER_BLOCK as i64 {
        if i % 2 == 0 {
            live.push(s.tuple(&[make_small(100 + i / 2)]).expect("live tuple"));
        } else {
            let _ = s.tuple(&[make_small(900 + i)]).expect("dead tuple");
        }
    }
    // Block 2 (lines 32..48): sixteen wholly dead tuples.
    for i in 0..LINES_PER_BLOCK as i64 {
        let _ = s.tuple(&[make_small(700 + i)]).expect("garbage tuple");
    }
    assert_eq!(
        s.used_lines(),
        3 * LINES_PER_BLOCK,
        "every line is occupied"
    );

    let mut roots: Vec<Eterm> = Vec::with_capacity(1 + live.len());
    roots.push(big);
    roots.extend(live.iter().copied());
    let before_used = s.used_lines();

    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).expect("collect");

    // `marked` and `kept` are reset; `used_lines()` is the allocated popcount.
    let after = s.counters();
    assert_eq!(
        after.lines_marked, 0,
        "the mark bitmap is reset after a collection"
    );
    let used_after = s.used_lines();
    assert_eq!(
        used_after,
        LINES_PER_BLOCK + live.len(),
        "kept == live lines"
    );
    assert_eq!(
        used_after + s.free_lines(),
        s.line_count(),
        "used + free == total"
    );
    assert_eq!(
        after.lines_total - after.lines_free,
        used_after,
        "lines_free agrees"
    );
    assert_eq!(
        after.used_words,
        used_after * LINE_WORDS,
        "words == lines * LINE_WORDS"
    );
    assert_eq!(
        after.blocks_total,
        after.blocks_free + after.blocks_partial + after.blocks_all_marked,
        "the block classification partitions the space"
    );
    assert_eq!(after.blocks_all_marked, 1, "the full block stayed pinned");
    assert_eq!(after.blocks_free, 1, "the wholly dead block is free again");
    assert_eq!(after.blocks_partial, 1, "the partly live block is Partial");
    assert_eq!(
        stats.words_reclaimed,
        before_used.saturating_sub(used_after) * LINE_WORDS,
        "words_reclaimed is the net reduction in occupied lines"
    );

    // Re-marking the same live set must reproduce exactly the allocated lines and the same
    // block classification — the allocated bitmap is exactly the surviving (kept) lines.
    s.mark(&roots, &[]).expect("re-mark");
    let remark = s.counters();
    assert_eq!(
        remark.lines_marked, used_after,
        "allocated == kept == the lines the live set covers"
    );
    assert_eq!(remark.blocks_all_marked, after.blocks_all_marked);
    assert_eq!(remark.blocks_partial, after.blocks_partial);
    assert_eq!(remark.blocks_free, after.blocks_free);
    assert_eq!(remark.lines_free, after.lines_free);

    // Every freed line is really free: fill the whole remaining space with one-line objects,
    // which can only succeed (and only be safe) if the holes are exactly the non-surviving
    // lines. No survivor may be clobbered.
    let holes = s.free_lines();
    for i in 0..holes {
        let _ = s
            .tuple(&[make_small(5_000 + i as i64)])
            .expect("a hole holds a line");
    }
    assert_eq!(s.free_lines(), 0, "every hole was handed back out");
    assert_eq!(
        s.used_lines(),
        s.line_count(),
        "the space is completely full"
    );

    // The survivors are byte-intact: the pinned block and every evacuated live tuple.
    assert_full_block_tuple(roots[0], 0);
    for (i, root) in roots[1..].iter().enumerate() {
        assert_eq!(
            small_tuple_value(*root),
            100 + i as i64,
            "survivor {i} was clobbered by a fresh allocation"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. No double-free / no spurious release
// ---------------------------------------------------------------------------

/// A pinned `AllMarked` block is never also released: `blocks_reclaimed` counts only the
/// wholly-dead block, and the pinned object keeps its address and its lines.
#[test]
fn a_pinned_block_is_released_by_neither_reclaim_nor_block_counting() {
    let mut s = ImmixSpace::new(3 * BLOCK_WORDS).expect("space");

    // Block 0: a whole-block object that will classify `AllMarked` and be pinned.
    let big = s.tuple(&full_block_elems(0)).expect("big");
    // Block 1: sixteen wholly dead one-line tuples — the only reclaimable block.
    for i in 0..LINES_PER_BLOCK as i64 {
        let _ = s.tuple(&[make_small(900 + i)]).expect("garbage");
    }
    // Block 2 is never touched.
    assert_eq!(s.used_lines(), 2 * LINES_PER_BLOCK);

    let before_used = s.used_lines();
    let mut roots = [big];
    let mut promoted: [Eterm; 0] = [];
    let stats = s.collect(&mut roots, &mut promoted).expect("collect");

    assert_eq!(
        roots[0].raw(),
        big.raw(),
        "the pinned object is not released and not moved"
    );
    assert_full_block_tuple(roots[0], 0);

    let immix_after = s.counters();
    assert_eq!(
        immix_after.blocks_all_marked, 1,
        "the pinned block is still fully live"
    );
    assert_eq!(
        immix_after.blocks_free, 2,
        "the dead block and the untouched block are free"
    );

    // Exactly one block was reclaimed — the wholly dead one, never the block holding the
    // pinned object.
    assert_eq!(
        stats.blocks_reclaimed, 1,
        "only the wholly dead block is reclaimed"
    );
    assert_eq!(stats.objects_evacuated, 0, "nothing was evacuated");
    assert_eq!(stats.objects_pinned, 1, "the whole-block object is pinned");
    assert_eq!(stats.lines_pinned, LINES_PER_BLOCK);
    assert_eq!(stats.words_pinned, FULL_BLOCK_WORDS);

    let after_used = s.used_lines();
    assert_eq!(
        after_used, LINES_PER_BLOCK,
        "only the pinned block's lines survive"
    );
    assert_eq!(
        after_used,
        before_used - LINES_PER_BLOCK,
        "the pinned block's lines were retained, not released (a spurious release shows here)"
    );
    assert_eq!(
        stats.words_reclaimed,
        (before_used - after_used) * LINE_WORDS,
        "words_reclaimed is the net reduction in occupied lines"
    );

    // The freed block is really free: fill it and nothing else can be touched.
    let holes = s.free_lines();
    assert_eq!(holes, 2 * LINES_PER_BLOCK);
    for i in 0..holes {
        let _ = s
            .tuple(&[make_small(3_000 + i as i64)])
            .expect("a free line");
    }
    assert_eq!(s.free_lines(), 0, "every freed line is reusable");
    assert_full_block_tuple(roots[0], 0);
}

/// A repeated collection over an **unchanged** live set is idempotent: the same
/// [`ImmixStats`](beam_gc::prelude::ImmixStats), the same occupied line count, and the pinned
/// whole-block objects at byte-identical addresses on every round.
#[test]
fn repeated_collections_over_an_unchanged_live_set_are_idempotent() {
    let mut s = ImmixSpace::new(2 * BLOCK_WORDS).expect("space");

    // Two whole-block objects, one per block: both blocks are fully live, so both pin.
    let a = s.tuple(&full_block_elems(0)).expect("a");
    let b = s.tuple(&full_block_elems(1_000)).expect("b");
    assert_eq!(
        s.used_lines(),
        2 * LINES_PER_BLOCK,
        "the space is entirely live"
    );

    let mut roots = [a, b];
    let mut promoted: [Eterm; 0] = [];

    let mut previous: Option<ImmixStats> = None;
    for round in 0..3 {
        let stats = s.collect(&mut roots, &mut promoted).expect("collect");
        assert_eq!(
            roots[0].raw(),
            a.raw(),
            "pinned object a kept its address in round {round}"
        );
        assert_eq!(
            roots[1].raw(),
            b.raw(),
            "pinned object b kept its address in round {round}"
        );
        assert_eq!(stats.objects_pinned, 2, "round {round}: both blocks pin");
        assert_eq!(stats.objects_evacuated, 0, "round {round}: nothing moves");
        assert_eq!(stats.lines_pinned, 2 * LINES_PER_BLOCK, "round {round}");
        assert_eq!(
            stats.blocks_reclaimed, 0,
            "round {round}: nothing is reclaimed"
        );
        assert_eq!(
            stats.words_reclaimed, 0,
            "round {round}: no line is released"
        );
        assert_eq!(
            s.used_lines(),
            2 * LINES_PER_BLOCK,
            "round {round}: occupancy unchanged"
        );
        assert_eq!(
            s.counters().blocks_all_marked,
            2,
            "round {round}: both blocks pinned"
        );
        assert_eq!(
            s.counters().lines_marked,
            0,
            "round {round}: marked is reset"
        );
        if let Some(prev) = previous {
            assert_eq!(prev, stats, "round {round}: the collection is idempotent");
        }
        previous = Some(stats);

        assert_full_block_tuple(roots[0], 0);
        assert_full_block_tuple(roots[1], 1_000);
    }
}
