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

//! **The non-trivial promotion rollback** — a refusal *after* objects have already been
//! placed, copied and marked.
//!
//! [`tests/immix_fragmentation.rs`](immix_fragmentation) drives a
//! [minor](GenImmixCollector::collect_minor) against a region whose every block already holds
//! two 7-line objects (112 of 128 lines used, 16 free in 2-line tails). Every object of that
//! corpus is **7 lines**, so the *first* reservation finds no run: the promotion copies
//! nothing, `plan.copies` is **empty** when the rollback runs, and neither of the rollback's
//! two actions (restore a source word from its copy, un-claim a reservation through
//! [`ImmixSpace::release_words`](beam_gc::prelude::ImmixSpace::release_words)) executes. The
//! byte-identity that test asserts would therefore also hold for a rollback that did nothing.
//!
//! This file forces the **non-trivial** case with the same packed region and a corpus that
//! mixes object sizes:
//!
//! * the **first** root is a **2-word (1-line) tuple**, and the packed region still has a
//!   one-line hole in each block — so phase R reserves a line, copies the tuple there and
//!   overwrites the source's header word with the forwarding marker;
//! * the **second** root is a **100-word (7-line) tuple**, and no block has a 7-line run —
//!   so phase R is refused *after* that first copy exists.
//!
//! The refusal therefore runs the rollback over a **one-entry** `plan.copies`, and three
//! independent observations pin that it really undid that copy:
//!
//! 1. **The copy exists.** Phase R writes the copy verbatim into the reserved line and the
//!    rollback only un-claims the line, never clearing the words — so the released line still
//!    holds the small tuple's own words
//!    ([`the_refusal_lands_after_a_placed_copy_and_undoes_it`] reads them). This is the half a
//!    trivial refusal cannot produce.
//! 2. **The source was restored.** The nursery is byte-identical, which (unlike the trivial
//!    case) *is* load-bearing here: a rollback that skipped its restore loop would leave
//!    `make_boxed(dst)` in the small tuple's header word.
//! 3. **The allocation was un-claimed.** The region's accounting view
//!    ([`ImmixCounters`](beam_gc::prelude::ImmixCounters)) is exactly the one from before the
//!    attempt — 112 used / 16 free lines, 8 partial blocks, the same `lines_marked` — and the
//!    released line is reusable: promoting the small tuple alone afterwards succeeds and takes
//!    one of the two leftover tail lines.
//!
//! Everything asserted here is a count, an accounting record or a structural comparison
//! (`beam_term::compare`); nothing is timed.

use beam_common::BeamError;
use beam_gc::prelude::{
    AnyCollector, CollectionKind, GcConfig, GenImmixCollector, HeapPolicy, HeapSpace, ImmixCounters,
    MatureKind, BLOCK_WORDS, LINES_PER_BLOCK, LINE_WORDS,
};
use beam_term::compare::eq as term_eq;
use beam_term::prelude::{
    arityval, boxed_val, is_arity_value, make_small, signed_value, AtomTable, Eterm,
};

/// The mature region every scenario runs against: 2048 words = 8 blocks = 128 lines.
const REGION_WORDS: usize = 2048;

/// The nursery, and the floor no collection can shrink it below.
const NURSERY_WORDS: usize = 1024;

/// The packer's object: a 100-word tuple (a header plus 99 elements) = exactly **7** Immix
/// lines, so two of them fill 14 of a block's 16 lines.
const PACK_ELEMS: usize = 99;

/// The first promoted object: a 2-word tuple (a header plus one element) = exactly **one**
/// Immix line, so it fits one of the two tail lines the packer leaves free.
const SMALL_ELEMS: usize = 1;

/// The small object's only element.
const SMALL_VALUE: i64 = 7;

/// The packing corpus' elements are `BIG_BASE + i`.
const BIG_BASE: i64 = 5000;

/// How many packer tuples one packing round promotes.
const PER_ROUND: usize = 2;

/// The occupancy the packer drives the region to: 8 blocks x 2 objects x 7 lines.
const PACKED_LINES: usize = 8 * 2 * 7;

/// The line the refused attempt's **first** placement took: the packer fills blocks in order
/// and leaves each block's last two lines free, so the allocator is sitting in the last block
/// and its first hole is that block's 14th line.
const REFUSED_LINE: usize =
    (REGION_WORDS / BLOCK_WORDS - 1) * LINES_PER_BLOCK + (LINES_PER_BLOCK - 2);

/// The heap policy every scenario uses: the region under test, and a nursery no collection
/// shrinks below one round's corpus.
fn policy() -> HeapPolicy {
    HeapPolicy {
        min_heap_words: NURSERY_WORDS,
        initial_young_words: NURSERY_WORDS,
        initial_mature_words: REGION_WORDS,
        ..HeapPolicy::default()
    }
}

/// The lines an object of `words` term words occupies — the run length a block must have free.
fn lines_of(words: usize) -> usize {
    words.div_ceil(LINE_WORDS)
}

/// Allocates a tuple of `values` in `c`'s nursery.
fn tuple_for(c: &mut GenImmixCollector, values: &[Eterm]) -> Eterm {
    c.young_mut()
        .heap_mut()
        .tuple(values)
        .expect("the tuple fits the nursery")
}

/// Allocates the **first** promoted object: the 1-line tuple.
fn small_tuple(c: &mut GenImmixCollector) -> Eterm {
    tuple_for(c, &[make_small(SMALL_VALUE)])
}

/// Allocates the **second** promoted object: the 7-line tuple.
fn big_tuple(c: &mut GenImmixCollector) -> Eterm {
    let values: Vec<Eterm> = (0..PACK_ELEMS as i64)
        .map(|i| make_small(BIG_BASE + i))
        .collect();
    tuple_for(c, &values)
}

/// The `round`-tagged packing corpus: [`PER_ROUND`] fresh 7-line tuples in the nursery.
fn pack_corpus(c: &mut GenImmixCollector, round: i64) -> Vec<Eterm> {
    (0..PER_ROUND as i64)
        .map(|i| {
            let tag = round * 100 + i;
            let values: Vec<Eterm> = (0..PACK_ELEMS as i64)
                .map(|j| make_small(tag * 1000 + j))
                .collect();
            tuple_for(c, &values)
        })
        .collect()
}

/// Asserts `term` is a tuple of `values` — element by element, so a mis-placed or half-copied
/// object fails wherever it is read.
fn assert_tuple_is(term: Eterm, values: &[Eterm]) {
    // SAFETY: `term` is a live tuple of `values.len() + 1` words the caller's graph holds.
    let header = unsafe { *boxed_val(term) };
    assert!(is_arity_value(header), "a live tuple header");
    assert_eq!(arityval(header), values.len());
    for (i, expected) in values.iter().enumerate() {
        // SAFETY: elements 1..=len of a live `len + 1`-word tuple.
        let actual = unsafe { *boxed_val(term).add(1 + i) };
        assert_eq!(signed_value(actual), signed_value(*expected), "element {i}");
    }
}

/// The nursery's **live region word for word** — the byte-identity witness: a forwarding
/// marker left in a from-space object, or an unrestored source word, changes one of these.
fn nursery_snapshot(c: &GenImmixCollector) -> Vec<Eterm> {
    let space: &HeapSpace = c.young();
    let base = space.base();
    let used = space.used_words();
    // SAFETY: `[base, base + used)` is exactly the region this nursery handed the mutator, so
    // every word in it is initialised.
    (0..used).map(|i| unsafe { *base.add(i) }).collect()
}

/// Builds a collector over [`policy`] and packs the region to [`PACKED_LINES`] lines with
/// 7-line tuples, asserting every round was a minor that left its roots intact.
fn packed_collector() -> (GenImmixCollector, i64) {
    let mut c = GenImmixCollector::from_policy(policy()).expect("collector");
    assert_eq!(c.mature().capacity_words(), REGION_WORDS);
    assert_eq!(c.mature().block_count(), REGION_WORDS / BLOCK_WORDS);
    let mut round = 0i64;
    while c.mature().used_lines() < PACKED_LINES {
        let mut roots = pack_corpus(&mut c, round);
        let collection = c
            .collect(&mut roots)
            .unwrap_or_else(|err| panic!("packing round {round}: {err:?}"));
        assert_eq!(collection.kind, CollectionKind::Minor, "round {round}");
        for (i, root) in roots.iter().enumerate() {
            let tag = round * 100 + i as i64;
            let expected: Vec<Eterm> = (0..PACK_ELEMS as i64)
                .map(|j| make_small(tag * 1000 + j))
                .collect();
            assert_tuple_is(*root, &expected);
        }
        round += 1;
    }
    (c, round)
}

/// Asserts the region is exactly the packed one the refusal needs: 112 used lines, 16 free
/// ones, **a one-line run** (for the first promoted object) and **no seven-line run** (for the
/// second). The promotion's own line need is covered by the region-wide `free_lines()`, so the
/// refusal comes from its line-granular reservation, not from `collect_minor`'s up-front check.
fn assert_packed_geometry(c: &GenImmixCollector) {
    let region = c.mature();
    assert_eq!(region.used_lines(), PACKED_LINES);
    assert_eq!(region.free_lines(), 16, "two unusable lines per block");
    assert_eq!(region.line_count(), REGION_WORDS / LINE_WORDS);
    assert!(
        region.can_reserve(SMALL_ELEMS + 1),
        "a one-line hole is free, so the promotion's FIRST object ({} words) is placed",
        SMALL_ELEMS + 1
    );
    assert!(
        !region.can_reserve(PACK_ELEMS + 1),
        "no seven-line run exists, so the promotion's SECOND object ({} words) is refused",
        PACK_ELEMS + 1
    );
    let needed = lines_of(SMALL_ELEMS + 1) + lines_of(PACK_ELEMS + 1);
    assert_eq!(needed, 8, "1 line for the small object, 7 for the big one");
    assert!(
        region.free_lines() >= needed,
        "the region-wide pre-check ({needed} lines needed, {} free) passes, so the refusal \
         comes from the promotion itself",
        region.free_lines()
    );
}

/// The mixed corpus both scenarios promote: the 1-line tuple **first** (it is placed), then
/// the 7-line tuple (it is refused). A promotion walks the roots in order, so the order of this
/// vector is what makes the refusal land on a later object.
fn mixed_corpus(c: &mut GenImmixCollector) -> Vec<Eterm> {
    let small = small_tuple(c);
    let big = big_tuple(c);
    vec![small, big]
}

/// Asserts each root of `[small, big]` is structurally equal to an independently built
/// witness — the "nothing was corrupted" check the recovery scenarios use.
fn assert_mixed_corpus_intact(roots: &[Eterm]) {
    assert_eq!(roots.len(), 2);
    let atoms = AtomTable::new();
    let mut witness = beam_term::heap::Heap::new(PACK_ELEMS + 8);
    let w_small = witness.tuple(&[make_small(SMALL_VALUE)]).expect("witness");
    assert!(term_eq(roots[0], w_small, &atoms), "the small tuple");
    witness.reset();
    let big: Vec<Eterm> = (0..PACK_ELEMS as i64)
        .map(|i| make_small(BIG_BASE + i))
        .collect();
    let w_big = witness.tuple(&big).expect("witness");
    assert!(term_eq(roots[1], w_big, &atoms), "the big tuple");
}

// ---------------------------------------------------------------------------
// 1. The refusal lands after a placed copy — and that copy is observable
// ---------------------------------------------------------------------------

#[test]
fn the_refusal_lands_after_a_placed_copy_and_undoes_it() {
    let (mut c, rounds) = packed_collector();
    assert_eq!(rounds, 8, "16 seven-line objects, two per round");
    assert_packed_geometry(&c);

    let mut roots = mixed_corpus(&mut c);
    let nursery_before = nursery_snapshot(&c);
    let counters_before: ImmixCounters = c.mature().counters();

    let err = c
        .collect_minor(&mut roots)
        .expect_err("the region has no seven-line run for the second object");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");

    // **The positive half.** The first object WAS placed: phase R copied the 2-word tuple
    // verbatim into the one-line hole the packed region left (line `REFUSED_LINE` of the
    // region — the block the allocator was filling, its first free line) and the rollback only
    // un-claims that line; it never clears the words. The copy is therefore still readable
    // there, which is exactly what a refusal on the *first* reservation could not show.
    let base = c.mature().base();
    let dst = base.wrapping_add(REFUSED_LINE * LINE_WORDS);
    // SAFETY: `REFUSED_LINE * LINE_WORDS < capacity_words`, so `dst` is inside the region, and
    // its first two words were initialised by the refused attempt's verbatim copy of the
    // 2-word small tuple. Nothing has written them since — the rollback restores *sources* and
    // un-claims lines (metadata only), and the accounting queries read bitmaps.
    unsafe {
        assert!(
            is_arity_value(*dst),
            "the released line still holds the placed copy's header word"
        );
        assert_eq!(arityval(*dst), SMALL_ELEMS, "the copy is the small tuple");
        assert_eq!(
            *dst.add(1),
            make_small(SMALL_VALUE),
            "the copy holds the small tuple's element"
        );
    }

    // **The negative half.** The copy was undone: the small tuple's source word carries its own
    // header again, not the `make_boxed(dst)` forwarding marker. With a placed copy this
    // assertion is load-bearing — a rollback that skipped its restore loop would leave the
    // marker here, which the fully packed (all-7-line) corpus can never show.
    assert_eq!(
        nursery_snapshot(&c),
        nursery_before,
        "the nursery is byte-identical: no marker survived in the object that WAS placed"
    );

    // The line the copy took is free again and the accounting view is unchanged.
    assert_eq!(c.mature().used_lines(), PACKED_LINES);
    assert_eq!(c.mature().free_lines(), 16);
    assert_eq!(c.mature().used_words(), PACKED_LINES * LINE_WORDS);
    assert_eq!(
        c.mature().counters(),
        counters_before,
        "every block/line counter is exactly what it was before the attempt"
    );
    assert_eq!(c.mature().block_count(), REGION_WORDS / BLOCK_WORDS);
    assert!(
        c.mature().can_reserve(SMALL_ELEMS + 1),
        "the released line is a usable one-line hole again"
    );
    assert!(
        !c.mature().can_reserve(PACK_ELEMS + 1),
        "and the fragmentation the refusal reported is unchanged"
    );
    println!(
        "placed-then-refused: copy at line {REFUSED_LINE}, region back to {} used / {} free lines",
        c.mature().used_lines(),
        c.mature().free_lines()
    );
}

// ---------------------------------------------------------------------------
// 2. The rollback is reproducible and the region is still usable afterwards
// ---------------------------------------------------------------------------

#[test]
fn the_rolled_back_region_is_reproducible_and_reusable() {
    let (mut c, _rounds) = packed_collector();
    assert_packed_geometry(&c);

    let mut roots = mixed_corpus(&mut c);
    let roots_before = roots.clone();
    let nursery_before = nursery_snapshot(&c);
    let counters_before: ImmixCounters = c.mature().counters();
    let stats_before = c.stats();

    // The refusal is a pure function of the region: repeating it changes nothing, so the
    // rollback is idempotent from the caller's point of view.
    for attempt in 0..2 {
        let err = c
            .collect_minor(&mut roots)
            .expect_err("the refusal is reproducible");
        assert!(matches!(err, BeamError::Alloc(_)), "attempt {attempt}");
        assert_eq!(roots, roots_before, "attempt {attempt}: no root rewritten");
        assert_eq!(
            nursery_snapshot(&c),
            nursery_before,
            "attempt {attempt}: the nursery is byte-identical again"
        );
        assert_eq!(
            c.mature().counters(),
            counters_before,
            "attempt {attempt}: the allocation state is restored again"
        );
        assert_eq!(
            c.stats(),
            stats_before,
            "attempt {attempt}: a refused minor is not counted"
        );
        assert!(
            !c.mature().contains_term(roots[0]) && c.young().contains_term(roots[0]),
            "attempt {attempt}: the small root is still resident in the un-forwarded nursery"
        );
        assert!(
            c.young().contains_term(roots[1]),
            "attempt {attempt}: the big root is still resident in the un-forwarded nursery"
        );
    }

    // The region is still usable: the small object now promotes on its own, into one of the
    // two tail lines the packer left free — so the line the refused attempt reserved really was
    // released (a leaked reservation would have refused this too, or double-claimed a line).
    let mut only_small = [roots[0]];
    let stats = c
        .collect_minor(&mut only_small)
        .expect("the released lines are reusable");
    assert_eq!(stats.objects_copied, 1, "just the small tuple moved");
    assert_eq!(stats.words_copied, 2, "a header plus one element");
    assert_eq!(
        c.mature().used_lines(),
        PACKED_LINES + 1,
        "the placement took exactly one line"
    );
    assert_eq!(c.mature().free_lines(), 15);
    let placed = only_small[0];
    assert!(c.mature().contains_term(placed), "the small root is mature now");
    assert_tuple_is(placed, &[make_small(SMALL_VALUE)]);
    // It went into one of the leftover tail lines — the only holes the packed region has.
    let line_bytes = LINE_WORDS * core::mem::size_of::<Eterm>();
    let line = (boxed_val(placed) as usize - c.mature().base() as usize) / line_bytes;
    assert!(
        line % LINES_PER_BLOCK >= LINES_PER_BLOCK - 2,
        "the small object took a leftover tail line (line {line})"
    );
    println!(
        "reusable: the small tuple placed at line {line}, region at {} used / {} free lines",
        c.mature().used_lines(),
        c.mature().free_lines()
    );
}

// ---------------------------------------------------------------------------
// 3. Recovery: the same refusal escalates to a fullsweep, through the concrete collector
// ---------------------------------------------------------------------------

#[test]
fn the_refused_minor_escalates_to_a_completing_fullsweep() {
    let (mut c, rounds) = packed_collector();
    assert_packed_geometry(&c);
    assert_eq!(c.stats().minor_collections as i64, rounds);

    let mut roots = mixed_corpus(&mut c);
    let collection = c.collect(&mut roots).expect("the escalation completes");

    assert_eq!(
        collection.kind,
        CollectionKind::Major,
        "a would-not-fit minor escalates to a fullsweep"
    );
    assert_eq!(c.stats().major_collections, 1, "one fullsweep ran");
    assert_eq!(
        c.stats().minor_collections as i64,
        rounds,
        "the refused minor is not counted, and no extra minor ran"
    );
    // The recovered region holds exactly the live set: one line for the small tuple, seven for
    // the big one. (A leaked reservation from the refused attempt would show up here.)
    assert_eq!(
        c.mature().used_lines(),
        lines_of(SMALL_ELEMS + 1) + lines_of(PACK_ELEMS + 1),
        "exactly the new round's live set is resident"
    );
    for root in &roots {
        assert!(c.mature().contains_term(*root), "every root was promoted");
    }
    assert_mixed_corpus_intact(&roots);
    println!(
        "escalation: {:?} with {} used lines, {} words moved",
        collection.kind,
        c.mature().used_lines(),
        collection.stats.words_copied
    );
}

// ---------------------------------------------------------------------------
// 4. Recovery: the same refusal escalates through the strategy-dispatching seam
// ---------------------------------------------------------------------------

#[test]
fn the_strategy_seam_reports_the_same_refusal_and_recovery() {
    let mut c = AnyCollector::from_config(&GcConfig::immix(policy())).expect("immix collector");
    assert_eq!(c.kind(), MatureKind::Immix, "the explicit strategy pin");
    assert_eq!(c.mature_capacity_words(), REGION_WORDS);

    // Pack the region the same way the concrete collector is packed.
    let atoms = AtomTable::new();
    let mut witness = beam_term::heap::Heap::new(PACK_ELEMS + 8);
    let mut round = 0i64;
    while c.mature_used_words() / LINE_WORDS < PACKED_LINES {
        let mut roots: Vec<Eterm> = (0..PER_ROUND as i64)
            .map(|i| {
                let tag = round * 100 + i;
                let values: Vec<Eterm> = (0..PACK_ELEMS as i64)
                    .map(|j| make_small(tag * 1000 + j))
                    .collect();
                c.young_mut()
                    .heap_mut()
                    .tuple(&values)
                    .expect("the tuple fits the nursery")
            })
            .collect();
        let collection = c
            .collect(&mut roots)
            .unwrap_or_else(|err| panic!("packing round {round}: {err:?}"));
        assert_eq!(collection.kind, CollectionKind::Minor, "round {round}");
        for (i, root) in roots.iter().enumerate() {
            let tag = round * 100 + i as i64;
            witness.reset();
            let expected: Vec<Eterm> = (0..PACK_ELEMS as i64)
                .map(|j| make_small(tag * 1000 + j))
                .collect();
            let w = witness.tuple(&expected).expect("witness");
            assert!(term_eq(*root, w, &atoms), "packing round {round}: root {i}");
        }
        round += 1;
    }
    assert_eq!(c.mature_used_words() / LINE_WORDS, PACKED_LINES);
    assert_eq!(c.mature_available_words() / LINE_WORDS, 16);

    // The mixed corpus: the 1-line tuple is placed, the 7-line one is refused.
    let small = c
        .young_mut()
        .heap_mut()
        .tuple(&[make_small(SMALL_VALUE)])
        .expect("small tuple");
    let big: Vec<Eterm> = (0..PACK_ELEMS as i64)
        .map(|i| make_small(BIG_BASE + i))
        .collect();
    let big = c.young_mut().heap_mut().tuple(&big).expect("big tuple");
    let mut roots = vec![small, big];
    let roots_before = roots.clone();

    let err = c
        .collect_minor(&mut roots)
        .expect_err("the seam reports the same refusal");
    assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");
    assert_eq!(roots, roots_before, "no root rewritten");
    assert_eq!(
        c.mature_used_words() / LINE_WORDS,
        PACKED_LINES,
        "the seam sees the same restored occupancy"
    );
    assert_eq!(c.mature_available_words() / LINE_WORDS, 16);

    // ... and the same recovery.
    let collection = c.collect(&mut roots).expect("the seam escalates too");
    assert_eq!(collection.kind, CollectionKind::Major);
    assert_eq!(
        c.mature_used_words() / LINE_WORDS,
        lines_of(SMALL_ELEMS + 1) + lines_of(PACK_ELEMS + 1)
    );
    assert_mixed_corpus_intact(&roots);
    println!(
        "seam: refused then escalated to {:?} with {} used lines",
        collection.kind,
        c.mature_used_words() / LINE_WORDS
    );
}
