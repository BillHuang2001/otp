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

//! Heap-size policy: [`HeapPolicy`] plus the curated growth table ([`heap_sizes`] /
//! [`next_heap_size`]).
//!
//! Mirrors the C `erts_init_gc` growth table and `erts_next_heap_size`
//! (`erts/emulator/beam/erl_gc.c`): Fibonacci+1 up to index 23, then +20 %, capped at
//! `2^53 / word`. All of this module is safe Rust.

use std::sync::LazyLock;

/// Number of entries the Fibonacci+1 growth stage fills (the C `for (i = 2; i < 23; i++)`
/// bound): table indices `0..FIBONACCI_STAGE_LEN` come from Fibonacci+1, the rest from
/// the +20 % stage.
const FIBONACCI_STAGE_LEN: usize = 23;

/// The C `H_DEFAULT_SIZE` / minimum process heap size, in words (`erl_vm.h:60`,
/// `erl_init.c:812`). `erts_next_heap_size(233, 0)` is 233, so the oracle reports
/// `min_heap_size` as exactly this value.
pub const DEFAULT_MIN_HEAP_WORDS: usize = 233;

/// The C `VH_DEFAULT_SIZE` (`erl_vm.h:61`): the seed for the minimum binary virtual
/// heap size. `erl_init.c:284` rounds it through `erts_next_heap_size(x, 0)`, which is
/// where the oracle's `min_bin_vheap_size` of `46422` comes from.
const VH_DEFAULT_SIZE_WORDS: usize = 32768;

/// The oracle's default `min_bin_vheap_size`: `erts_next_heap_size(32768, 0)`.
///
/// Computed through the growth table rather than hard-coded, so the value can never
/// drift from `erts_init.c:284`; `default_policy_matches_the_beam_defaults` pins it.
fn default_min_bin_vheap_words() -> usize {
    next_heap_size(VH_DEFAULT_SIZE_WORDS, 0)
}

/// The C `spawn_opt` minimum rounding (`erl_process.c:12321-12349`): a request below
/// `floor` is replaced by `floor`, anything else is rounded up through
/// `erts_next_heap_size(x, 0)` so the result is a fixed point of the growth table.
fn round_spawn_min(requested: usize, floor: usize) -> usize {
    if requested < floor {
        floor
    } else {
        next_heap_size(requested, 0)
    }
}

/// The curated heap-size growth table, built once on first use.
static HEAP_SIZES: LazyLock<Vec<usize>> = LazyLock::new(build_heap_sizes);

/// Builds the growth table exactly as `erts_init_gc` does (`erl_gc.c:262-291`).
///
/// ```text
/// 12, 38, 51, 90, 142, 233, ...          (Fibonacci + 1, one word for the block header)
/// ... then +20 % per step, capped at MAX(52-bit) / sizeof(Eterm)
/// ```
fn build_heap_sizes() -> Vec<usize> {
    let mut sizes: Vec<usize> = Vec::with_capacity(FIBONACCI_STAGE_LEN + 128);

    // Growth stage 1 — Fibonacci + 1. `12, 38` hits 233, the old default.
    sizes.push(12);
    sizes.push(38);
    while sizes.len() < FIBONACCI_STAGE_LEN {
        let next = sizes[sizes.len() - 1] + sizes[sizes.len() - 2] + 1;
        sizes.push(next);
    }

    // 64-bit cap: MAX(52bit) / sizeof(Eterm). This crate targets the frozen 64-bit term
    // layout, so the 32-bit `(~0u32)/4` branch of the C is not reachable here.
    let cap: usize = ((1u64 << 53) / 8) as usize;

    // Growth stage 2 — +20 % steps, stopping *before* the entry that would exceed the cap
    // (the C writes the offending size, then discards it and ends the table one shorter).
    while let Some(&last) = sizes.last() {
        let next = last + last / 5;
        if next > cap {
            break;
        }
        sizes.push(next);
    }

    sizes
}

/// The process heap-size policy (the `erts_next_heap_size` analogue).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapPolicy {
    /// Smallest heap the policy will ever size to, in term words. `0` sets **no floor**.
    ///
    /// This is the growth knob's lower bound: whenever the policy computes a target size
    /// (a nursery resize or a fullsweep mature size), [`next_words`](HeapPolicy::next_words)
    /// raises it to at least this value, so growth can never shrink a space below it.
    pub min_heap_words: usize,
    /// Largest heap the policy will ever size to, in term words. **`0` means unbounded
    /// growth** — the crate's "no upper bound" encoding: a collection (or
    /// [`Collector::ensure_capacity`](crate::Collector::ensure_capacity)) may grow the
    /// nursery and the mature generation to whatever the live set requires, up to the
    /// growth table's ceiling. Any nonzero value clamps every size the policy computes, and
    /// is what makes an otherwise-fit collection report the typed heap exhaustion instead.
    pub max_heap_words: usize,
    /// Number of minor collections before a fullsweep is forced (0 = a fullsweep on
    /// every collection). The C `max_gen_gcs` (`erl_process.h:1145`), whose default is
    /// `(Uint16) -1 == 65535` (`erl_init.c:843`).
    pub fullsweep_after: u32,
    /// Smallest binary virtual heap the policy sizes to, in term words — the C
    /// `min_vheap_size` / `BIN_VH_MIN_SIZE`, whose default the oracle reports as 46422.
    pub min_bin_vheap_words: usize,
    /// Initial size of the young (nursery) space, in term words.
    pub initial_young_words: usize,
    /// Initial size of the mature space, in term words.
    pub initial_mature_words: usize,
    /// Slack kept available so a collection's result still fits, in term words.
    pub need_reserve_words: usize,
}

impl HeapPolicy {
    /// The BEAM defaults: `fullsweep_after` 65535, a 233-word initial young space, the
    /// curated minimum heap size and the C default minimum binary virtual heap size
    /// (`min_bin_vheap_size` 46422).
    ///
    /// `min_heap_words` is **0** — *no floor* — rather than the C's 233-word minimum for
    /// a spawned process. A caller that wants the process default uses
    /// [`HeapPolicy::from_spawn_opts`] (which applies the C `H_MIN_SIZE` floor);
    /// `DEFAULT_MIN_HEAP_WORDS` is that value.
    pub fn default() -> HeapPolicy {
        HeapPolicy {
            min_heap_words: 0,
            max_heap_words: 0,
            fullsweep_after: 65535,
            min_bin_vheap_words: default_min_bin_vheap_words(),
            initial_young_words: 233,
            initial_mature_words: 233,
            need_reserve_words: 64,
        }
    }

    /// A policy bounded by `min_words`..=`max_words` (0 = unbounded) with the other
    /// fields at their default values.
    pub fn from_min_max(min_words: usize, max_words: usize) -> HeapPolicy {
        HeapPolicy {
            min_heap_words: min_words,
            max_heap_words: max_words,
            ..HeapPolicy::default()
        }
    }

    /// A policy that permits **unbounded growth** — the growth knob made explicit.
    ///
    /// [`max_heap_words`](Self::max_heap_words) is set to `0`, the crate's "no upper bound"
    /// encoding, so a collection or an on-demand grow
    /// ([`Collector::ensure_capacity`](crate::Collector::ensure_capacity)) may size the
    /// nursery and the mature generation as large as the live set requires (up to the
    /// growth table's ceiling). [`min_heap_words`](Self::min_heap_words) still floors every
    /// size. This is exactly [`HeapPolicy::default`]'s growth behaviour; the constructor
    /// exists so a caller that *wants* unbounded growth can state it rather than rely on the
    /// default.
    pub fn unbounded() -> HeapPolicy {
        HeapPolicy {
            max_heap_words: 0,
            ..HeapPolicy::default()
        }
    }

    /// A policy for a process started with the `spawn_opt` heap options
    /// `min_heap_size`, `min_bin_vheap_size` and `fullsweep_after`, applying the C's
    /// rounding rules (`erl_process.c:12321-12360`).
    ///
    /// `min_heap_words` below the C `H_MIN_SIZE` (`DEFAULT_MIN_HEAP_WORDS`, 233) is
    /// replaced by that minimum rather than rounded up past it; otherwise it is rounded
    /// through `erts_next_heap_size(x, 0)`, so the stored minimum is a fixed point of the
    /// growth table — the C's
    /// `ASSERT(p->min_heap_size == erts_next_heap_size(p->min_heap_size, 0))`
    /// (`erl_process.c:12575`). `min_bin_vheap_words` follows the identical rule against
    /// the C default `BIN_VH_MIN_SIZE`. `fullsweep_after` is stored verbatim (the C only
    /// rejects a negative value).
    pub fn from_spawn_opts(
        min_heap_words: usize,
        min_bin_vheap_words: usize,
        fullsweep_after: u32,
    ) -> HeapPolicy {
        HeapPolicy {
            min_heap_words: round_spawn_min(min_heap_words, DEFAULT_MIN_HEAP_WORDS),
            min_bin_vheap_words: round_spawn_min(
                min_bin_vheap_words,
                default_min_bin_vheap_words(),
            ),
            fullsweep_after,
            ..HeapPolicy::default()
        }
    }

    /// Whether the fullsweep cadence is due after `minor_gcs` minor collections — the C
    /// `GEN_GCS(p) >= MAX_GEN_GCS(p)` test (`erl_gc.c:694,831`).
    ///
    /// A `fullsweep_after` of 0 is due immediately, so *every* collection is a
    /// fullsweep — exactly as in the C, where `0 >= 0`.
    pub fn fullsweep_due(&self, minor_gcs: u32) -> bool {
        minor_gcs >= self.fullsweep_after
    }
    /// The next size (in words) to grow `size` to, `offset` entries further along the
    /// growth table.
    ///
    /// Applies the policy bounds after the table lookup: at least [`Self::min_heap_words`],
    /// and at most [`Self::max_heap_words`] when that is not `0` (unbounded).
    pub fn next_words(&self, size: usize, offset: usize) -> usize {
        let size = next_heap_size(size, offset);
        let size = size.max(self.min_heap_words);
        if self.max_heap_words != 0 {
            size.min(self.max_heap_words)
        } else {
            size
        }
    }

    /// The heap size to use after a fullsweep, from the `live_words` a collection kept
    /// and the `need_words` the mutator still requires.
    ///
    /// `need_reserve_words` of slack is requested on top, so the result is always at least
    /// `live_words + need_words` (and therefore a fullsweep can always fit its result) —
    /// unless `max_heap_words` clamps it down, which is the caller's explicit choice.
    pub fn size_after_fullsweep(&self, live_words: usize, need_words: usize) -> usize {
        self.next_words(live_words + need_words + self.need_reserve_words, 0)
    }

    /// Whether the heap at `heap_words` should be grown to satisfy `need_words`.
    pub fn should_grow(&self, heap_words: usize, need_words: usize) -> bool {
        heap_words < need_words || 3 * heap_words < 4 * need_words
    }

    /// Whether the heap at `heap_words` should be shrunk after keeping `need_words`.
    pub fn should_shrink(&self, heap_words: usize, need_words: usize) -> bool {
        heap_words > 4 * need_words && heap_words > self.min_heap_words
    }

    /// The mature-space size (in words) to use after a fullsweep that kept `live_words`
    /// alive and still needs `need_words` — the C `adjust_after_fullsweep` decision
    /// (`erl_gc.c:2061-2098`) reduced to one target size.
    ///
    /// `need_after = live_words + need_words + need_reserve_words` is the room the
    /// collection must leave behind (`erl_gc.c:2072`). The current `capacity_words` is
    ///
    /// * **grown** to `next_words(need_after, 0)` when [`should_grow`](Self::should_grow)
    ///   holds — a fullsweep always leaves enough room for its own result (`erl_gc.c:2073-2077`);
    /// * **shrunk** to `next_words(2 * need_after, 0)` when
    ///   [`should_shrink`](Self::should_shrink) holds — the C's `wanted = 2 * need_after`
    ///   (`erl_gc.c:2087`);
    /// * otherwise **kept** as it is.
    ///
    /// [`min_heap_words`](Self::min_heap_words) floors and
    /// [`max_heap_words`](Self::max_heap_words) clamps every result (through
    /// [`next_words`](Self::next_words)); the shrink branch additionally never returns
    /// more than the current capacity, so a "shrink" can only ever make the heap smaller.
    pub fn mature_after_fullsweep(
        &self,
        capacity_words: usize,
        live_words: usize,
        need_words: usize,
    ) -> usize {
        let need_after = live_words
            .saturating_add(need_words)
            .saturating_add(self.need_reserve_words);
        if self.should_grow(capacity_words, need_after) {
            self.next_words(need_after, 0)
        } else if self.should_shrink(capacity_words, need_after) {
            self.next_words(need_after.saturating_mul(2), 0)
                .min(capacity_words)
        } else {
            capacity_words
        }
    }

    /// The nursery size (in words) to use after a collection that ran with `used_words`
    /// live words in a `capacity_words` nursery — the C's minor-time young-heap resize
    /// `next_heap_size(p, stack_size + MAX(size_before, need), 0)` (`erl_gc.c:1493`).
    ///
    /// The next round's request is at least [`need_reserve_words`](Self::need_reserve_words)
    /// and otherwise whatever the round just used, so a nursery that filled up
    /// ([`should_grow`](Self::should_grow)) steps up the growth table while one left mostly
    /// empty ([`should_shrink`](Self::should_shrink)) steps back down to the live set; a
    /// nursery that was neither keeps its size. Bounded by
    /// [`min_heap_words`](Self::min_heap_words)/[`max_heap_words`](Self::max_heap_words).
    pub fn young_after_collection(&self, capacity_words: usize, used_words: usize) -> usize {
        let request = used_words.max(self.need_reserve_words);
        if self.should_grow(capacity_words, request) {
            // The round filled (or nearly filled) the nursery: grow one curated step past
            // whatever it had, so the next round starts with room to spare.
            self.next_words(capacity_words.max(request), 1)
        } else if self.should_shrink(capacity_words, request) {
            // The round left the nursery mostly empty: fall back to the live set.
            self.next_words(request, 0)
        } else {
            capacity_words
        }
    }}

impl Default for HeapPolicy {
    fn default() -> Self {
        HeapPolicy::default()
    }
}

/// The curated heap-size growth table (the C `heap_sizes`: Fibonacci+1 to index 23,
/// then +20 %, capped at `2^53 / word`).
pub fn heap_sizes() -> &'static [usize] {
    &HEAP_SIZES
}

/// The next heap size after `size`, `offset` entries along the growth table — the
/// `erts_next_heap_size` analogue.
///
/// With `offset == 0` this is the smallest curated size `>= size`; with `offset == 1` the
/// next size strictly greater than `size`.
///
/// Unlike the C, which calls `erts_exit` when `size` exceeds the table, an out-of-range
/// `size` (or an `offset` that would run past the end) clamps to the largest curated size:
/// the M1 safe fallback, since a policy must never abort the runtime.
pub fn next_heap_size(size: usize, offset: usize) -> usize {
    let table = heap_sizes();
    // The table is never empty and always starts at 12 (`heap_sizes[0]`).
    let smallest = table[0];
    if size < smallest {
        return smallest;
    }

    // The clamping fallback for sizes beyond the table and for offsets that leave it.
    let largest = table[table.len() - 1];

    let mut low = 0usize;
    let mut high = table.len();
    while low < high {
        let mid = low + (high - low) / 2;
        if size < table[mid] {
            high = mid;
        } else if size == table[mid] {
            return table.get(mid + offset).copied().unwrap_or(largest);
        } else if mid + 1 < table.len() && size < table[mid + 1] {
            return table.get(mid + offset + 1).copied().unwrap_or(largest);
        } else {
            low = mid + 1;
        }
    }

    largest
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 64-bit cap the growth table is built against (`MAX(52bit) / sizeof(Eterm)`).
    fn cap() -> usize {
        ((1u64 << 53) / 8) as usize
    }

    #[test]
    fn growth_table_shape_mirrors_the_c_heap_sizes() {
        let table = heap_sizes();
        // Growth stage 1: Fibonacci + 1, reaching 233 at index 5 (the old default).
        assert_eq!(&table[..6], &[12, 38, 51, 90, 142, 233]);
        assert!(table.contains(&233));
        assert!(table.len() > FIBONACCI_STAGE_LEN);
        // Strictly increasing, so the binary search is well-defined.
        assert!(table.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn growth_table_stage_two_is_twenty_percent_capped() {
        let table = heap_sizes();
        for i in FIBONACCI_STAGE_LEN..table.len() {
            assert_eq!(table[i], table[i - 1] + table[i - 1] / 5);
        }
        // Every entry stays within the cap, and the table stopped exactly before the step
        // that would have exceeded it.
        let largest = table[table.len() - 1];
        assert!(largest <= cap());
        assert!(largest + largest / 5 > cap());
    }

    #[test]
    fn next_heap_size_matches_the_documented_lookup() {
        assert_eq!(next_heap_size(0, 0), 12);
        assert_eq!(next_heap_size(11, 0), 12);
        assert_eq!(next_heap_size(12, 0), 12);
        assert_eq!(next_heap_size(13, 0), 38);
        assert_eq!(next_heap_size(38, 0), 38);
        assert_eq!(next_heap_size(39, 0), 51);
        assert_eq!(next_heap_size(12, 1), 38);
        assert_eq!(next_heap_size(13, 1), 51);

        // Every curated size is a fixed point with offset 0 ...
        let table = heap_sizes();
        for &size in table {
            assert_eq!(next_heap_size(size, 0), size);
        }
        // ... and steps to its successor with offset 1.
        for pair in table.windows(2) {
            assert_eq!(next_heap_size(pair[0], 1), pair[1]);
        }
    }

    #[test]
    fn next_heap_size_clamps_out_of_range_sizes() {
        let largest = heap_sizes()[heap_sizes().len() - 1];
        assert_eq!(next_heap_size(usize::MAX, 0), largest);
        assert_eq!(next_heap_size(largest + 1, 0), largest);
        // An offset that would leave the table clamps too (no abort, no panic).
        assert_eq!(next_heap_size(largest, 1), largest);
        assert_eq!(next_heap_size(usize::MAX, 1), largest);
    }

    #[test]
    fn default_policy_matches_the_beam_defaults() {
        let policy = HeapPolicy::default();
        assert_eq!(policy.min_heap_words, 0);
        assert_eq!(policy.max_heap_words, 0);
        assert_eq!(policy.fullsweep_after, 65535);
        assert_eq!(policy.min_bin_vheap_words, 46_422);
        assert_eq!(policy.initial_young_words, 233);
        assert_eq!(policy.initial_mature_words, 233);
        assert_eq!(policy.need_reserve_words, 64);
        // The `Default` impl forwards to the inherent constructor.
        let via_trait: HeapPolicy = Default::default();
        assert_eq!(via_trait, policy);
    }

    #[test]
    fn min_bin_vheap_default_is_the_curated_rounding_of_vh_default_size() {
        // `erl_init.c:284`: `BIN_VH_MIN_SIZE = erts_next_heap_size(VH_DEFAULT_SIZE, 0)`.
        assert_eq!(VH_DEFAULT_SIZE_WORDS, 32768);
        assert_eq!(next_heap_size(VH_DEFAULT_SIZE_WORDS, 0), 46_422);
        assert_eq!(
            HeapPolicy::default().min_bin_vheap_words,
            next_heap_size(VH_DEFAULT_SIZE_WORDS, 0)
        );
        // `DEFAULT_MIN_HEAP_WORDS` is itself a fixed point of the table.
        assert_eq!(DEFAULT_MIN_HEAP_WORDS, 233);
        assert_eq!(next_heap_size(DEFAULT_MIN_HEAP_WORDS, 0), DEFAULT_MIN_HEAP_WORDS);
    }

    #[test]
    fn from_spawn_opts_applies_the_c_rounding_rules() {
        // The process defaults: the oracle reports min_heap_size 233, min_bin_vheap_size
        // 46422 and fullsweep_after 65535.
        let default = HeapPolicy::from_spawn_opts(0, 0, 65535);
        assert_eq!(default.min_heap_words, 233);
        assert_eq!(default.min_bin_vheap_words, 46_422);
        assert_eq!(default.fullsweep_after, 65535);

        // A sub-minimum request is replaced by the minimum, never rounded past it.
        assert_eq!(HeapPolicy::from_spawn_opts(1, 1, 3).min_heap_words, 233);
        assert_eq!(HeapPolicy::from_spawn_opts(232, 10, 3).min_heap_words, 233);
        assert_eq!(
            HeapPolicy::from_spawn_opts(0, 46_421, 3).min_bin_vheap_words,
            46_422
        );

        // A request at or above the minimum is rounded up through `next_heap_size(x, 0)`
        // and is therefore a fixed point of the growth table.
        assert_eq!(HeapPolicy::from_spawn_opts(233, 0, 7).min_heap_words, 233);
        assert_eq!(HeapPolicy::from_spawn_opts(234, 0, 7).min_heap_words, 376);
        assert_eq!(HeapPolicy::from_spawn_opts(377, 0, 7).min_heap_words, 610);
        assert_eq!(HeapPolicy::from_spawn_opts(46_422, 0, 7).min_heap_words, 46_422);
        assert_eq!(HeapPolicy::from_spawn_opts(46_423, 0, 7).min_heap_words, 75113);

        for request in [233usize, 234, 376, 377, 1_000, 46_422, 100_000] {
            let policy = HeapPolicy::from_spawn_opts(request, request, 0);
            assert_eq!(
                policy.min_heap_words,
                next_heap_size(policy.min_heap_words, 0),
                "the stored minimum must be a growth-table fixed point"
            );
            assert_eq!(
                policy.min_bin_vheap_words,
                next_heap_size(policy.min_bin_vheap_words, 0)
            );
        }

        // `fullsweep_after` is stored verbatim, including 0.
        assert_eq!(HeapPolicy::from_spawn_opts(233, 0, 0).fullsweep_after, 0);
        assert_eq!(HeapPolicy::from_spawn_opts(233, 0, 12).fullsweep_after, 12);
    }

    #[test]
    fn fullsweep_cadence_including_zero() {
        let policy = HeapPolicy {
            fullsweep_after: 2,
            ..HeapPolicy::default()
        };
        assert!(!policy.fullsweep_due(0));
        assert!(!policy.fullsweep_due(1));
        assert!(policy.fullsweep_due(2));
        assert!(policy.fullsweep_due(3));

        // 0 means a fullsweep on *every* collection, exactly as the C `0 >= 0`.
        let always = HeapPolicy {
            fullsweep_after: 0,
            ..HeapPolicy::default()
        };
        assert!(always.fullsweep_due(0));
        assert!(always.fullsweep_due(1));

        // The default 65535 is effectively "never" for any realistic counter.
        let never = HeapPolicy::default();
        assert!(!never.fullsweep_due(65_534));
        assert!(never.fullsweep_due(65_535));
    }
    #[test]
    fn from_min_max_overrides_only_the_bounds() {
        let policy = HeapPolicy::from_min_max(233, 100_000);
        let default = HeapPolicy::default();
        assert_eq!(policy.min_heap_words, 233);
        assert_eq!(policy.max_heap_words, 100_000);
        assert_eq!(policy.fullsweep_after, default.fullsweep_after);
        assert_eq!(policy.initial_young_words, default.initial_young_words);
        assert_eq!(policy.initial_mature_words, default.initial_mature_words);
        assert_eq!(policy.need_reserve_words, default.need_reserve_words);
    }

    #[test]
    fn next_words_honours_the_policy_bounds() {
        let policy = HeapPolicy::default();
        assert_eq!(policy.next_words(0, 0), 12);
        assert_eq!(policy.next_words(13, 0), 38);
        assert_eq!(policy.next_words(12, 1), 38);

        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        assert_eq!(floored.next_words(10, 0), 500);
        assert_eq!(floored.next_words(300, 0), 500); // table says 376, the floor wins
        assert_eq!(floored.next_words(400, 0), 610); // table is already above the floor

        let capped = HeapPolicy {
            max_heap_words: 100,
            ..HeapPolicy::default()
        };
        assert_eq!(capped.next_words(10, 0), 12);
        assert_eq!(capped.next_words(300, 0), 100);
    }

    #[test]
    fn size_after_fullsweep_always_fits_the_live_set() {
        let policy = HeapPolicy::default();
        for live in [0usize, 1, 12, 100, 233, 10_000] {
            for need in [0usize, 1, 50, 233, 5_000] {
                let size = policy.size_after_fullsweep(live, need);
                assert!(
                    size >= live + need,
                    "size {size} < live + need {}",
                    live + need
                );
                // The collection always asks for its reserve on top.
                assert!(size > live + need);
            }
        }

        // A tiny heap still gets a curated size: the request is 0 + 0 + 64 = 64 words, so the
        // next curated size is 90.
        assert_eq!(policy.size_after_fullsweep(0, 0), 90);
        // Explicit bounds win.
        let bounded = HeapPolicy::from_min_max(1000, 2000);
        assert_eq!(bounded.size_after_fullsweep(0, 0), 1000);
        assert_eq!(bounded.size_after_fullsweep(1_000_000, 0), 2000);
    }

    #[test]
    fn grow_and_shrink_boundaries() {
        let policy = HeapPolicy::default();

        // Growth triggers when the heap is too small, or when it is below 4/3 of the need.
        assert!(!policy.should_grow(100, 75)); // 3*100 == 4*75: exactly enough
        assert!(policy.should_grow(100, 76)); // one over
        assert!(policy.should_grow(100, 101)); // outright too small
        assert!(!policy.should_grow(1000, 750));
        assert!(policy.should_grow(1000, 751));
        assert!(policy.should_grow(0, 1));
        assert!(!policy.should_grow(4, 3));

        // Shrinking triggers only when more than 4x the need is kept.
        assert!(!policy.should_shrink(100, 25)); // 100 == 4*25: not yet
        assert!(policy.should_shrink(100, 24));
        assert!(!policy.should_shrink(0, 0));
        assert!(policy.should_shrink(1000, 0));

        // The minimum heap size floors the shrink decision.
        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        assert!(!floored.should_shrink(500, 100));
        assert!(floored.should_shrink(501, 100));
    }

    #[test]
    fn mature_after_fullsweep_grows_shrinks_and_keeps() {
        let policy = HeapPolicy::default();

        // Grown: a mature space too small for the live set gets the curated size that
        // covers `live + reserve`. `mature_after_fullsweep(12, 13, 0)` asks for 77 words.
        assert_eq!(policy.mature_after_fullsweep(12, 13, 0), 90);
        assert_eq!(policy.mature_after_fullsweep(0, 0, 0), 90);

        // Kept: 233 words with a 100-word live set needs 164 — neither grow nor shrink.
        assert_eq!(policy.mature_after_fullsweep(233, 100, 0), 233);

        // Shrunk: 1000 words keeping 0 needs 64, so `should_shrink` fires and the target is
        // the curated size of `2 * 64 = 128`, i.e. 142 — never more than the old capacity.
        assert_eq!(policy.mature_after_fullsweep(1000, 0, 0), 142);
        assert_eq!(policy.mature_after_fullsweep(1000, 5, 0), 142);
        assert_eq!(policy.mature_after_fullsweep(1000, 10, 0), 233);
        // A grown result always covers the live set it was asked about.
        for live in [0usize, 1, 12, 233, 5_000, 100_000] {
            let size = policy.mature_after_fullsweep(12, live, 0);
            assert!(size >= live, "grown size {size} < live {live}");
        }
        // A shrunk result always covers the live set too.
        for live in [0usize, 12, 233] {
            let size = policy.mature_after_fullsweep(1_000_000, live, 0);
            assert!(size >= live, "shrunk size {size} < live {live}");
            assert!(size <= 1_000_000);
        }
    }

    #[test]
    fn mature_after_fullsweep_honours_min_and_max_bounds() {
        // A minimum floors both the grown and the shrunk target.
        let floored = HeapPolicy {
            min_heap_words: 4_096,
            ..HeapPolicy::default()
        };
        assert_eq!(floored.mature_after_fullsweep(12, 13, 0), 4_096);
        // 8192 words keeping 100 needs 164: the shrink target (curated 328) is floored to
        // 4096, which is still smaller than 8192.
        assert_eq!(floored.mature_after_fullsweep(8_192, 100, 0), 4_096);
        // A capacity already at the floor is never "shrunk" further.
        assert_eq!(floored.mature_after_fullsweep(4_096, 100, 0), 4_096);

        // A maximum clamps a would-be grow: 12 words needing 464 would go to 610, capped to 512.
        let capped = HeapPolicy {
            max_heap_words: 512,
            ..HeapPolicy::default()
        };
        assert_eq!(capped.mature_after_fullsweep(12, 400, 0), 512);
        // A would-be shrink is clamped by the maximum but is still strictly smaller.
        assert_eq!(capped.mature_after_fullsweep(100_000, 400, 0), 512);        // The fullsweep always sees the byte-accounted reserve on top of the live set.
        let default = HeapPolicy::default();
        assert_eq!(
            default.mature_after_fullsweep(12, 100, 100),
            default.next_words(100 + 100 + default.need_reserve_words, 0)
        );
    }

    #[test]
    fn young_after_collection_grows_under_pressure_and_shrinks_when_empty() {
        let policy = HeapPolicy::default();

        // A full nursery grows one curated step: 233 -> 376.
        assert_eq!(policy.young_after_collection(233, 233), 376);
        // "Under pressure" starts once the round used more than 3/4 of the nursery.
        assert_eq!(policy.young_after_collection(233, 175), 376);
        assert_eq!(policy.young_after_collection(233, 174), 233);

        // A nursery the round left mostly empty keeps its size (233 is not > 4 * 64).
        assert_eq!(policy.young_after_collection(233, 13), 233);
        // A grossly over-sized nursery shrinks back towards the live set.
        assert_eq!(policy.young_after_collection(1_000, 13), 90);
        assert_eq!(policy.young_after_collection(1_000, 0), 90);

        // The result is always bounded by the policy.
        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        let shrunk = floored.young_after_collection(100_000, 0);
        assert_eq!(shrunk, 500, "the minimum floors the shrunk nursery");
        let capped = HeapPolicy {
            max_heap_words: 300,
            ..HeapPolicy::default()
        };
        assert_eq!(capped.young_after_collection(233, 233), 300);
    }
}