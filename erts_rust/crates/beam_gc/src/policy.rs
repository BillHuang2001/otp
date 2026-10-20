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

/// How much of the **whole-heap live set** one nursery sizes itself to
/// ([`HeapPolicy::young_target_words`]).
///
/// A copying nursery amortizes the live set's evacuation over the words the mutator allocates
/// into it: a collection copies `live` words and the mutator then allocates a whole nursery
/// before the next one, so the collector's copy volume per allocated word is
/// `live / nursery`. Sizing the nursery at a *multiple* of the live set therefore divides the
/// re-copying by that multiple — the nursery holds several rounds of allocation before the
/// (fixed-cost) live set is copied again — at a memory cost of the same multiple.
///
/// `2` is the smallest multiple that also leaves the live set's own **fluctuation** inside the
/// keep band: a workload's live set swings by ~3× within one operation (a partially built
/// accumulator is live that the next round replaces), and the band from
/// [`YOUNG_SHRINK_FACTOR`] must be wider than that swing or the capacity oscillates between
/// collections — see `MEASURED.md`, "The nursery follows the live set".
pub const YOUNG_TARGET_LIVE_MULTIPLE: usize = 2;

/// The factor by which a nursery must exceed its live-set-derived target
/// ([`HeapPolicy::young_target_words`]) before a collection gives the memory back.
///
/// This is the hysteresis band's upper edge, and both of its properties matter:
///
/// * it must be **wider than the live set's own fluctuation** (`YOUNG_TARGET_LIVE_MULTIPLE`
///   times the ~3× a partially built term swings a live set) or a collection that samples the
///   live set at a trough shrinks a nursery the next one has to grow back — an oscillation,
///   not a fixed point; and
/// * it must still be **finite and unrelated to the capacity**, so a nursery grown for a burst
///   returns to the live set's scale once the burst is dead (`tests/nursery_bounded.rs`), and
///   so that the growth rule can never feed itself.
pub const YOUNG_SHRINK_FACTOR: usize = 8;

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
    /// Seed for the size of the young (nursery) space, in term words — the role of the C's
    /// `heap_need` at spawn. The **first** young space a collector installs is
    /// [`initial_heap_words`](Self::initial_heap_words), i.e. this value floored by
    /// [`min_heap_words`](Self::min_heap_words), so `+hms`/`spawn_opt`'s `min_heap_size`
    /// really sizes a new process's heap.
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

    /// The size, in words, of the **first** young (nursery) space a process is born with —
    /// the C's initial-heap rule at spawn:
    ///
    /// ```c
    /// sz = heap_need < p->min_heap_size ? p->min_heap_size : erts_next_heap_size(heap_need, 0);
    /// p->heap_sz = sz;                          /* erl_process.c:12586-12592 */
    /// ```
    ///
    /// A cheap spawn's `heap_need` is below the minimum, so the answer is the policy's
    /// [`min_heap_words`](Self::min_heap_words) whenever one is set — the emulator's `+hms`
    /// switch or a `spawn_opt` `{min_heap_size, N}` option — and
    /// [`initial_young_words`](Self::initial_young_words) otherwise. That value *is* the
    /// default minimum: `HeapPolicy::default` carries no floor, so its `initial_young_words`
    /// (`DEFAULT_MIN_HEAP_WORDS`, 233) is the seed that stands in for the C's `H_MIN_SIZE`.
    /// This is what makes `erlang:process_info(P, heap_size)` on a freshly spawned process
    /// exactly its `min_heap_size` (`HEAP_SIZE(p) == p->heap_sz`, `erl_process.h:1017`,
    /// `erl_bif_info.c:936`) — including `+hms 100`, whose 142-word heap is *below* the
    /// unconfigured 233.
    ///
    /// The seed is not rounded through the growth table: a caller that set
    /// `initial_young_words` named a size rather than a request (the `spawn_opt` path rounds
    /// its own requests in [`Self::spawn_policy`]).
    ///
    /// # Deviation from the C
    ///
    /// This runtime does not model the C's `heap_need = arg_size + S_RESERVED (+ the group
    /// leader's NC_HEAP_SIZE)` at spawn: the child's arguments are copied onto its heap
    /// afterwards and the space grows on demand, so the first heap is `min_heap_size` rather
    /// than `max(heap_need, min_heap_size)`. A spawn whose argument graph exceeds the minimum
    /// therefore starts smaller than the oracle's and reaches a comparable size through a
    /// collection instead of starting there.
    pub fn initial_heap_words(&self) -> usize {
        if self.min_heap_words != 0 {
            self.min_heap_words
        } else {
            self.initial_young_words
        }
    }

    /// The policy a **new process** gets from the `spawn_opt` heap options
    /// `min_heap_size`, `min_bin_vheap_size` and `fullsweep_after`, with `self` as the
    /// emulator's global policy — the C's `hmin` / `bin_vhmin` / `erts_max_gen_gcs` floor
    /// source (`erl_process.c:12242-12248`, `12321-12360`).
    ///
    /// Per option, the C's rule (`erl_process.c:12321-12332`) is:
    ///
    /// * an option value below the **global** minimum is replaced by that global minimum
    ///   (not rounded past it) — so `+hms 4000` floors a `{min_heap_size, 100}` request at
    ///   4185;
    /// * anything else is rounded through `erts_next_heap_size(x, 0)`, so the stored minimum
    ///   is a fixed point of the growth table — the C's
    ///   `ASSERT(p->min_heap_size == erts_next_heap_size(p->min_heap_size, 0))`
    ///   (`erl_process.c:12575`);
    /// * an absent option (and `fullsweep_after` in every case) falls back to the global
    ///   policy's own value.
    ///
    /// `min_bin_vheap_words` follows the identical rule against the global
    /// `min_bin_vheap_words` (the C `BIN_VH_MIN_SIZE`). Every other field — the initial
    /// space sizes, `need_reserve_words` and `max_heap_words` (the C `H_MAX_SIZE`, which a
    /// `spawn_opt` child inherits unless its own `{max_heap_size, …}` overrides it) — is
    /// inherited from `self`.
    ///
    /// A global field of `0` means *unset* (both minimums are `0` only in
    /// [`HeapPolicy::default`], which is a growth-policy default rather than an emulator
    /// configuration), so the emulator's unconfigured defaults
    /// ([`DEFAULT_MIN_HEAP_WORDS`] / [`default_min_bin_vheap_words`]) are the floor then.
    /// That is why [`Self::from_spawn_opts`] — the spawn rule over an *unconfigured*
    /// emulator — is exactly `HeapPolicy::default().spawn_policy(…)`.
    pub fn spawn_policy(
        &self,
        min_heap_words: usize,
        min_bin_vheap_words: usize,
        fullsweep_after: Option<u32>,
    ) -> HeapPolicy {
        let heap_floor = if self.min_heap_words == 0 {
            DEFAULT_MIN_HEAP_WORDS
        } else {
            self.min_heap_words
        };
        let vheap_floor = if self.min_bin_vheap_words == 0 {
            default_min_bin_vheap_words()
        } else {
            self.min_bin_vheap_words
        };
        HeapPolicy {
            min_heap_words: round_spawn_min(min_heap_words, heap_floor),
            min_bin_vheap_words: round_spawn_min(min_bin_vheap_words, vheap_floor),
            fullsweep_after: fullsweep_after.unwrap_or(self.fullsweep_after),
            ..self.clone()
        }
    }

    /// A policy for a process started with the `spawn_opt` heap options
    /// `min_heap_size`, `min_bin_vheap_size` and `fullsweep_after` against the
    /// **unconfigured** emulator — the C defaults `H_MIN_SIZE` (`DEFAULT_MIN_HEAP_WORDS`,
    /// 233) and `BIN_VH_MIN_SIZE` as the floors.
    ///
    /// This is [`HeapPolicy::default().spawn_policy`]: an emulator whose `+hms`/`+hmbs`
    /// switches were not given floors the request at exactly the same values. A host that
    /// *does* parse those switches passes its own policy instead (that is `beam_rt`'s
    /// `Runtime::default_spawn_policy`), so the floor has exactly one authority and the
    /// `+hms` value cannot drift from the `spawn_opt` floor.
    pub fn from_spawn_opts(
        min_heap_words: usize,
        min_bin_vheap_words: usize,
        fullsweep_after: u32,
    ) -> HeapPolicy {
        HeapPolicy::default().spawn_policy(
            min_heap_words,
            min_bin_vheap_words,
            Some(fullsweep_after),
        )
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

    /// The nursery size (in words) to use after a **minor**, from the `capacity_words` the
    /// nursery chain had when it was collected and the `live_words` the collection kept alive
    /// across the **whole heap**.
    ///
    /// A minor **keeps or shrinks; it never grows.** The live set a minor can see cheaply is
    /// the mature generation's occupancy (a minor promotes every live young word into it), and
    /// that figure holds the garbage the minors since the last fullsweep promoted — it is an
    /// **over**-estimate, so using it to grow would close a loop through the nursery (a bigger
    /// nursery admits a bigger promotion, which raises the occupancy that sizes the nursery).
    /// Growth is therefore decided only where the live set is measured **exactly**: a fullsweep
    /// ([`young_after_fullsweep`](Self::young_after_fullsweep)), or an explicit request
    /// ([`ensure_capacity`](crate::Collector::ensure_capacity)).
    ///
    /// The shrink half is the property that must survive: a nursery stepped up for a burst has
    /// to come back down, so once the live set has fallen [`YOUNG_SHRINK_FACTOR`] times below
    /// [`young_target_words`](Self::young_target_words) the nursery falls back to the target.
    /// The band is that wide because an over-estimated live set shrinks *conservatively* (it can
    /// only postpone a shrink), while a narrower band would oscillate on a live set that swings
    /// within one operation.
    pub fn young_after_collection(&self, capacity_words: usize, live_words: usize) -> usize {
        let target = self.young_target_words(live_words);
        if capacity_words > YOUNG_SHRINK_FACTOR.saturating_mul(target) {
            // The collection kept only a small part of what the nursery is sized for: give the
            // memory back.
            target.min(capacity_words)
        } else {
            // The nursery is within its band: keep it. (Keeping — rather than re-deriving —
            // is what stops a collection from installing a space per round.)
            capacity_words
        }
    }

    /// The size (in words) one nursery aims at, given the **whole-heap live set** a collection
    /// kept: the curated size covering [`YOUNG_TARGET_LIVE_MULTIPLE`] x `live_words` plus the
    /// reserve.
    ///
    /// This is the nursery's one sizing *target*, and both of the ways it is used
    /// ([`young_after_collection`](Self::young_after_collection)'s shrink,
    /// [`young_after_fullsweep`](Self::young_after_fullsweep)'s three-way choice) compare a
    /// capacity against it. Two properties make it the right quantity, and neither is a
    /// property of the nursery's own capacity or fill:
    ///
    /// * **It is a workload property.** The live set is what the process's heap *is*, so the
    ///   target is idempotent — re-deriving it at the same live set yields the same capacity,
    ///   which is what makes the capacity a fixed point instead of a ratchet. A target derived
    ///   from the fill or from the current capacity is not: both are ≈ the capacity at a
    ///   collection, so `next_words` would step the capacity up on **every** collection
    ///   (`tests/nursery_bounded.rs` is that regression's gate).
    /// * **It converts the copy volume.** A collection copies the live set and the mutator
    ///   allocates a nursery between collections, so the collector's copy work per allocated
    ///   word is `live / nursery`: a nursery of `k` x the live set divides it by `k`. Below
    ///   `k == 1` the collector copies *more* per op than the mutator allocates, which is
    ///   exactly the `gc_bound/churn` pathology.
    ///
    /// Floored by [`min_heap_words`](Self::min_heap_words) and clamped by
    /// [`max_heap_words`](Self::max_heap_words) through [`next_words`](Self::next_words), so a
    /// `+hms`/`spawn_opt` minimum still sizes every nursery a policy produces.
    pub fn young_target_words(&self, live_words: usize) -> usize {
        self.next_words(
            live_words
                .saturating_mul(YOUNG_TARGET_LIVE_MULTIPLE)
                .saturating_add(self.need_reserve_words),
            0,
        )
    }

    /// The nursery size (in words) to use after a **fullsweep** — the C's **two-step** sizing
    /// reduced to one decision over the two quantities the round measured.
    ///
    /// 1. `major_collection` (`erl_gc.c:1866`) sizes the receiving young heap from the
    ///    **pre-copy** occupancy: `new_sz = next_heap_size(stack_size + size_before + S_RESERVED,
    ///    0)`, where `size_before` is the young usage **plus the old heap's occupancy** — the
    ///    whole heap the round found, which is why `observed_words` is folded together with
    ///    `live_words` here.
    /// 2. `adjust_after_fullsweep` (`erl_gc.c:2062-2098`) then re-sizes from the **post-copy
    ///    live set**: it **grows** when `HEAP_SIZE < need_after` or `3 * HEAP_SIZE < 4 *
    ///    need_after`, keeps while the two are inside that band, and **shrinks** when
    ///    `4 * need_after < HEAP_SIZE && HEAP_SIZE > H_MIN_SIZE`. That is this crate's
    ///    three-way [`should_grow`](Self::should_grow)/keep/[`YOUNG_SHRINK_FACTOR`] decision,
    ///    taken against [`young_target_words`](Self::young_target_words).
    ///
    /// The growth half is the half this crate was missing: a fullsweep is the one collection
    /// that measures the whole-heap live set **exactly** (it copies every live word), so it is
    /// where the nursery learns how large it should be. Without it a nursery could be enlarged
    /// only by an explicit request, so it settles wherever its longest single allocation put it
    /// — for a real workload one to two orders of magnitude below the live set — and every
    /// collection then copies the whole live set (`gc_bound/churn`: a 609-word nursery against
    /// a 464-1353-word live set, 1.5 collections and 1283 copied words per op).
    ///
    /// `observed_words` must be the occupancy the round sampled **before** the copy (so a round
    /// that genuinely filled a larger nursery is not shrunk back), and `live_words` the
    /// **whole-heap** post-copy live count (young *and* mature; a nursery-only count is ≈ 0
    /// after a fullsweep and would collapse the nursery to the reserve floor).
    ///
    /// # Boundedness
    ///
    /// Every branch returns either the current capacity or
    /// [`young_target_words`](Self::young_target_words) = `next_words(k * live + reserve)`,
    /// which depends **only on the live set** — never on the capacity and never on the fill.
    /// Re-deriving it at the same live set therefore yields the same value, so the capacity is
    /// a fixed point as soon as the live set stops growing (a "fill ≈ capacity ⇒ grow" rule
    /// steps the growth table once per collection instead, which is unbounded). The capacity also
    /// stays below `YOUNG_SHRINK_FACTOR` x that target after every collection, so it can never
    /// exceed a fixed multiple of the live set, and a hint of a burst that outlived its data is
    /// corrected at the next fullsweep.
    pub fn young_after_fullsweep(
        &self,
        capacity_words: usize,
        observed_words: usize,
        live_words: usize,
    ) -> usize {
        let target = self.young_target_words(live_words);
        // Step 1 — the C `major_collection` pre-copy occupancy sizing (`erl_gc.c:1866`): the
        // demand is what the round actually found in use, and a demand beyond the capacity is
        // the one unambiguous growth signal (a nursery's occupancy cannot exceed its capacity,
        // so this fires only when the live set has outgrown the nursery).
        let demand = observed_words.max(live_words);
        if demand > capacity_words {
            return self.next_words(demand, 0);
        }
        // Step 2 — the C `adjust_after_fullsweep` post-copy rule (`erl_gc.c:2062-2098`).
        if self.should_grow(capacity_words, target) {
            // `HEAP_SIZE < need_after || 3 * HEAP_SIZE < 4 * need_after`: the nursery's copy
            // volume is above the target's, so stop re-copying the live set every round.
            target
        } else if capacity_words > YOUNG_SHRINK_FACTOR.saturating_mul(target) {
            // Far above the band: the burst the capacity was stepped up for is long dead.
            target.min(capacity_words)
        } else {
            capacity_words
        }
    }
}

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
        assert_eq!(
            next_heap_size(DEFAULT_MIN_HEAP_WORDS, 0),
            DEFAULT_MIN_HEAP_WORDS
        );
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
        assert_eq!(
            HeapPolicy::from_spawn_opts(46_422, 0, 7).min_heap_words,
            46_422
        );
        assert_eq!(
            HeapPolicy::from_spawn_opts(46_423, 0, 7).min_heap_words,
            75113
        );

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
        assert_eq!(capped.mature_after_fullsweep(100_000, 400, 0), 512); // The fullsweep always sees the byte-accounted reserve on top of the live set.
        let default = HeapPolicy::default();
        assert_eq!(
            default.mature_after_fullsweep(12, 100, 100),
            default.next_words(100 + 100 + default.need_reserve_words, 0)
        );
    }

    #[test]
    fn young_after_collection_is_bounded_and_shrinks_when_empty() {
        let policy = HeapPolicy::default();

        // A nursery a round left *full* of live words keeps its size: the capacity is never
        // stepped up merely because a collection ran (the runaway this rule prevents — a
        // nursery is only collected when it is full, so "grow when full" grows on *every*
        // collection).
        assert_eq!(policy.young_after_collection(233, 233), 233);
        assert_eq!(policy.young_after_collection(233, 175), 233);
        assert_eq!(policy.young_after_collection(233, 174), 233);

        // The *only* growth is a surviving set the nursery cannot hold.
        assert_eq!(policy.young_after_collection(233, 234), 376);
        assert_eq!(policy.young_after_collection(233, 1_000), 1_598);
        // `need_reserve_words` (64) is the request's floor, so a nursery smaller than that
        // reserve grows to cover it (12 -> curated 64 -> 90).
        assert_eq!(policy.young_after_collection(12, 0), 90);

        // A nursery the round left mostly empty keeps its size (233 is not > 4 * 64).
        assert_eq!(policy.young_after_collection(233, 13), 233);
        // A grossly over-sized nursery shrinks back towards the live set.
        assert_eq!(policy.young_after_collection(1_000, 13), 90);
        assert_eq!(policy.young_after_collection(1_000, 0), 90);
        // ... and a shrink is idempotent: the shrunk nursery is already about right.
        assert_eq!(policy.young_after_collection(90, 0), 90);

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
        assert_eq!(capped.young_after_collection(233, 1_000), 300);
        assert_eq!(capped.young_after_collection(233, 233), 233);
    }

    /// Step 1 (`major_collection`, `erl_gc.c:1866`) and step 2 (`adjust_after_fullsweep`,
    /// `erl_gc.c:2062-2098`) in sequence, and the clamp/floor interaction of the pair.
    #[test]
    fn young_after_fullsweep_applies_both_steps_in_order() {
        let policy = HeapPolicy::default();

        // (1) Step 1 keeps a nursery the round observed full (233 is not > 4 * 233): the
        // pre-copy-occupancy rule — the churn win `8f74faff19` removed the per-op install with.
        assert_eq!(policy.young_after_collection(2_819, 2_586), 2_819);
        // (2) Step 2 then shrinks that same nursery when almost nothing survived:
        // need_after = 60 + 64 = 124, 4 * 124 = 496 < 2 819 ⇒ target next_words(248, 0) = 376.
        assert_eq!(policy.young_after_fullsweep(2_819, 2_586, 60), 376);
        assert_eq!(policy.next_words(2 * (60 + policy.need_reserve_words), 0), 376);

        // Step 1 alone would have kept 2 819 — the shrink is step 2's, never step 1's.
        assert_ne!(policy.young_after_collection(2_819, 2_586), 376);

        // A live set that is *not* far below the capacity protects it: 4 * (400 + 64) = 1 856 >
        // 1 220 ⇒ no shrink (the `gc_bound/churn` geometry, `MEASURED.md`).
        assert_eq!(policy.young_after_fullsweep(1_220, 987, 400), 1_220);
        assert_eq!(policy.young_after_fullsweep(609, 609, 464), 609);

        // Step 2 never exceeds step 1's size, whatever the inputs (a fullsweep cannot enlarge).
        for &capacity in &[12usize, 38, 90, 233, 376, 987, 1_598, 2_586] {
            for observed in [0, 1, capacity / 2, capacity] {
                let kept = policy.young_after_collection(capacity, observed);
                for live in [0, 1, 60, 233, 1_000, 10_000] {
                    let target = policy.young_after_fullsweep(capacity, observed, live);
                    assert!(target <= kept, "{capacity}/{observed}/{live}: {target} > {kept}");
                }
                // ... and it is a fixed point of itself once shrunk: the gate it just passed
                // cannot fire again, because every curated step is under 4x its request.
                for live in [0usize, 60, 200, 2_000] {
                    let shrunk = policy.young_after_fullsweep(capacity, observed, live);
                    assert_eq!(policy.young_after_fullsweep(shrunk, shrunk, live), shrunk);
                }
            }
        }

        // The reserve is part of `need_after`, so even an empty live set never collapses the
        // nursery below `next_words(2 * need_reserve_words, 0)`.
        assert_eq!(
            policy.young_after_fullsweep(1_000, 1_000, 0),
            policy.next_words(2 * policy.need_reserve_words, 0)
        );

        // `+hms` (`min_heap_words`) floors the shrunk target ...
        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        assert_eq!(floored.young_after_fullsweep(100_000, 100_000, 10), 500);
        // ... and `+hmax` (`max_heap_words`) clamps it, still strictly shrinking.
        let capped = HeapPolicy {
            max_heap_words: 300,
            ..HeapPolicy::default()
        };
        assert_eq!(capped.young_after_fullsweep(100_000, 100_000, 10), 233);
        assert!(capped.young_after_fullsweep(100_000, 100_000, 10) < 100_000);
        // A live set above the cap leaves the clamp — not a fabricated larger target.
        assert_eq!(capped.young_after_fullsweep(233, 233, 5_000), 233);
    }

    /// The C's initial-heap rule (`erl_process.c:12586-12592`): a new process's first heap is
    /// its `min_heap_size`, and a policy with no minimum keeps its `initial_young_words` seed.
    #[test]
    fn initial_heap_words_is_the_minimum_when_one_is_set() {
        // No floor: the seed is the rule (`HeapPolicy::default`'s 233 stands in for the C's
        // `H_MIN_SIZE`), and a caller-set seed is honoured verbatim rather than re-rounded.
        assert_eq!(HeapPolicy::default().initial_heap_words(), 233);
        assert_eq!(
            HeapPolicy {
                initial_young_words: 1024,
                ..HeapPolicy::default()
            }
            .initial_heap_words(),
            1024
        );

        // A floor wins, in **both** directions: `+hms 4000` => 4185 (the oracle's fresh-process
        // `heap_size`), and `+hms 100` => 142 -- *below* the unconfigured 233 seed, because the
        // C's default `H_MIN_SIZE` is simply replaced by the switch.
        assert_eq!(HeapPolicy::from_min_max(4185, 0).initial_heap_words(), 4185);
        assert_eq!(HeapPolicy::from_min_max(142, 0).initial_heap_words(), 142);
        assert_eq!(
            HeapPolicy::from_spawn_opts(0, 0, 65_535).initial_heap_words(),
            233
        );
        assert_eq!(
            HeapPolicy::from_spawn_opts(1000, 0, 65_535).initial_heap_words(),
            1598
        );
        assert_eq!(
            HeapPolicy::from_spawn_opts(8000, 0, 65_535).initial_heap_words(),
            10958
        );
    }

    /// [`HeapPolicy::spawn_policy`] floors a `spawn_opt` request against the **global**
    /// policy's minimums (the C `H_MIN_SIZE`/`BIN_VH_MIN_SIZE`), which is the one authority --
    /// so `+hms 4000` floors `{min_heap_size, 100}` at 4185, and `+hms 100` floors it at 142.
    /// Everything else (`max_heap_size`, the initial space sizes) is inherited from the global.
    #[test]
    fn spawn_policy_floors_against_the_global_policy() {
        let default_global = HeapPolicy {
            min_heap_words: DEFAULT_MIN_HEAP_WORDS,
            ..HeapPolicy::default()
        };
        // The unconfigured emulator: identical to `from_spawn_opts`.
        assert_eq!(
            default_global.spawn_policy(0, 0, Some(65_535)),
            HeapPolicy::from_spawn_opts(0, 0, 65_535)
        );

        for (hms, floor) in [(100usize, 142usize), (1000, 1598), (4000, 4185), (8000, 10958)] {
            let global = HeapPolicy {
                min_heap_words: next_heap_size(hms, 0),
                ..HeapPolicy::default()
            };
            assert_eq!(global.min_heap_words, floor, "+hms {hms}");
            // A below-floor request is replaced by the global (never rounded past it) ...
            assert_eq!(global.spawn_policy(100, 0, None).min_heap_words, floor);
            // ... an above-floor request is rounded through the growth table ...
            assert_eq!(
                global.spawn_policy(20_000, 0, None).min_heap_words,
                next_heap_size(20_000, 0)
            );
            // ... and the global's binary-vheap floor applies likewise.
            assert_eq!(global.spawn_policy(0, 100, None).min_bin_vheap_words, 46_422);
            // An absent `fullsweep_after` inherits the global's; every other field too (the
            // C's `H_MAX_SIZE` included).
            let inherited = global.spawn_policy(0, 0, None);
            assert_eq!(inherited.fullsweep_after, global.fullsweep_after);
            assert_eq!(inherited.initial_mature_words, global.initial_mature_words);
            assert_eq!(inherited.need_reserve_words, global.need_reserve_words);
            assert_eq!(inherited.max_heap_words, global.max_heap_words);
            // The inherited maximum really is the global's.
            let capped = HeapPolicy {
                max_heap_words: 12_345,
                ..global.clone()
            };
            assert_eq!(capped.spawn_policy(0, 0, None).max_heap_words, 12_345);
        }
    }
}
