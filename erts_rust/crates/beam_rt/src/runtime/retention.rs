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

//! **Retention accounting**: one snapshot of the runtime's four never-freed storage regions.
//!
//! `beam_rt` holds four regions that are allocated but never released for the runtime's
//! lifetime, and until this module existed not one of them was observable from outside:
//!
//! 1. the **arena** — [`Runtime::arena`], the engine's `MinimalContext` whose `ContextHeap` keeps
//!    a `Vec<ReservedHeap>` that only ever grows (`beam_engine/src/context.rs`); on the
//!    `-eval`/table-less route every term and every nested `Machine`'s heap/stack is allocated
//!    there, through [`Runtime::alloc_words`];
//! 2. the **pooled BIF heap** — [`Runtime::bif_heaps`], where a growing call *retires* its region
//!    back into the pool (`crate::context::calls`'s `take_bif_frontier`) instead of reusing it,
//!    so the pool accumulates regions for the runtime's life;
//! 3. the **`-eval` term store** — [`Runtime::eval_heap`] plus the regions
//!    [`Runtime::grow_eval_heap`] replaced into [`Runtime::eval_heap_retired`] (the growth doubles
//!    the capacity and never frees the old region, because a `beam_alloc::Heap` never moves a
//!    term);
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
}

impl fmt::Display for RetentionStats {
    /// Renders the snapshot as **one** line of `label=value` pairs, in field order — the shape
    /// `BEAM_RT_MEM_TRACE` prints, and stable enough for a script to grep a single figure out of.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "arena_words={} arena_allocs={} bif_regions={} bif_words={} eval_heap_words={} \
             eval_heap_retired_regions={} eval_heap_retired_words={} eval_off_heap_handles={}",
            self.arena_words,
            self.arena_allocs,
            self.bif_regions,
            self.bif_words,
            self.eval_heap_words,
            self.eval_heap_retired_regions,
            self.eval_heap_retired_words,
            self.eval_off_heap_handles,
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
        RetentionStats {
            arena_words: self.arena_alloc_words,
            arena_allocs: self.arena_allocs,
            bif_regions: self.bif_heaps.len(),
            bif_words: self
                .bif_heaps
                .iter()
                .map(|heap| heap.capacity() as u64)
                .sum(),
            eval_heap_words: self.eval_heap.capacity() as u64,
            eval_heap_retired_regions: self.eval_heap_retired.len(),
            eval_heap_retired_words: self
                .eval_heap_retired
                .iter()
                .map(|heap| heap.capacity() as u64)
                .sum(),
            eval_off_heap_handles: self.eval_off_heap.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            eval_heap_words: 5,
            eval_heap_retired_regions: 6,
            eval_heap_retired_words: 7,
            eval_off_heap_handles: 8,
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
        ] {
            assert!(
                line.contains(field),
                "the line must carry `{field}`: {line}"
            );
        }
    }
}
