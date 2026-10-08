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

//! Heap observables in the shape of `erlang:process_info(Pid, garbage_collection)`
//! (the `ERTS_PI_IX_GARBAGE_COLLECTION` item, `erts/emulator/beam/erl_bif_info.c:2073`):
//! `minor_gcs`, `fullsweep_after`, `min_heap_size`, `min_bin_vheap_size`, `max_heap_size`,
//! plus the derived `heap_size`/`total_heap_size` the runtime reports separately
//! (`ERTS_PI_IX_HEAP_SIZE` / `ERTS_PI_IX_TOTAL_HEAP_SIZE`, `erl_bif_info.c:1983,2022`).
//!
//! The oracle's item is a proplist whose emitted order is `max_heap_size`,
//! `min_bin_vheap_size`, `min_heap_size`, `fullsweep_after`, `minor_gcs` — the reverse of
//! the order the C builds it in, because each `CONS` prepends. [`HeapInfo::items`] emits
//! exactly that order.
//!
//! # What `heap_size` / `total_heap_size` count in the two-space scheme
//!
//! ERTS reports `heap_size = HEAP_SIZE(rp)` (the allocated young+stack heap, `rp->heap_sz`)
//! and `total_heap_size = heap_sz + (old_hend - old_heap) + mbuf + on-heap messages`
//! (`erl_bif_info.c:2022-2042`). This crate's heap is a **copying nursery** plus a
//! **mature generation** (and no message buffer — the mailbox lives in `beam_process`), so
//! the mapping is:
//!
//! * `heap_size` = the nursery's **capacity** in words — the "heap" in the C sense, i.e.
//!   how many words the process may currently allocate before the next collection;
//! * `total_heap_size` = `heap_size` **plus the mature generation's capacity** — the C's
//!   `heap_sz + old heap`, since the mature space is this scheme's old generation.
//!
//! Words, not bytes: the oracle's numbers are word counts too.
//!
//! This module is safe Rust and inspects no term interior.

use crate::policy::HeapPolicy;

/// The `garbage_collection` observables of one live collector — the values a
/// `process_info(Pid, garbage_collection)` reply and the heap-size fields of
/// `system_info(garbage_collection)` are built from.
///
/// Field order follows the objective's set; [`items`](HeapInfo::items) emits the *oracle's*
/// list order. `max_heap_size` is carried as the raw word bound (0 = unlimited); the
/// oracle wraps it in a map of flags (`#{size, kill, error_logger, include_shared_binaries}`,
/// `erts_max_heap_size_map`), which this crate has no process policy for yet, so the map
/// shape is deliberately left to `beam_rt`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapInfo {
    /// Minor collections run since the last fullsweep (the C `GEN_GCS`).
    pub minor_gcs: u32,
    /// Minor collections after which a fullsweep is forced (the C `max_gen_gcs`).
    pub fullsweep_after: u32,
    /// The collector's minimum heap size, in words (the C `min_heap_size`).
    pub min_heap_size: usize,
    /// The collector's minimum binary virtual heap size, in words (the C
    /// `min_bin_vheap_size`).
    pub min_bin_vheap_size: usize,
    /// The collector's maximum heap size, in words (0 = unbounded; the C
    /// `max_heap_size`).
    pub max_heap_size: usize,
    /// The nursery's capacity in words — the process's current usable heap.
    pub heap_size: usize,
    /// `heap_size` plus the mature generation's capacity, in words.
    pub total_heap_size: usize,
}

/// The observables a collector must expose for [`HeapInfo::of`].
///
/// Implemented by [`Collector`](crate::Collector),
/// [`GenImmixCollector`](crate::GenImmixCollector) and
/// [`AnyCollector`](crate::AnyCollector), so `HeapInfo::of` works uniformly over the
/// configured strategy.
pub trait HeapObservable {
    /// Minor collections run since the last fullsweep (see [`HeapInfo::minor_gcs`]).
    fn minor_gcs(&self) -> u32;
    /// The current usable heap in words (see [`HeapInfo::heap_size`]).
    fn heap_size_words(&self) -> usize;
    /// The total heap in words (see [`HeapInfo::total_heap_size`]).
    fn total_heap_size_words(&self) -> usize;
    /// The heap-size policy whose bounds the item reports.
    fn heap_policy(&self) -> &HeapPolicy;
}

/// The item's keys in the order the oracle emits them (`erl_bif_info.c:2082-2093`).
pub const GARBAGE_COLLECTION_KEYS: [&str; 5] = [
    "max_heap_size",
    "min_bin_vheap_size",
    "min_heap_size",
    "fullsweep_after",
    "minor_gcs",
];

impl HeapInfo {
    /// Builds the item from its parts.
    pub fn new(
        policy: &HeapPolicy,
        minor_gcs: u32,
        heap_size: usize,
        total_heap_size: usize,
    ) -> HeapInfo {
        HeapInfo {
            minor_gcs,
            fullsweep_after: policy.fullsweep_after,
            min_heap_size: policy.min_heap_words,
            min_bin_vheap_size: policy.min_bin_vheap_words,
            max_heap_size: policy.max_heap_words,
            heap_size,
            total_heap_size,
        }
    }

    /// Derives the item from any [`HeapObservable`] collector — its `minor_gcs` counter,
    /// its young/mature capacities and its [`HeapPolicy`].
    pub fn of<C: HeapObservable + ?Sized>(collector: &C) -> HeapInfo {
        HeapInfo::new(
            collector.heap_policy(),
            collector.minor_gcs(),
            collector.heap_size_words(),
            collector.total_heap_size_words(),
        )
    }

    /// The item's values, in [`GARBAGE_COLLECTION_KEYS`] order
    /// (`{max_heap_size, min_bin_vheap_size, min_heap_size, fullsweep_after, minor_gcs}`).
    pub fn values(&self) -> [usize; 5] {
        [
            self.max_heap_size,
            self.min_bin_vheap_size,
            self.min_heap_size,
            self.fullsweep_after as usize,
            self.minor_gcs as usize,
        ]
    }

    /// The item as the oracle emits it: `{max_heap_size, min_bin_vheap_size,
    /// min_heap_size, fullsweep_after, minor_gcs}` in that order
    /// (`erl_bif_info.c:2082-2093`).
    pub fn items(&self) -> [(&'static str, usize); 5] {
        let values = self.values();
        let [a, b, c, d, e] = GARBAGE_COLLECTION_KEYS;
        [(a, values[0]), (b, values[1]), (c, values[2]), (d, values[3]), (e, values[4])]
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_takes_the_policy_bounds_and_the_given_sizes() {
        let policy = HeapPolicy {
            min_heap_words: 233,
            max_heap_words: 1_000_000,
            fullsweep_after: 7,
            min_bin_vheap_words: 46_422,
            ..HeapPolicy::default()
        };
        let info = HeapInfo::new(&policy, 3, 610, 1_598);
        assert_eq!(info.minor_gcs, 3);
        assert_eq!(info.fullsweep_after, 7);
        assert_eq!(info.min_heap_size, 233);
        assert_eq!(info.min_bin_vheap_size, 46_422);
        assert_eq!(info.max_heap_size, 1_000_000);
        assert_eq!(info.heap_size, 610);
        assert_eq!(info.total_heap_size, 1_598);
    }

    #[test]
    fn items_follow_the_oracle_order() {
        let policy = HeapPolicy::default();
        let info = HeapInfo::new(&policy, 4, 233, 466);
        assert_eq!(
            info.items(),
            [
                ("max_heap_size", 0),
                ("min_bin_vheap_size", 46_422),
                ("min_heap_size", 0),
                ("fullsweep_after", 65_535),
                ("minor_gcs", 4),
            ],
            "the emitted order is the reverse of the C's CONS order"
        );
        assert_eq!(
            GARBAGE_COLLECTION_KEYS,
            [
                "max_heap_size",
                "min_bin_vheap_size",
                "min_heap_size",
                "fullsweep_after",
                "minor_gcs"
            ]
        );
        assert_eq!(info.values(), [0, 46_422, 0, 65_535, 4]);
    }
    #[test]
    fn a_default_policy_reports_the_beam_default_fullsweep_and_vheap() {
        let info = HeapInfo::new(&HeapPolicy::default(), 0, 0, 0);
        assert_eq!(info.fullsweep_after, 65_535);
        assert_eq!(info.min_bin_vheap_size, 46_422);
        // `HeapPolicy::default()` is the *unbounded* floor, so the item reports 0.
        assert_eq!(info.min_heap_size, 0);
        assert_eq!(info.max_heap_size, 0);
    }
}
