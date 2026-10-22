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

//! **Retention accounting**: one snapshot of the runtime's four storage regions.
//!
//! `beam_rt` holds four regions whose capacity outlives the call that needed it, and until this
//! module existed not one of them was observable from outside:
//!
//! 1. the **arena** — [`Runtime::arena`], the engine's `MinimalContext` whose `ContextHeap` keeps
//!    a `Vec<ReservedHeap>` that only ever grows (`beam_engine/src/context.rs`); on the
//!    `-eval`/table-less route every term and every nested `Machine`'s heap/stack is allocated
//!    there, through [`Runtime::alloc_words`] — the one region with no size policy at all;
//! 2. the **pooled BIF heap** — [`Runtime::bif_heaps`], a stack of regions that is **depth-sized**
//!    (`bif_heap_words` for the outermost call, `NESTED_BIF_HEAP_SEED_WORDS` for a nested one) and
//!    reused across calls, where a growing call *retires* its region back into the pool
//!    (`crate::context::calls`'s `take_bif_frontier`, which grows geometrically from the region's
//!    own capacity);
//! 3. the **`-eval` term store** — [`Runtime::eval_heap`] plus the regions
//!    [`Runtime::grow_eval_heap`] replaced into [`Runtime::eval_heap_retired`] (the growth doubles
//!    the capacity, clamped at `MAX_EVAL_HEAP_WORDS` = 256 MiB, and never frees the old region,
//!    because a `beam_alloc::Heap` never moves a term);
//! 4. the **escaped off-heap payload store** — [`Runtime::eval_off_heap`], the off-heap table that
//!    *owns* the payload references every copy onto the store depends on.
//!
//! [`RetentionStats`] reports each of them, [`Runtime::retention_stats`] reads it, and
//! `BEAM_RT_MEM_TRACE` prints one line of it after the boot and again on the way out of
//! [`crate::cli::run`] — a **diagnostic only**, with no behaviour depending on it.
//!
//! # This is instrumentation, not reclamation
//!
//! Nothing here frees, shrinks, caps or otherwise changes a region: the snapshot only reads sizes
//! that already exist. A figure that grows across a long in-process `compile:file/2` is therefore
//! a statement about the *current* allocation policy, not an effect of measuring it.
use super::Runtime;
use core::fmt;

/// A snapshot of the words the runtime's four never-freed storage regions hold, plus the arena
/// demand counters.
///
/// Every field is a **measurement of current state** ([`Runtime::retention_stats`]); none is
/// cleared, reset or adjusted by reading it. Word counts are `Eterm` words (8 bytes each on a
/// 64-bit host); `capacity` means the region's reserved-and-usable word count, never its written
/// prefix.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RetentionStats {
    /// The **cumulative** words [`Runtime::alloc_words`] has handed out of the arena.
    ///
    /// A low/er bound on what the arena retains, never the exact figure: the engine's
    /// `ContextHeap` never frees a region, but a region's *reserved* capacity is the previous
    /// region's doubled (`ContextHeap::fresh_region`), and a request that does not fit the
    /// frontier's committed slice raises that commitment in place. The arena therefore holds at
    /// least the words handed out and in general considerably more address space than that. The
    /// engine exposes no accessor for its region list (`ContextHeap::regions` is private to
    /// `beam_engine`), so this demand counter is the honest observable.
    ///
    /// A request of `0` words still counts as a call (it hands out the documented dangling
    /// pointer and reserves nothing), so `arena_allocs` is "calls served", not "calls that
    /// reserved".
    pub arena_words: u64,
    /// How many calls [`Runtime::alloc_words`] has served, cumulatively — the counter
    /// [`arena_words`](RetentionStats::arena_words) is summed over.
    pub arena_allocs: u64,
    /// How many regions the pooled BIF heap ([`Runtime::bif_heaps`]) currently holds.
    ///
    /// The pool is a stack of [`beam_alloc::Heap`]s: `1` on a fresh runtime (its seed region), one
    /// more per nested call, and — the accumulation this field is for — one more per **retired**
    /// region a growing call left behind (`crate::context::calls`'s `take_bif_frontier` pushes the
    /// exhausted region back and installs a doubled one).
    pub bif_regions: usize,
    /// The summed [`Heap::capacity`](beam_alloc::Heap::capacity) of the pooled regions, in words.
    pub bif_words: u64,
    /// The summed [`Heap::peak_used`](beam_alloc::Heap::peak_used) of the pooled regions, in words —
    /// how much of them any caller has ever actually written.
    ///
    /// The counterpart of [`bif_words`](RetentionStats::bif_words): `peak_used` is a
    /// **high-water mark that a rewind preserves** (`Heap::reset`/`reset_to` rewind the bump
    /// pointer, never the peak), so this figure says what the pool's *capacity* bought — one
    /// region that served 40 K words out of a 16 M-word reservation is 128 MiB of reservation for
    /// 320 KiB of use.
    pub bif_peak_used_words: u64,
    /// The largest single pooled region's capacity, in words — the per-region figure whose
    /// multiplication by the nesting depth is what a nested call's fresh region costs.
    pub bif_max_capacity_words: u64,
    /// How many pooled BIF heaps are **out on loan** right now — the runtime's current nested-call
    /// depth (`Runtime::take_bif_heap` counts one up per heap handed out and
    /// `Runtime::put_bif_heap` one down per heap returned).
    ///
    /// `0` between calls, `1` inside a top-level call, `2` and up inside a fun that call applied.
    /// It is the multiplier the pooled BIF heap's retention used to scale with.
    pub bif_heaps_taken: usize,
    /// The seed size, in words, a **nested** call's fresh region gets when the pool is empty
    /// ([`Runtime::bif_nested_heap_words`]).
    pub bif_nested_heap_words: usize,
    /// The high-water `used()` ever observed on a region a **nested** call held
    /// ([`Runtime::bif_nested_peak_words`]) — the measurement
    /// [`bif_nested_heap_words`](RetentionStats::bif_nested_heap_words) self-tunes from, and the
    /// figure that says how much of a nested region's capacity was really bought.
    pub bif_nested_peak_words: usize,
    /// The **current** `-eval` term store's capacity
    /// ([`Runtime::eval_heap`]'s [`Heap::capacity`](beam_alloc::Heap::capacity)), in words.
    ///
    /// This is the region new store terms are allocated from — not what the store retains in
    /// total; pair it with
    /// [`eval_heap_retired_words`](RetentionStats::eval_heap_retired_words).
    pub eval_heap_words: u64,
    /// How many replaced store regions [`Runtime::grow_eval_heap`] has kept alive
    /// ([`Runtime::eval_heap_retired`]'s length).
    ///
    /// Each one is a region that handed out terms still in use, so it is never freed and never
    /// relocated.
    pub eval_heap_retired_regions: usize,
    /// The summed capacity of those retired store regions, in words.
    ///
    /// `eval_heap_words + eval_heap_retired_words` is the store's total retained capacity.
    pub eval_heap_retired_words: u64,
    /// How many **MSO handles** the escaped off-heap payload store
    /// ([`Runtime::eval_off_heap`]) currently holds.
    ///
    /// [`beam_message::OffHeapTable::len`] counts the table's MSO slots only: the refc-binary and
    /// node lists the same table carries are **not** included, and `beam_message` exposes no
    /// combined handle count (its binaries are reachable only through `binaries()` and its nodes
    /// through `nodes()`). `beam_message` is a sibling crate this node cannot change, so the field
    /// reports the one handle count that exists rather than fabricating a total.
    ///
    /// A table that has only had `push_binary` calls made on it therefore still reads `0` here
    /// (the trap `src/CONTEXT.md` records for anyone pruning that table).
    pub eval_off_heap_handles: usize,
    /// How many store regions a [`crate::runtime::reclaim`] pass **compacted** (its live words
    /// moved).
    ///
    /// The store move ([`crate::runtime::reclaim::compact`]): one region holding a live word is
    /// evacuated into a fresh right-sized region so its source can be dropped. A region whose live
    /// fraction is too high, whose candidates are below the window threshold, or whose pass declined
    /// is never compacted.
    pub store_compactions: u64,
    /// How many store words the reclaim pass's drops released, summed
    /// ([`RegionReclaimStats::store_words_reclaimed`](crate::runtime::RegionReclaimStats)).
    ///
    /// A dropped retired region contributes its whole written prefix; the current region
    /// contributes the window prefix a reset released; a move contributes its candidates' **dead**
    /// words. [`eval_heap_retired_words`] is the falling counterpart.
    ///
    /// [`eval_heap_retired_words`]: RetentionStats::eval_heap_retired_words
    pub store_words_reclaimed: u64,
    /// How many **retired** store regions the reclaim pass dropped, summed
    /// ([`RegionReclaimStats::store_regions_dropped`](crate::runtime::RegionReclaimStats)).
    ///
    /// The falling counterpart of
    /// [`eval_heap_retired_regions`](RetentionStats::eval_heap_retired_regions).
    pub store_regions_dropped: u64,
    /// How many store **move** attempts declined, for any reason in
    /// [`crate::runtime::reclaim::compact`]'s decline list
    /// ([`RegionReclaimStats::store_move_declines`](crate::runtime::RegionReclaimStats)).
    ///
    /// Kept apart from [`reclaim_declines`](RetentionStats::reclaim_declines): the move is an
    /// optional extra action, so a moved-off A/B run must not read as if the whole pass refused.
    pub store_move_declines: u64,
    /// The reason of the most recent store-move decline — see
    /// [`crate::runtime::reclaim::compact`]'s decline list. `None` until the first one.
    pub store_move_last_decline: Option<&'static str>,
    /// How many times the reclaim pass rewound the **engine arena**
    /// ([`RegionReclaimStats::arena_reclaims`](crate::runtime::RegionReclaimStats)).
    pub arena_reclaims: u64,
    /// How many arena words the rewinds released, summed.
    ///
    /// This is the **live** side of the arena, and the counterpart of
    /// [`arena_words`](RetentionStats::arena_words) — which keeps its cumulative-demand meaning and
    /// is *not* lowered by a rewind. `beam_engine::ContextHeapStats::live_words` is the same figure
    /// as a level.
    pub arena_words_reclaimed: u64,
    /// How many arena regions the rewinds dropped, summed.
    pub arena_regions_dropped: u64,
    /// How many reclaim passes declined because soundness could not be shown.
    ///
    /// A decline touches nothing: both regions stay exactly as they were, so this counter and
    /// [`reclaim_last_decline`](RetentionStats::reclaim_last_decline) are how a measurement tells
    /// "the pass did nothing" from "the pass had nothing to do".
    pub reclaim_declines: u64,
    /// The reason of the most recent reclaim decline — see [`crate::runtime::reclaim`]'s decline
    /// list. `None` until the first decline.
    pub reclaim_last_decline: Option<&'static str>,
}

impl fmt::Display for RetentionStats {
    /// Renders the snapshot as **one** line of `label=value` pairs, in field order — the shape
    /// `BEAM_RT_MEM_TRACE` prints, and stable enough for a script to grep a single figure out of.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "arena_words={} arena_allocs={} bif_regions={} bif_words={} \
             bif_peak_used_words={} bif_max_capacity_words={} bif_heaps_taken={} \
             bif_nested_heap_words={} bif_nested_peak_words={} eval_heap_words={} \
             eval_heap_retired_regions={} eval_heap_retired_words={} eval_off_heap_handles={} \
             store_compactions={} store_words_reclaimed={} store_regions_dropped={} \
             store_move_declines={} store_move_last_decline={} \
             arena_reclaims={} arena_words_reclaimed={} arena_regions_dropped={} \
             reclaim_declines={} reclaim_last_decline={}",
            self.arena_words,
            self.arena_allocs,
            self.bif_regions,
            self.bif_words,
            self.bif_peak_used_words,
            self.bif_max_capacity_words,
            self.bif_heaps_taken,
            self.bif_nested_heap_words,
            self.bif_nested_peak_words,
            self.eval_heap_words,
            self.eval_heap_retired_regions,
            self.eval_heap_retired_words,
            self.eval_off_heap_handles,
            self.store_compactions,
            self.store_words_reclaimed,
            self.store_regions_dropped,
            self.store_move_declines,
            self.store_move_last_decline.unwrap_or("none"),
            self.arena_reclaims,
            self.arena_words_reclaimed,
            self.arena_regions_dropped,
            self.reclaim_declines,
            // `Option<&'static str>` has no `Display`: a `None` is the documented "no decline has
            // happened yet", rendered as `none` so the line stays one `label=value` pair per field.
            self.reclaim_last_decline.unwrap_or("none"),
        )
    }
}

impl Runtime {
    /// The runtime's **retention accounting**: what the four never-freed storage regions hold
    /// right now (see [`RetentionStats`], whose every field documents its own source).
    ///
    /// It is a pure read — `O(regions)` (one capacity sum over the pooled BIF heaps and one over
    /// the retired store regions), no allocation, no mutation — so a test or a diagnostic may call
    /// it at any point. Nothing in the runtime consults it.
    pub fn retention_stats(&self) -> RetentionStats {
        let reclaim = self.region_reclaim_stats();
        RetentionStats {
            arena_words: self.arena_alloc_words,
            arena_allocs: self.arena_allocs,
            bif_regions: self.bif_heaps.len(),
            bif_words: self
                .bif_heaps
                .iter()
                .map(|heap| heap.capacity() as u64)
                .sum(),
            bif_peak_used_words: self
                .bif_heaps
                .iter()
                .map(|heap| heap.peak_used() as u64)
                .sum(),
            bif_max_capacity_words: self
                .bif_heaps
                .iter()
                .map(|heap| heap.capacity())
                .max()
                .unwrap_or(0) as u64,
            bif_heaps_taken: self.bif_heaps_taken,
            bif_nested_heap_words: self.bif_nested_heap_words,
            bif_nested_peak_words: self.bif_nested_peak_words,
            eval_heap_words: self.eval_heap.capacity() as u64,
            eval_heap_retired_regions: self.eval_heap_retired.len(),
            eval_heap_retired_words: self
                .eval_heap_retired
                .iter()
                .map(|heap| heap.capacity() as u64)
                .sum(),
            eval_off_heap_handles: self.eval_off_heap.len(),
            store_compactions: reclaim.store_compactions,
            store_words_reclaimed: reclaim.store_words_reclaimed,
            store_regions_dropped: reclaim.store_regions_dropped,
            store_move_declines: reclaim.store_move_declines,
            store_move_last_decline: reclaim.store_move_last_decline,
            arena_reclaims: reclaim.arena_reclaims,
            arena_words_reclaimed: reclaim.arena_words_reclaimed,
            arena_regions_dropped: reclaim.arena_regions_dropped,
            reclaim_declines: reclaim.reclaim_declines,
            reclaim_last_decline: reclaim.reclaim_last_decline,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::NESTED_BIF_HEAP_SEED_WORDS;

    /// The snapshot is a read of the runtime's own state, so the tests drive the regions through
    /// their real entry points (`alloc_words`, `take_bif_heap`, `grow_eval_heap`) and assert the
    /// figures move with them.
    fn runtime() -> Runtime {
        Runtime::with_defaults().expect("the default runtime")
    }

    #[test]
    fn a_fresh_runtime_reports_only_its_seed_regions() {
        let rt = runtime();
        let stats = rt.retention_stats();
        assert_eq!(stats.arena_words, 0, "nothing has been served yet");
        assert_eq!(stats.arena_allocs, 0);
        assert_eq!(stats.bif_regions, 1, "the pool starts with its seed region");
        assert_eq!(stats.bif_words, rt.bif_heaps[0].capacity() as u64);
        assert_eq!(stats.eval_heap_words, rt.eval_heap.capacity() as u64);
        assert_eq!(stats.eval_heap_retired_regions, 0);
        assert_eq!(stats.eval_heap_retired_words, 0);
        assert_eq!(stats.eval_off_heap_handles, 0);
    }

    #[test]
    fn the_arena_counters_are_the_cumulative_demand_alloc_words_served() {
        let mut rt = runtime();
        let before = rt.retention_stats();
        rt.alloc_words(8).expect("the arena serves 8 words");
        let after_one = rt.retention_stats();
        assert_eq!(after_one.arena_allocs, before.arena_allocs + 1);
        assert_eq!(after_one.arena_words, before.arena_words + 8);
        rt.alloc_words(16).expect("the arena serves 16 more words");
        let after_two = rt.retention_stats();
        assert_eq!(after_two.arena_allocs, before.arena_allocs + 2);
        assert_eq!(
            after_two.arena_words,
            before.arena_words + 8 + 16,
            "the words are a running sum, never a high-water mark"
        );
        // A zero-word request is still a served call (it hands out the documented dangling
        // pointer and reserves no storage), which is what the field documents.
        rt.alloc_words(0).expect("a zero-word request is served");
        let after_zero = rt.retention_stats();
        assert_eq!(after_zero.arena_allocs, before.arena_allocs + 3);
        assert_eq!(after_zero.arena_words, after_two.arena_words);
    }

    #[test]
    fn the_bif_pool_reports_every_region_it_holds() {
        let mut rt = runtime();
        let first = rt
            .take_bif_heap()
            .expect("the pool hands out its seed region");
        let second = rt
            .take_bif_heap()
            .expect("an emptied pool allocates a fresh region");
        // Both regions are out of the pool now: the figures follow the pool, not the callers.
        let taken = rt.retention_stats();
        assert_eq!(taken.bif_regions, 0);
        assert_eq!(taken.bif_words, 0);
        rt.put_bif_heap(first);
        rt.put_bif_heap(second);
        let returned = rt.retention_stats();
        assert_eq!(returned.bif_regions, 2);
        assert_eq!(
            returned.bif_words,
            rt.bif_heaps
                .iter()
                .map(|heap| heap.capacity() as u64)
                .sum::<u64>()
        );
    }

    /// **The outermost region stays full size; a nested one is seeded from need.**
    ///
    /// The depth rule the fix rests on: [`Runtime::take_bif_heap`] reads the loan count *before*
    /// counting its own loan, so the first heap of a chain is [`Runtime::bif_heap_words`] and every
    /// later one is [`Runtime::bif_nested_heap_words`]. Both are read straight out of the heap that
    /// was actually handed out, so a change to either policy fails here.
    #[test]
    fn the_outermost_region_is_full_size_and_a_nested_one_is_seeded_small() {
        let mut rt = runtime();
        let outermost = rt.take_bif_heap().expect("the seed region");
        assert_eq!(
            outermost.capacity(),
            rt.bif_heap_words,
            "the one-BIF-builds-a-huge-term shape keeps its full region"
        );
        assert_eq!(rt.retention_stats().bif_heaps_taken, 1);
        let nested = rt.take_bif_heap().expect("a nested call's own region");
        assert_eq!(
            (nested.capacity(), rt.bif_nested_heap_words),
            (NESTED_BIF_HEAP_SEED_WORDS, NESTED_BIF_HEAP_SEED_WORDS),
            "a nested call is seeded from the measured need, not from heap_words"
        );
        assert!(
            nested.capacity() < rt.bif_heap_words,
            "the seed must be smaller than the outermost region for this to be a bound at all"
        );
        assert_eq!(rt.retention_stats().bif_heaps_taken, 2);
        rt.put_bif_heap(nested);
        rt.put_bif_heap(outermost);
        assert_eq!(
            rt.retention_stats().bif_heaps_taken,
            0,
            "the loan count is back to `0` — the pool holds every region again"
        );
    }

    /// **200 nested BIF-heap loans stay bounded by the seed, not by `heap_words`.**
    ///
    /// This is the regression pin for the retention defect the depth rule fixes: the pool is a
    /// stack of never-freed regions, one per concurrent call level, so before the rule its
    /// retention was `nesting depth × bif_heap_words` — 27 regions of `1 << 24` words = 3.62 GB for
    /// one in-process `compile:file("cerl_clauses.erl")` (a 13.8 KB source), which is what made
    /// an in-process compile reserve gigabytes for a few hundred KiB of real need. The depth is
    /// real and is *not* bounded here (one region per level is by design), and neither is the
    /// outermost region — what the rule bounds is the **capacity a nested level may reserve for
    /// itself**, which is why the bound asserted below is `outermost + N × seed` and not
    /// `N × heap_words`.
    ///
    /// The synthetic chain is the real nesting path (`take_bif_heap`/`put_bif_heap` at increasing
    /// depth) with the deepest nesting the compiler really reaches:
    /// `cerl_clauses`/`orddict` each nest ~23–27 levels and their nested regions peak at 1 550 and
    /// 2 841 words, i.e. two orders of magnitude below the seed.
    #[test]
    fn two_hundred_nested_bif_heap_loans_stay_bounded_by_the_seed() {
        const DEPTH: usize = 200;
        let mut rt = runtime();
        let heap_words = rt.bif_heap_words;
        let seed = rt.bif_nested_heap_words;
        let mut loans = Vec::with_capacity(DEPTH);
        for level in 0..DEPTH {
            let heap = rt
                .take_bif_heap()
                .unwrap_or_else(|e| panic!("level {level}: a region: {e}"));
            assert_eq!(
                rt.retention_stats().bif_heaps_taken,
                level + 1,
                "level {level}: the loan count is the nesting depth"
            );
            if level > 0 {
                assert_eq!(
                    heap.capacity(),
                    seed,
                    "level {level}: every nested level is seeded from the same rule"
                );
            }
            loans.push(heap);
        }
        while let Some(heap) = loans.pop() {
            rt.put_bif_heap(heap);
        }

        let stats = rt.retention_stats();
        assert_eq!(
            (stats.bif_regions, stats.bif_heaps_taken),
            (DEPTH, 0),
            "one region per level — the count is the depth, and the pool holds them all again"
        );
        assert_eq!(
            stats.bif_max_capacity_words, heap_words as u64,
            "the outermost region is still exactly `bif_heap_words`"
        );
        assert_eq!(
            heap_words, crate::config::DEFAULT_HEAP_WORDS,
            "the configured region is unchanged by the depth rule"
        );
        // The bound: the one outermost region plus one seed per nested level. Nothing about the
        // workload was measured here, so no seed grew, and the figures are exact rather than `<`.
        assert_eq!(
            stats.bif_words,
            (heap_words + (DEPTH - 1) * seed) as u64,
            "the pool holds the outermost region plus one seed per nested level"
        );
        // …and that is the point: the same depth under the old `heap_words`-per-level rule would
        // have reserved `DEPTH × heap_words`, ~128× more.
        assert!(
            stats.bif_words * 64 < (DEPTH as u64) * (heap_words as u64),
            "200 nested levels must cost far less than 200 full-size regions: {} words against {}",
            stats.bif_words,
            (DEPTH as u64) * (heap_words as u64)
        );
    }

    /// **A nested region that really needs more raises the seed; nothing ever lowers it.**
    ///
    /// The self-tuning half of the rule: [`Runtime::put_bif_heap`] raises
    /// [`Runtime::bif_nested_heap_words`] to twice the highest `peak_used()` a nested region ever
    /// reached (capped at [`Runtime::bif_heap_words`]), so a workload whose nested calls are large
    /// pays one growth step and is served from then on. It is a `max`, so a later small nested call
    /// cannot shrink the seed back and re-introduce the growth.
    #[test]
    fn a_large_nested_need_raises_the_seed_and_a_small_one_never_lowers_it() {
        let mut rt = runtime();
        let seeded = rt.bif_nested_heap_words;
        let outer = rt.take_bif_heap().expect("the outermost region");
        let mut nested = rt.take_bif_heap().expect("a nested region");
        // Write 70 000 words into the nested region: a `tuple/1` of that arity, so `used` — and
        // therefore `peak_used` — really reaches the figure the tuning reads.
        let elements = vec![beam_term::make_small(0); 70_000];
        nested
            .tuple(&elements)
            .expect("70 000 elements fit the nested seed");
        rt.put_bif_heap(nested);
        rt.put_bif_heap(outer);
        assert_eq!(
            rt.retention_stats().bif_nested_peak_words,
            70_001,
            "the peak is the written word count (the tuple's own header included)"
        );
        assert_eq!(
            rt.retention_stats().bif_nested_heap_words,
            140_002,
            "the seed is twice the observed nested need ({seeded} was too small to serve it twice)"
        );
        // A second round with a small nested need must not lower it.
        let outer = rt.take_bif_heap().expect("the outermost region");
        let nested = rt.take_bif_heap().expect("a nested region");
        rt.put_bif_heap(nested);
        rt.put_bif_heap(outer);
        assert_eq!(
            rt.retention_stats().bif_nested_heap_words,
            140_002,
            "the seed is a `max`: a small nested call cannot shrink it"
        );
    }

    /// **The store's doubling chain is clamped by its one absolute ceiling, and a refusal retires
    /// nothing.**
    ///
    /// [`Runtime::grow_eval_heap`] doubles by region retirement, so an unclamped chain retains
    /// twice its final region. [`MAX_EVAL_HEAP_WORDS`] is that chain's only clamp: `needed` is a
    /// floor only up to it, and a request beyond it is the **typed** allocation failure a failed
    /// `Heap::new` reports (never a truncated copy), leaving the store exactly as it was — same
    /// region, same retired list.
    ///
    /// A configured `+hmax` is deliberately **not** the ceiling (asserted first): it is a
    /// *process-heap* limit, and a store that could not outgrow one pooled region would fail the
    /// exact workload it exists for (`context/pool/tests.rs` pins that end to end). The ceiling
    /// itself is exercised arithmetically — asking for one word past it is refused before any
    /// region is allocated, so no 256 MiB region is ever created here.
    #[test]
    fn growing_the_eval_store_past_its_ceiling_is_a_typed_failure_that_retires_nothing() {
        const ROUND_WORDS: usize = 2_048;
        const CONFIGURED_HMAX: usize = 8_192;
        let mut rt = Runtime::new(
            crate::config::RuntimeConfig::new()
                .with_heap_words(ROUND_WORDS)
                .with_heap_options(
                    crate::config::HeapOptions::new().with_max_heap_words(CONFIGURED_HMAX),
                ),
        )
        .expect("a runtime whose `+hmax` sizes its pooled BIF heap");
        assert_eq!(
            rt.eval_heap_ceiling(),
            crate::runtime::MAX_EVAL_HEAP_WORDS,
            "`+hmax` ({CONFIGURED_HMAX} words) must not narrow the store's ceiling"
        );
        rt.grow_eval_heap(ROUND_WORDS * 2)
            .expect("a doubling inside the ceiling");
        let grown = rt.retention_stats();
        assert_eq!(grown.eval_heap_words, (ROUND_WORDS * 2) as u64);
        assert_eq!(grown.eval_heap_retired_regions, 1);
        // One word past the absolute ceiling: the clamp cannot serve it.
        let refused = rt
            .grow_eval_heap(crate::runtime::MAX_EVAL_HEAP_WORDS + 1)
            .expect_err("a request past the ceiling cannot be served");
        let exhausted = crate::error::enforce_term_heap_limit(refused);
        assert!(
            matches!(exhausted, beam_common::BeamError::SystemLimit),
            "the refusal is the documented typed exhaustion, not a silently truncated copy: \
             {exhausted:?}"
        );
        assert_eq!(
            rt.retention_stats(),
            grown,
            "a refused growth leaves the store exactly as it was"
        );
    }

    #[test]
    fn growing_the_eval_store_moves_capacity_into_the_retired_figures() {
        let mut rt = runtime();
        let before = rt.retention_stats();
        // Ask for more than the current region can take: `grow_eval_heap` replaces it with a
        // doubled region and keeps the old one, never freeing it.
        let needed = rt.eval_heap.available() + 1;
        rt.grow_eval_heap(needed).expect("the store grows");
        let after = rt.retention_stats();
        assert_eq!(after.eval_heap_retired_regions, 1);
        assert_eq!(
            after.eval_heap_retired_words, before.eval_heap_words,
            "the retired figure is exactly the region that was replaced"
        );
        assert!(
            after.eval_heap_words >= before.eval_heap_words.saturating_mul(2),
            "the replacement region is the old capacity doubled ({} -> {})",
            before.eval_heap_words,
            after.eval_heap_words
        );
        // A store with room to spare is left alone, so nothing new is retired.
        rt.grow_eval_heap(1)
            .expect("an available region needs no growth");
        assert_eq!(rt.retention_stats(), after);
    }

    #[test]
    fn the_off_heap_handle_count_is_the_mso_slot_count() {
        let mut rt = runtime();
        assert_eq!(rt.retention_stats().eval_off_heap_handles, 0);
        // The documented trap: `OffHeapTable::len` is MSO-only, so the refc-binary half of the
        // table is invisible to this figure. The field's doc says so; assert it here so the
        // limitation cannot quietly become a lie.
        let payload = beam_alloc::RefcBinary::new(&[0u8; 8]).expect("an 8-byte payload");
        rt.eval_off_heap.push_binary(payload);
        assert_eq!(rt.eval_off_heap.binaries().len(), 1);
        assert_eq!(rt.retention_stats().eval_off_heap_handles, 0);
    }

    #[test]
    fn the_display_form_labels_every_field_on_one_line() {
        let stats = RetentionStats {
            arena_words: 1,
            arena_allocs: 2,
            bif_regions: 3,
            bif_words: 4,
            bif_peak_used_words: 9,
            bif_max_capacity_words: 10,
            bif_heaps_taken: 11,
            bif_nested_heap_words: 12,
            bif_nested_peak_words: 13,
            eval_heap_words: 5,
            eval_heap_retired_regions: 6,
            eval_heap_retired_words: 7,
            eval_off_heap_handles: 8,
            store_compactions: 14,
            store_words_reclaimed: 15,
            store_regions_dropped: 16,
            store_move_declines: 21,
            store_move_last_decline: Some("a move reason"),
            arena_reclaims: 17,
            arena_words_reclaimed: 18,
            arena_regions_dropped: 19,
            reclaim_declines: 20,
            reclaim_last_decline: Some("a reason"),
        };
        let line = stats.to_string();
        assert!(!line.contains('\n'), "the diagnostic is one line: {line}");
        for field in [
            "arena_words=1",
            "arena_allocs=2",
            "bif_regions=3",
            "bif_words=4",
            "eval_heap_words=5",
            "eval_heap_retired_regions=6",
            "eval_heap_retired_words=7",
            "eval_off_heap_handles=8",
            "store_compactions=14",
            "store_words_reclaimed=15",
            "store_regions_dropped=16",
            "store_move_declines=21",
            "store_move_last_decline=a move reason",
            "arena_reclaims=17",
            "arena_words_reclaimed=18",
            "arena_regions_dropped=19",
            "reclaim_declines=20",
            "reclaim_last_decline=a reason",
        ] {
            assert!(
                line.contains(field),
                "the line must carry `{field}`: {line}"
            );
        }
        // A pass that has never declined renders the documented `none`, so the pair is always
        // present and a script can grep it unconditionally — for both decline reasons.
        let fresh = RetentionStats::default().to_string();
        assert!(fresh.ends_with("reclaim_last_decline=none"), "{fresh}");
        assert!(fresh.contains("store_move_last_decline=none"), "{fresh}");
    }
}
