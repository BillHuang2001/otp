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

//! The **binary virtual heap** — the trigger half of an off-heap collection, the C
//! `bin_vheap_sz` accounting (`erts/emulator/beam/erl_gc.c:2914-2959`).
//!
//! A process's off-heap binaries live outside every process heap, so growing the *heap*
//! occupancy never signals that they need collecting. ERTS sizes a separate budget for them and
//! makes it a second collection trigger
//! (`erts/emulator/beam/erl_gc.h:132-135`):
//!
//! ```text
//! ERTS_IS_GC_DESIRED_INTERNAL(Proc, HTop, STop, XtraFlags)
//!     = ((STop - HTop < mbuf_sz)                       /* no heap room left */
//!        | (Proc->off_heap.overhead > Proc->bin_vheap_sz) /* too much off-heap payload */
//!        | !!(Proc->flags & (F_FORCE_GC | XtraFlags)))
//! ```
//!
//! So the second disjunct is exactly [`BinVheap::triggered`] — the off-heap overhead the process
//! is charged for, in **words**, against the limit the process last sized itself to. After a
//! collection the limit is re-derived from what **survived** (`erl_gc.c:3288-3289`):
//!
//! ```text
//! p->bin_vheap_sz = next_vheap_size(p, bin_vheap, p->bin_vheap_sz);
//! MSO(p).overhead = bin_vheap;
//! ```
//!
//! [`BinVheap::record_collection`] is that pair of statements, with `bin_vheap` (the surviving
//! overhead the collection measured, `erl_gc.c:3051`/`:3177`) passed in by the caller.
//!
//! # Why this matters for retention
//!
//! Without this trigger an off-heap-heavy process is collected only when its *heap* fills, so a
//! workload that builds a 64 KiB binary per call — a `binary:copy/2`, a `binary_append`-shaped
//! `<< <<C:8,C:8,C:8,C:8>> || <<C:8>> <= Bin >>`, any `bs_init_writable` accumulator — can
//! retain one payload per call until the nursery happens to fill. The C's answer is this
//! budget: the payload the process is charged for, compared with a limit that shrinks back
//! toward the surviving live set, so the collection cadence tracks the payload volume rather
//! than the term volume. This crate's half is the *accounting*; the runtime's half is to consult
//! [`BinVheap::triggered_for`] at a heap-admission door and then collect + sweep
//! ([`crate::sweep_unreachable_binaries`]).
//!
//! The floor is [`HeapPolicy::min_bin_vheap_words`], the C `p->min_vheap_size`
//! (`BIN_VH_MIN_SIZE`, which the oracle reports as `min_bin_vheap_size` = 46422 words), so a
//! process that holds almost no off-heap payload never shrinks its budget below the configured
//! minimum — the same value the `erlang:process_info(Pid, garbage_collection)` item reports.
//!
//! This module is safe Rust, allocation-free and has no dependency beyond
//! [`HeapPolicy`](crate::policy::HeapPolicy) and the roster it measures.

use core::mem::size_of;

use beam_term::prelude::Eterm;

use crate::mso::OffHeap;
use crate::policy::HeapPolicy;

/// The golden-ratio growth factor of `do_next_vheap_size` (`erl_gc.c:2937`).
///
/// The C reads `(Uint64) vheap_sz * 1.618`, i.e. a `double` multiply truncated toward zero; the
/// same expression is reproduced here so the grown limit is bit-for-bit the C's.
const GROWTH_RATIO: f64 = 1.618;

/// A process's binary virtual heap: the off-heap overhead, in words, past which a collection is
/// wanted, and the C `next_vheap_size` rule that re-sizes it after every collection.
///
/// Units are **term words** throughout, exactly as in the C (`bin_vheap += overhead /
/// sizeof(Eterm)`, `erl_gc.c:3051`), so the limit is directly comparable with
/// [`HeapPolicy::min_bin_vheap_words`] and with `erlang:process_info(Pid,
/// garbage_collection)`'s `min_bin_vheap_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinVheap {
    /// The C `p->bin_vheap_sz`: the off-heap overhead (in words) past which a collection is
    /// desired.
    limit_words: usize,
}

impl BinVheap {
    /// A fresh budget for a process started with `policy` — the C `p->bin_vheap_sz =
    /// p->min_vheap_size` at process creation (`erl_process.c`), i.e.
    /// [`HeapPolicy::min_bin_vheap_words`].
    pub fn new(policy: &HeapPolicy) -> BinVheap {
        BinVheap {
            limit_words: policy.min_bin_vheap_words,
        }
    }

    /// The current limit, in term words (the C `p->bin_vheap_sz`).
    pub fn limit_words(&self) -> usize {
        self.limit_words
    }

    /// Whether `off_heap_overhead_words` of off-heap payload past the current limit wants a
    /// collection — the C `Proc->off_heap.overhead > Proc->bin_vheap_sz` (`erl_gc.h:134`).
    ///
    /// The comparison is **strictly greater**: an overhead exactly equal to the limit is still
    /// within budget, exactly as in the C (a `>` on the two `Uint64` fields).
    pub fn triggered(&self, off_heap_overhead_words: usize) -> bool {
        off_heap_overhead_words > self.limit_words
    }

    /// [`triggered`](Self::triggered) over a process's off-heap roster, charging each entry as
    /// the C does: the payload's size in words.
    ///
    /// The roster's [`overhead_bytes`](OffHeap::overhead_bytes) is the sum of the live entries'
    /// apparent sizes (an MSO's Erlang-visible size, a refc binary's payload length), and each
    /// is divided by the word size here — one integer division on the total rather than one per
    /// entry, which agrees with the C's per-binary `overhead / sizeof(Eterm)` up to the rounding
    /// of the summed remainder (a fraction of a word per entry).
    pub fn triggered_for(&self, off_heap: &OffHeap) -> bool {
        self.triggered(off_heap.overhead_bytes() / size_of::<Eterm>())
    }

    /// Re-sizes the budget after a collection, from the off-heap overhead that **survived** it —
    /// the C `p->bin_vheap_sz = next_vheap_size(p, bin_vheap, p->bin_vheap_sz)`
    /// (`erl_gc.c:3288`) with `bin_vheap` the surviving overhead the collection counted
    /// (`erl_gc.c:3051`, `:3177`).
    ///
    /// Grows by the golden ratio while the surviving payload outgrows a quarter of the budget,
    /// halves when it falls below a quarter, and stands still in between
    /// ([`next_vheap_size`]). The result is floored at `policy.min_bin_vheap_words` — the C
    /// `p->min_vheap_size` (`erl_gc.c:2958`), which is why a process holding almost nothing
    /// converges to the configured minimum instead of collapsing to zero.
    pub fn record_collection(&mut self, policy: &HeapPolicy, surviving_overhead_words: usize) {
        self.limit_words = next_vheap_size(
            surviving_overhead_words,
            self.limit_words,
            policy.min_bin_vheap_words,
        );
    }
}

/// The C `do_next_vheap_size` (`erl_gc.c:2914-2953`): the growth-table-free rule that sizes the
/// binary virtual heap to the payload that survived a collection.
///
/// ```text
///                 grow            vheap/3 > vheap_sz/4
/// vheap_sz ======================
///
/// vheap 75% +     grow            (the same test, growing vheap_sz by the golden ratio until
///          ----------------------  it is out of the grow band)
///
/// vheap 25-75%    same            both tests false: keep vheap_sz
///          ----------------------
///
/// vheap ~-25%     shrink          vheap < vheap_sz/4  =>  vheap_sz >> 1
///          ----------------------
/// ```
///
/// Reproduced with the C's own integer arithmetic: both comparisons are integer divisions
/// (`vheap/3`, `vheap_sz/4`), the growth step is a `double` multiply truncated toward zero, and
/// the shrink step is a right shift. The one addition is the stagnation guard: the C's
/// `if (new_vheap_sz < vheap_sz) return vheap_sz;` catches a wrapping multiply, and requiring
/// `new_vheap_sz > vheap_sz` catches the same non-progress in the cases where the truncating
/// `f64` product fails to advance (a stagnating loop would otherwise never terminate for a
/// limit past `2^53`).
fn do_next_vheap_size(vheap: usize, vheap_sz: usize) -> usize {
    if vheap / 3 > vheap_sz / 4 {
        let mut vheap_sz = vheap_sz;
        while vheap / 3 > vheap_sz / 4 {
            let grown = (vheap_sz as f64 * GROWTH_RATIO) as usize;
            if grown <= vheap_sz {
                // No progress (a wrapping product in the C, a stagnating `f64` product here):
                // the C returns the current size for the former, and there is no better answer
                // for the latter than to stop where we are.
                return vheap_sz;
            }
            vheap_sz = grown;
        }
        return vheap_sz;
    }

    if vheap < vheap_sz / 4 {
        return vheap_sz >> 1;
    }

    vheap_sz
}

/// The C `next_vheap_size` (`erl_gc.c:2955-2959`): [`do_next_vheap_size`] floored at the
/// process's `min_vheap_size`.
fn next_vheap_size(vheap: usize, vheap_sz: usize, min_vheap_size: usize) -> usize {
    do_next_vheap_size(vheap, vheap_sz).max(min_vheap_size)
}

#[cfg(test)]
mod tests {
    use super::*;

    use beam_alloc::RefcBinary;
    /// A policy whose binary-virtual-heap floor is small enough to see the growth rule work.
    fn policy_with_floor(min_bin_vheap_words: usize) -> HeapPolicy {
        HeapPolicy {
            min_bin_vheap_words,
            ..HeapPolicy::default()
        }
    }

    #[test]
    fn a_fresh_budget_is_the_policys_minimum_binary_vheap_size() {
        // The C `p->bin_vheap_sz = p->min_vheap_size` at process creation, and the oracle's own
        // default `min_bin_vheap_size` of 46422 words (`erts_next_heap_size(32768, 0)`).
        let default = HeapPolicy::default();
        assert_eq!(BinVheap::new(&default).limit_words(), 46_422);
        assert_eq!(
            BinVheap::new(&policy_with_floor(17)).limit_words(),
            17,
            "the launch budget is the policy's floor, not the BEAM default"
        );
    }

    #[test]
    fn the_trigger_is_strictly_greater_than_the_limit() {
        // The C `Proc->off_heap.overhead > Proc->bin_vheap_sz` (`erl_gc.h:134`): an overhead
        // exactly at the limit is still within budget.
        let vheap = BinVheap::new(&policy_with_floor(100));
        assert!(!vheap.triggered(99), "below the limit");
        assert!(
            !vheap.triggered(100),
            "exactly at the limit is NOT a trigger"
        );
        assert!(vheap.triggered(101), "one word past the limit triggers");
    }

    #[test]
    fn the_roster_trigger_counts_payload_bytes_in_words() {
        // 4096 payload bytes = 512 words against a 512-word limit: exactly at the limit, so no
        // trigger; one more byte pushes it over.
        let vheap = BinVheap::new(&policy_with_floor(512));
        let mut roster = OffHeap::new();
        roster.push(RefcBinary::new(&vec![0u8; 4096]).expect("payload"));
        assert_eq!(roster.overhead_bytes(), 4096);
        assert!(
            !vheap.triggered_for(&roster),
            "4096 bytes is exactly 512 words, which is still within the 512-word budget"
        );
        roster.push(RefcBinary::new(&vec![0u8; 8]).expect("payload"));
        assert!(
            vheap.triggered_for(&roster),
            "513 words of payload is past the 512-word budget"
        );
    }

    #[test]
    fn a_surviving_payload_past_the_grow_band_grows_the_budget_by_the_golden_ratio() {
        // The C `do_next_vheap_size(1000, 100)` (`erl_gc.c:2932-2945`), step by step, with the
        // C's integer divisions and its truncating `* 1.618`:
        //   vheap/3 = 333;  vheap_sz/4 = 25   -> grow
        //   100  * 1.618 = 161.8  -> 161;  161/4  = 40;   333 > 40   -> keep going
        //   161  * 1.618 = 260.5  -> 260;  260/4  = 65;   333 > 65   -> keep going
        //   260  * 1.618 = 420.7  -> 420;  420/4  = 105;  333 > 105  -> keep going
        //   420  * 1.618 = 679.6  -> 679;  679/4  = 169;  333 > 169  -> keep going
        //   679  * 1.618 = 1098.6 -> 1098; 1098/4 = 274;  333 > 274  -> keep going
        //   1098 * 1.618 = 1776.6 -> 1776; 1776/4 = 444;  333 > 444  -> stop
        assert_eq!(do_next_vheap_size(1000, 100), 1776);
        // Through the floor-free entry point, and through a floor that does not bite:
        assert_eq!(next_vheap_size(1000, 100, 0), 1776);
        assert_eq!(next_vheap_size(1000, 100, 1000), 1776);
        // A floor above the grown value wins (the C `new_vheap_sz < p->min_vheap_size` clamp):
        assert_eq!(next_vheap_size(1000, 100, 2000), 2000);
    }

    #[test]
    fn a_surviving_payload_below_a_quarter_halves_the_budget() {
        // The C `if (vheap < (Uint64) (vheap_sz/4)) return (vheap_sz >> 1);`
        // (`erl_gc.c:2947-2949`): 10/3 = 3 is not > 250, and 10 < 250, so the shrink branch
        // runs and the budget halves.
        assert_eq!(do_next_vheap_size(10, 1000), 500);
        assert_eq!(next_vheap_size(10, 1000, 0), 500);
        // The floor is applied *after* the halving (the C clamps in `next_vheap_size`), so a
        // halved budget below the policy minimum is raised back to it.
        assert_eq!(next_vheap_size(10, 1000, 600), 600);
        // And a halving that would land exactly on the limit keeps the limit.
        assert_eq!(next_vheap_size(249, 1000, 0), 500, "249 <= 1000/4 - 1");
        assert_eq!(
            do_next_vheap_size(250, 1000),
            1000,
            "250 is not < 1000/4, so the 25-75% band holds the budget still"
        );
    }

    #[test]
    fn a_surviving_payload_in_the_middle_band_leaves_the_budget_alone() {
        // Both of the C's tests false: not growing (vheap/3 <= vheap_sz/4) and not shrinking
        // (vheap >= vheap_sz/4) — the 25-75% band keeps the budget.
        for vheap in [250usize, 400, 500, 749] {
            assert_eq!(
                do_next_vheap_size(vheap, 1000),
                1000,
                "{vheap} words of survivors is inside the 25-75% band"
            );
        }
    }

    #[test]
    fn record_collection_reproduces_next_vheap_size_and_honours_the_floor() {
        // Growth: an 8192-word payload (one 64 KiB binary) against a 233-word budget.
        let policy = policy_with_floor(233);
        let mut vheap = BinVheap { limit_words: 233 };
        vheap.record_collection(&policy, 8192);
        assert_eq!(
            vheap.limit_words(),
            do_next_vheap_size(8192, 233),
            "the recorded limit is the C rule, unfloored here because it is already > 233"
        );
        assert!(
            vheap.limit_words() > 8192,
            "the budget must end up above the surviving payload, or the next collection is due \
             immediately"
        );

        // Shrink, then the floor: a nearly empty roster cannot pull the budget below the
        // policy's minimum (the C `p->min_vheap_size`, `erl_gc.c:2958`).
        let mut vheap = BinVheap::new(&policy);
        vheap.record_collection(&policy, 0);
        assert_eq!(
            vheap.limit_words(),
            233,
            "233 >> 1 = 116 is below the 233-word floor, so the floor wins"
        );

        // A second, larger floor overrides an otherwise-legitimate shrink.
        let policy = policy_with_floor(900);
        let mut vheap = BinVheap::new(&policy);
        vheap.record_collection(&policy, 0);
        assert_eq!(vheap.limit_words(), 900);
    }

    #[test]
    fn the_limit_never_stagnates_and_stays_above_the_surviving_payload() {
        // Repeated collections whose survivor set grows: the golden-ratio loop always terminates
        // and always ends strictly above `vheap/3 * 4`, i.e. above the payload that must fit.
        let mut limit = 233usize;
        for surviving in [1usize, 100, 8_192, 100_000, 1 << 40] {
            let next = next_vheap_size(surviving, limit, 233);
            assert!(
                next > surviving,
                "limit {next} must exceed the surviving {surviving} words"
            );
            assert!(next >= 233, "and never fall below the floor");
            limit = next;
        }
        // A limit past the range where an `f64` product can still advance cannot spin: the
        // stagnation guard returns the current size instead of looping forever.
        let huge = usize::MAX / 4;
        assert!(
            do_next_vheap_size(usize::MAX / 2, huge) >= huge,
            "a limit past the exact-integer range must still terminate, and never shrink"
        );
    }
}
