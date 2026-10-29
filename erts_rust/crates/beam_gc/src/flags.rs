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

//! The **live heap-sizing-flag** seam: the three `process_flag/2` heap flags applied to a
//! collector's [`HeapPolicy`] in place.
//!
//! A process's heap-size policy is built once, at spawn (the `spawn_opt` heap options through
//! [`HeapPolicy::spawn_policy`]), and every sizing decision reads it: the young-capacity rules
//! ([`HeapPolicy::young_minor_target_words`] / [`HeapPolicy::young_target_words`] and the two
//! collapse rules that consume them), the fullsweep cadence
//! ([`HeapPolicy::fullsweep_due`], consulted first by every
//! [`Collector::collect`](crate::Collector::collect) /
//! [`GenImmixCollector::collect`](crate::GenImmixCollector::collect)), and the binary
//! virtual-heap floor (`BinVheap::new` / `BinVheap::record_collection`). `process_flag/2` can
//! change three of those fields **after** the fact, and this module is that write path — the
//! only one in the crate, since nothing else mutates a policy after construction.
//!
//! # The three flags, and where each one lands
//!
//! * **`min_heap_size`** → [`HeapPolicy::min_heap_words`], the floor every size passes through
//!   ([`HeapPolicy::next_words`]). It lands on the **young capacity at the next collection**:
//!   for a live set small enough to be floored, that collection installs exactly
//!   `next_heap_size(request, 0)` (the oracle: `process_flag(min_heap_size, 100000)` reports
//!   `heap_size` 121536 on the next collection, `200000` → 318187, `500000` → 514838). It does
//!   **not** grow the heap at the call and does **not** force a collection.
//! * **`min_bin_vheap_size`** → [`HeapPolicy::min_bin_vheap_words`], the floor of
//!   `next_vheap_size` (`erl_gc.c:2955-2959`). It lands at the **next**
//!   [`BinVheap::record_collection`](crate::BinVheap::record_collection) — the C reads
//!   `p->min_vheap_size` only there and in the launch assignment, so a raise does not
//!   retroactively lift the **current** `bin_vheap_sz` (`erl_gc.c:2958`, `:3288`).
//! * **`fullsweep_after`** → [`HeapPolicy::fullsweep_after`], the cadence
//!   [`HeapPolicy::fullsweep_due`] compares against the live `minor_gcs`. It lands at the
//!   **next collection** (`0` makes every collection a fullsweep, `3` gives three minors and
//!   then one), and it never resets the counter.
//!
//! # The deferred model — what a setter is, and what it is not
//!
//! Every entry point here is **a policy write and nothing else**, exactly like the C arm it
//! mirrors (`erts/emulator/beam/bif.c:2101-2142`: read `old_value`, assign the field, return).
//! A call runs **no collection**, resets **no** counter (`minor_gcs` included), allocates,
//! frees, grows, shrinks and retires **no** space, moves no term, and leaves every policy field
//! other than the named one alone. The consequence is the observable model the oracle exhibits:
//! the flag's effect appears at the *next* collection (young capacity, fullsweep cadence) or at
//! the next binary-vheap refit (binary budget), never at the setter.
//!
//! The rounding is the **caller's** — `erts_next_heap_size(i, 0)` plus the `H_MIN_SIZE` /
//! `BIN_VH_MIN_SIZE` floor for `process_flag/2`, and the `Uint16` narrowing for
//! `fullsweep_after` — so the value the program reads back from the flag and the value that
//! sizes the heap are the same number by construction. The [`HeapPolicy`] setters store
//! verbatim and return the value they replaced, which is what lets a host answer the flag's own
//! read-back from one call.
//!
//! This module owns no collection, sizing or accounting logic: each method is a two-line
//! forward, so the rules, the thresholds and the defaults stay where they are.
//! `tests/heap_flags.rs` is the out-of-crate acceptance suite.

use crate::any_collector::AnyCollector;
use crate::collector::Collector;
use crate::immix_collector::GenImmixCollector;

impl Collector {
    /// Applies `process_flag(min_heap_size, N)` to this collector's live policy, returning the
    /// previous minimum (in words).
    ///
    /// The value is stored **verbatim** — the caller owns `process_flag/2`'s rounding
    /// ([`HeapPolicy::set_min_heap_words`]) — and it is a **policy write, nothing else**: no
    /// collection runs, `minor_gcs` is not reset, no space is allocated, freed, resized or
    /// retired, no term moves, and no policy field other than `min_heap_words` changes. The new
    /// floor reaches the young capacity at the **next** collection.
    pub fn set_min_heap_words(&mut self, words: usize) -> usize {
        self.policy_mut().set_min_heap_words(words)
    }

    /// Applies `process_flag(min_bin_vheap_size, N)` to this collector's live policy, returning
    /// the previous minimum binary virtual heap size (in words).
    ///
    /// As pure a policy write as [`set_min_heap_words`](Self::set_min_heap_words): the effect
    /// lands at the next [`BinVheap::record_collection`](crate::BinVheap::record_collection),
    /// and a live budget is not lifted retroactively.
    pub fn set_min_bin_vheap_words(&mut self, words: usize) -> usize {
        self.policy_mut().set_min_bin_vheap_words(words)
    }

    /// Applies `process_flag(fullsweep_after, N)` to this collector's live policy, returning the
    /// previous cadence.
    ///
    /// As pure a policy write as [`set_min_heap_words`](Self::set_min_heap_words): the effect
    /// lands at the next collection, whose kind [`HeapPolicy::fullsweep_due`] decides — and
    /// `minor_gcs` is **not** reset, so a cadence at or below the live counter makes that next
    /// collection a fullsweep.
    pub fn set_fullsweep_after(&mut self, after: u32) -> u32 {
        self.policy_mut().set_fullsweep_after(after)
    }
}

impl GenImmixCollector {
    /// Applies `process_flag(min_heap_size, N)` to this collector's live policy, returning the
    /// previous minimum (in words).
    ///
    /// The mark-region twin of [`Collector::set_min_heap_words`], with the identical contract: a
    /// policy write and nothing else, whose floor reaches the shared nursery's capacity at the
    /// next collection. It does **not** resize the mark-region mature generation — no flag does;
    /// only a fullsweep sizes that region.
    pub fn set_min_heap_words(&mut self, words: usize) -> usize {
        self.policy_mut().set_min_heap_words(words)
    }

    /// Applies `process_flag(min_bin_vheap_size, N)` to this collector's live policy, returning
    /// the previous minimum binary virtual heap size (in words). See
    /// [`Collector::set_min_bin_vheap_words`].
    pub fn set_min_bin_vheap_words(&mut self, words: usize) -> usize {
        self.policy_mut().set_min_bin_vheap_words(words)
    }

    /// Applies `process_flag(fullsweep_after, N)` to this collector's live policy, returning the
    /// previous cadence. See [`Collector::set_fullsweep_after`].
    pub fn set_fullsweep_after(&mut self, after: u32) -> u32 {
        self.policy_mut().set_fullsweep_after(after)
    }
}

impl AnyCollector {
    /// Applies `process_flag(min_heap_size, N)` to whichever strategy this collector selected,
    /// returning the previous minimum (in words).
    ///
    /// Forwarded to the selected variant, exactly as the other [`AnyCollector`] accessors are, so
    /// the flag is strategy-independent: both strategies read the one shared nursery and size it
    /// from the one [`HeapPolicy`], so the same write lands on the same capacity.
    pub fn set_min_heap_words(&mut self, words: usize) -> usize {
        match self {
            AnyCollector::Copying(c) => c.set_min_heap_words(words),
            AnyCollector::Immix(c) => c.set_min_heap_words(words),
        }
    }

    /// Applies `process_flag(min_bin_vheap_size, N)` to whichever strategy this collector
    /// selected, returning the previous minimum binary virtual heap size (in words). See
    /// [`Collector::set_min_bin_vheap_words`].
    pub fn set_min_bin_vheap_words(&mut self, words: usize) -> usize {
        match self {
            AnyCollector::Copying(c) => c.set_min_bin_vheap_words(words),
            AnyCollector::Immix(c) => c.set_min_bin_vheap_words(words),
        }
    }

    /// Applies `process_flag(fullsweep_after, N)` to whichever strategy this collector selected,
    /// returning the previous cadence. See [`Collector::set_fullsweep_after`].
    pub fn set_fullsweep_after(&mut self, after: u32) -> u32 {
        match self {
            AnyCollector::Copying(c) => c.set_fullsweep_after(after),
            AnyCollector::Immix(c) => c.set_fullsweep_after(after),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::config::{GcConfig, MatureKind};
    use crate::heap_info::HeapObservable;
    use crate::policy::{DEFAULT_MIN_HEAP_WORDS, HeapPolicy};

    /// A policy with a mature generation roomy enough that a `collect` is the kind the cadence
    /// chose, never an escalation these cases did not intend.
    fn roomy_policy() -> HeapPolicy {
        HeapPolicy {
            initial_mature_words: 32_768,
            ..HeapPolicy::default()
        }
    }

    fn collector(kind: MatureKind, policy: HeapPolicy) -> AnyCollector {
        let config = match kind {
            MatureKind::Copying => GcConfig::copying(policy),
            MatureKind::Immix => GcConfig::immix(policy),
        };
        AnyCollector::from_config(&config).expect("collector")
    }

    #[test]
    fn the_policy_setters_store_verbatim_and_return_the_previous_value() {
        let mut policy = HeapPolicy::default();
        // `HeapPolicy::default`'s three figures are the C's: no floor, the `BIN_VH_MIN_SIZE`
        // rounding of `VH_DEFAULT_SIZE`, and the `(Uint16) -1` cadence.
        let default = HeapPolicy::default();
        assert_eq!(policy.min_heap_words, 0);
        assert_eq!(policy.min_bin_vheap_words, default.min_bin_vheap_words);
        assert_eq!(policy.fullsweep_after, 65_535);

        // Verbatim: an unrounded value is stored as given (no growth-table step, no floor), and
        // the previous value is reported.
        assert_eq!(policy.set_min_heap_words(100_000), 0);
        assert_eq!(policy.min_heap_words, 100_000);
        assert_eq!(policy.set_min_bin_vheap_words(7), default.min_bin_vheap_words);
        assert_eq!(policy.min_bin_vheap_words, 7);
        assert_eq!(policy.set_fullsweep_after(3), 65_535);
        assert_eq!(policy.fullsweep_after, 3);
        // The setters touch nothing else.
        assert_eq!(policy.max_heap_words, 0);
        assert_eq!(policy.initial_young_words, 233);
        assert_eq!(policy.need_reserve_words, 64);

        // A second write reports the first one back.
        assert_eq!(policy.set_min_heap_words(DEFAULT_MIN_HEAP_WORDS), 100_000);
        assert_eq!(policy.min_heap_words, DEFAULT_MIN_HEAP_WORDS);
    }

    #[test]
    fn the_fullsweep_setter_moves_the_cadence_only() {
        let mut policy = HeapPolicy::default();
        // The default cadence is never due for any counter a process reaches.
        assert!(!policy.fullsweep_due(65_534));

        // 0 is due immediately, i.e. every collect is a fullsweep — the C's `0 >= 0`.
        assert_eq!(policy.set_fullsweep_after(0), 65_535);
        assert!(policy.fullsweep_due(0));

        // 3 gives three minors and then a fullsweep; a setter does not reset a live counter.
        policy.set_fullsweep_after(3);
        assert!(!policy.fullsweep_due(2));
        assert!(policy.fullsweep_due(3));
    }

    #[test]
    fn the_collector_setters_forward_to_the_live_policy_and_move_nothing_else() {
        for kind in [MatureKind::Copying, MatureKind::Immix] {
            let mut c = collector(kind, roomy_policy());
            // One commit so the counters and the spaces are non-trivial before the writes.
            let term = c
                .young_mut()
                .heap_mut()
                .tuple(&[beam_term::prelude::make_small(1)])
                .expect("a 2-word nursery tuple");
            let mut roots = vec![term];
            c.collect(&mut roots).expect("the first collection");

            let heap = c.heap_size_words();
            let total = c.total_heap_size_words();
            let minors = c.minor_gcs();
            let stats = c.stats();
            let spaces = c.young_space_count();

            assert_eq!(c.set_min_heap_words(1_000), 0);
            assert_eq!(c.set_min_bin_vheap_words(1_000), 46_422);
            assert_eq!(c.set_fullsweep_after(7), 65_535);

            // The policy is exactly the old one with the three named fields replaced ...
            assert_eq!(
                *c.policy(),
                HeapPolicy {
                    min_heap_words: 1_000,
                    min_bin_vheap_words: 1_000,
                    fullsweep_after: 7,
                    ..roomy_policy()
                },
                "{kind:?}: only the three named fields may move"
            );
            // ... and nothing observable about the heap, the counters or the chain moved.
            assert_eq!(c.heap_size_words(), heap, "{kind:?}: heap_size");
            assert_eq!(c.total_heap_size_words(), total, "{kind:?}: total_heap_size");
            assert_eq!(c.minor_gcs(), minors, "{kind:?}: minor_gcs");
            assert_eq!(c.stats(), stats, "{kind:?}: stats");
            assert_eq!(c.young_space_count(), spaces, "{kind:?}: chain length");
        }
    }
}
