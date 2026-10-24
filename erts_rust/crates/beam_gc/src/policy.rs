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
///
/// # Where this multiple is *not* applied — the flatness-vs-amortisation trade
///
/// The nursery's **post-minor** rule deliberately uses **one** times the young live set
/// ([`HeapPolicy::young_minor_target_words`]) instead of this multiple, and the two rules are
/// chosen together:
///
/// * a multiple is an **amortisation** trade — a nursery holding `k` live sets lets the mutator
///   allocate `k` live sets between collections, so the collector copies the live set `1/k` as
///   often — and it is the fullsweep that applies one here, because a fullsweep is the one
///   collection that measures the **whole-heap** live set exactly;
/// * a minor's figure is the **young-only** live set, an under-estimate of that whole, so a
///   multiple of it would be a multiple of an under-estimate.
///   One times the figure is the rule instead: the capacity a minor installs is
///   `next_words(young live + reserve)` — a function of the live set, never of the fill the round
///   reached — while [`HeapPolicy::young_after_collection`] keeps an existing capacity anywhere
///   inside `[target, max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)]`, so the growth
///   seam may step the nursery one growth-table entry at a time up to the last entry below that
///   band's upper edge and the capacity then **stops**: a fixed point at a constant live set.
///   Every minor promotes its surviving young words into the mature region, which
///   [`HeapPolicy::mature_target_words`] sizes from `MATURE_TARGET_LIVE_MULTIPLE` × the room one
///   fullsweep leaves behind (`live` + the nursery occupancy a promotion must fit in + the
///   reserve), and [`HeapPolicy::mature_after_fullsweep`] applies the same shape of rule — grown
///   onto that target below it, given back once the capacity exceeds four times it, kept
///   otherwise.
///   Both capacities are therefore functions of the live set alone, which is what makes the pair
///   settle rather than ratchet on the call count.
/// * a larger minor multiple would amortise further and is **excluded**: it moves the converged
///   young capacity off the live set's own curated size, which is exactly the figure the
///   dependents pin.
///   On a 401-word live set those curated sizes are `young_target_words(401) =` **987** words (the
///   fullsweep target) and `young_minor_target_words(401) =` **610** (the minor target, the keep
///   band's lower edge), and `beam_rt`'s `reds_door` measures the nursery converging on the
///   987-word fullsweep target at both N = 2 000 and N = 20 000, its `slice_charging` interpreter
///   control reading the **233**-word floor (`young_minor_target_words(0)`).
///   The 2× memory-for-amortisation trade therefore lives on the **mature** side
///   ([`MATURE_TARGET_LIVE_MULTIPLE`], where the same 2× buys a *run* of minors between
///   fullsweeps) and only one live-set multiple **2** is applied per generation.
///
/// **The settled figures are measured, not read off the rule.** Both capacities are stated above
/// as rules, and the values a particular workload lands on are whatever those rules reproduce at
/// its live set — so they move whenever a sizing rule moves, and quoting one here is a
/// measurement of the *current* geometry rather than part of the contract.
/// For the current geometry, the dependent `beam_rt` `boundary_cost` churn fixture holds one
/// **2 585**-word young space and a **6 772**-word mature region at **2.36 collections per
/// call** over its 2 001 calls (its heap-admission and boundary doors together).
/// The young figure is a **chain sum materialised into a single young space** by
/// [`HeapPolicy::young_after_collection`]'s keep branch, and not a growth-table entry — nor is it
/// meant to be one: 2 585 is not in the table (2 586 is) because nothing ever picked it from there.
/// The growth seam had taken the chain to `987 + 1598 = 2 585` words — a 987-word space a growth
/// retired plus the 1 598-word one it installed — and that sum lies inside the keep band
/// `[target, max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)]` around the fixture's own
/// 376-word minor target (`young_minor_target_words(201) = next_words(201 + 64)`); the floor does
/// not move that band's edge here, because `YOUNG_SHRINK_FACTOR × 376 = 3 008 > 3 000`.
/// The keep branch therefore answers with the figure it was handed, and the collection installs it
/// as **one** space: 2 585 words, 3.2 of the fixture's 806-word iterations of allocation.
/// The identical materialisation is visible one step earlier in the runtime's own heap trace
/// (`BEAM_RT_GC_TRACE=1` on the same fixture): its `young_cap` readings are `610` and
/// `1597 = 610 + 987`, the same kept chain sum one collection earlier, and `1597` is no more a
/// growth-table entry than `2 585` is.
/// `HeapSpace::capacity_words` reports the constructed capacity verbatim, so a capacity outside
/// the table needs no arithmetic story of its own and no word is "accounted for elsewhere".
/// The mature one is `next_words(2 × (201 + 2 585 + 64)) = next_words(5 700) = 6 772`.
/// The canonical table and the protocol are `beam_rt`'s (`src/runner/tests/boundary_cost.rs`) and
/// are re-measured, never recomputed, whenever either rule changes.
pub const YOUNG_TARGET_LIVE_MULTIPLE: usize = 2;

/// The factor by which a nursery must exceed its live-set-derived target
/// ([`HeapPolicy::young_target_words`] after a fullsweep,
/// [`HeapPolicy::young_minor_target_words`] after a minor) before a collection gives the memory
/// back.
///
/// This is the hysteresis band's upper edge — for the **fullsweep** rule
/// ([`HeapPolicy::young_after_fullsweep`]) that is the whole story, and its
/// [`HeapPolicy::mature_after_fullsweep`] counterpart reads the same factor. The **minor** rule
/// ([`HeapPolicy::young_after_collection`]) has one more edge above this one: it also refuses to
/// hand back a capacity at or below [`MINOR_SHRINK_FLOOR_WORDS`] (the C's `HEAP_SIZE(p) > 3000`
/// gate, `erts/emulator/beam/erl_gc.c:1517-1529`), so a minor's keep band is
/// `[target, max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)]`.
///
/// Both of the factor's properties matter:
///
/// * it must be **wider than the live set's own fluctuation** (`YOUNG_TARGET_LIVE_MULTIPLE`
///   times the ~3× a partially built term swings a live set) or a collection that samples the
///   live set at a trough shrinks a nursery the next one has to grow back — an oscillation,
///   not a fixed point; and
/// * it must still be **finite and unrelated to the capacity**, so a nursery grown for a burst
///   returns to the live set's scale once the burst is dead (`tests/nursery_bounded.rs`), and
///   so that the growth rule can never feed itself.
///
/// The floor does not weaken the second property: it is a constant, so a kept capacity is still
/// bounded by a function of the live set alone
/// (`max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)`), never by the iteration count.
pub const YOUNG_SHRINK_FACTOR: usize = 8;

/// The floor below which a **minor** never hands nursery capacity back, in words — the C
/// `HEAP_SIZE(p) > 3000` conjunct of the minor resizer's shrink gate
/// (`erts/emulator/beam/erl_gc.c:1517-1529`).
///
/// `HEAP_SIZE(p)` is the **young** generation's capacity in term **words**
/// (`erts/emulator/beam/erl_process.h:1017`; words, not bytes, and not the old/mature heap) —
/// exactly the quantity [`HeapPolicy::young_after_collection`]'s `capacity_words` parameter stands
/// for (a chain sum here, one space in the C — both are "words of young-heap capacity"). The C's
/// own rationale for the floor:
///
/// > Excessively large heaps should be shrunk, but don't even bother on reasonable small heaps.
/// >
/// > The reason for this is that after tenuring, we often use a really small portion of new heap,
/// > therefore, unless the heap size is substantial, we don't want to shrink.
///
/// A minor has just promoted its survivors out of the young heap, so the young heap's *live*
/// portion is a small fraction of its capacity whatever the capacity is. Without the floor the
/// shrink rule therefore reads a live-set trough on a **small** heap as eagerly as on a large one
/// and hands the capacity back, which the next round's allocation has to grow again — a
/// shrink/grow oscillation. The floor makes that impossible: a capacity at or below this figure is
/// kept.
///
/// # What is deliberately **not** mirrored
///
/// The C's gate is a conjunction of three tests; only the floor is reproduced here.
///
/// * `4 * need_after < HEAP_SIZE(p)` — the C shrinks once the post-collection need is under a
///   quarter of the young heap. This crate's band is **stricter**:
///   [`YOUNG_SHRINK_FACTOR`] (`8`) times the live-set target instead of `4` times the need, so
///   every capacity the C's first conjunct would let shrink, this crate also considers (and
///   more). The floor is the one place the crate would otherwise be *less* conservative than the
///   C, which is why it is the conjunct added here.
/// * `HEAP_SIZE(p) > 8000 || HEAP_SIZE(p) > (OLD_HEND(p) - OLD_HEAP(p))` — the C keeps a young
///   heap that is not large compared with the **old** heap. There is no counterpart here: the
///   mature generation is sized by [`HeapPolicy::mature_target_words`] from the live set a
///   fullsweep measured ([`MATURE_TARGET_LIVE_MULTIPLE`]), never from the nursery's own scale, so
///   a young-vs-old size relation carries no information the band does not already encode.
///
/// The **fullsweep** resizer is a different rule (`adjust_after_fullsweep`,
/// `erts/emulator/beam/erl_gc.c:2061-2098`) gated on `HEAP_SIZE(p) > H_MIN_SIZE` — a
/// minimum-heap test, not a 3000-word one — so [`HeapPolicy::young_after_fullsweep`] deliberately
/// does **not** apply this floor; its band stays `[target, YOUNG_SHRINK_FACTOR × target]`.
pub const MINOR_SHRINK_FLOOR_WORDS: usize = 3000;

/// How much of the **room a fullsweep must leave behind** the mature generation sizes itself
/// to ([`HeapPolicy::mature_target_words`]).
///
/// The mature generation is the destination of every minor, so the quantity it is sized from is
/// the room one fullsweep has to guarantee: the exact live set it kept, plus one whole nursery's
/// worth of promotion (the C `adjust_after_fullsweep`'s `need_after`,
/// `erl_gc.c:2072`), plus the reserve. [`Collector::collect`](crate::Collector::collect) only
/// allows a minor while the mature space has that much room left
/// (`mature.available_words() >= nursery.used_words()`), so a mature space of exactly
/// `need_after` admits **one** promotion and the next collection is a fullsweep again.
///
/// This is [`YOUNG_TARGET_LIVE_MULTIPLE`]'s counterpart on the other generation: the nursery
/// holds `2` live sets so the mutator can allocate a whole nursery before the live set is copied
/// again, and the mature generation holds `2` `need_after` budgets so **a run of minors** — not
/// a single one — fits between fullsweeps. The trade is identical and it is the same one: the
/// multiple divides the fullsweeps (each of which copies the whole live set) by `2`, at a memory
/// cost of `2` times the room a fullsweep's result needs.
///
/// The figure is a property of the **live set the fullsweep measured**, never of the mature
/// space's own capacity or occupancy, which is what makes the resulting capacity a fixed point
/// rather than a ratchet (`tests/mature_capacity_convergence.rs`).
pub const MATURE_TARGET_LIVE_MULTIPLE: usize = 2;
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

    /// The smallest size (in words) **strictly greater** than `size` — the "one step further
    /// than the space it replaces" size a growth installs.
    ///
    /// This is the C's `erts_next_heap_size(size, 1)` (`erl_gc.c:310-338`, used at
    /// `:1886-1888`): the growth path takes the table entry *after* the current size, which is
    /// the "never the same size when **growing**" rule. [`next_words`](Self::next_words) with
    /// `offset == 1` answers that entry, and this accessor completes it into a total function:
    /// past the growth table's top (or under a [`min_heap_words`](Self::min_heap_words) floor
    /// above every entry) the table cannot name a larger size, so the answer is `size + 1`. It is
    /// therefore **always** strictly greater than `size`, which is the property the no-relocation
    /// growth seam needs: a growth must never install a space no larger than the one it retires.
    ///
    /// A clamp by [`max_heap_words`](Self::max_heap_words) can still place the answer below
    /// `size`, which is the caller's explicit ceiling, not a sizing rule.
    pub fn next_words_above(&self, size: usize) -> usize {
        let stepped = self.next_words(size, 1);
        if stepped > size {
            stepped
        } else {
            size.saturating_add(1)
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

    /// The room a fullsweep must leave behind (in words), given the `live_words` it kept and the
    /// `young_occupancy_words` still to be promoted — the C's `need_after`
    /// (`erl_gc.c:2072`).
    ///
    /// It is the **un-multiplied** quantity: enough for the live set, one whole nursery's worth
    /// of promotion and the reserve. [`Collector::collect`](crate::Collector::collect) admits a
    /// minor only while the mature space has at least one nursery's occupancy available, so this
    /// is the smallest mature capacity a generational cadence runs at all.
    pub fn need_after_fullsweep(&self, live_words: usize, young_occupancy_words: usize) -> usize {
        live_words
            .saturating_add(young_occupancy_words)
            .saturating_add(self.need_reserve_words)
    }

    /// The mature-space size (in words) one fullsweep aims at, given the **exact live set** it
    /// measured and the young heap's occupancy still to be promoted: the curated size covering
    /// [`MATURE_TARGET_LIVE_MULTIPLE`] x [`need_after_fullsweep`](Self::need_after_fullsweep).
    ///
    /// This is [`young_target_words`](Self::young_target_words)'s counterpart on the mature
    /// generation, and it has the same two load-bearing properties:
    ///
    /// * **It is a workload property.** Its input is the live set a fullsweep measured, so
    ///   re-deriving it at the same live set yields the same capacity — a fixed point, not a
    ///   ratchet. A target derived from the mature space's occupancy or capacity is not: a
    ///   generational heap is *full* when the cadence fires, so such a rule steps the growth
    ///   table once per fullsweep without bound (`tests/mature_capacity_convergence.rs` is that
    ///   regression's gate).
    /// * **It converts the fullsweep count.** One `need_after` admits exactly one minor
    ///   promotion before the mature space is too full to take another, so the multiple is
    ///   literally the number of promotions one mature space is sized for; the copy work a
    ///   fullsweep pays (the whole live set) is divided by it.
    ///
    /// Floored by [`min_heap_words`](Self::min_heap_words) and clamped by
    /// [`max_heap_words`](Self::max_heap_words) through [`next_words`](Self::next_words), so a
    /// `+hms`/`spawn_opt` minimum still sizes every mature space a policy produces and a capped
    /// policy's target is the cap itself.
    pub fn mature_target_words(&self, live_words: usize, young_occupancy_words: usize) -> usize {
        self.next_words(
            self.need_after_fullsweep(live_words, young_occupancy_words)
                .saturating_mul(MATURE_TARGET_LIVE_MULTIPLE),
            0,
        )
    }

    /// The mature-space size (in words) to use after a fullsweep that kept `live_words`
    /// alive and still needs `need_words` — the C `adjust_after_fullsweep` decision
    /// (`erl_gc.c:2061-2098`) reduced to one target size.
    ///
    /// The **target** is [`mature_target_words`](Self::mature_target_words):
    /// [`MATURE_TARGET_LIVE_MULTIPLE`] x
    /// [`need_after_fullsweep`](Self::need_after_fullsweep), where the un-multiplied
    /// `need_after = live_words + need_words + need_reserve_words` is the room the collection
    /// must leave behind (`erl_gc.c:2072`). One `need_after` admits exactly one minor
    /// promotion, so the multiple is what lets a **run** of minors fit between fullsweeps (see
    /// [`MATURE_TARGET_LIVE_MULTIPLE`]). The current `capacity_words` is
    ///
    /// * **grown** onto the target when it is **below** it — the space is too small for its own
    ///   result, and a generational heap too small for one promotion could not run a single
    ///   minor;
    /// * **shrunk** back onto the target when [`should_shrink`](Self::should_shrink) holds — the
    ///   live set fell four times below what the space is sized for, so the memory goes back;
    /// * otherwise **kept** as it is, which makes the target a fixed point.
    ///
    /// Both brackets are keyed on the **target**, not on the un-multiplied `need_after`, and the
    /// grow side is the strict `capacity < target` rather than [`should_grow`](Self::should_grow):
    /// `should_grow`'s "less than a quarter of headroom left" arm fires for a capacity that is
    /// one growth-table step **above** the target, which would make a branch named *grow* shrink
    /// a space and — for the mark-region mature generation — turn an in-place reclaim into a
    /// relocation. Keying both brackets on the target and keeping them disjoint makes each
    /// branch install the target in the direction its comparison established: a "grow" only
    /// ever enlarges and a "shrink" only ever makes the space smaller.
    ///
    /// The rule reads the **live set** (through `need_after`) and never the mature space's own
    /// occupancy, so it cannot ratchet.
    ///
    /// [`min_heap_words`](Self::min_heap_words) floors and
    /// [`max_heap_words`](Self::max_heap_words) clamps every result through
    /// [`next_words`](Self::next_words) — including the target itself, so a clamped policy has
    /// the clamped target as its fixed point.
    pub fn mature_after_fullsweep(
        &self,
        capacity_words: usize,
        live_words: usize,
        need_words: usize,
    ) -> usize {
        let target = self.mature_target_words(live_words, need_words);
        if capacity_words < target {
            target
        } else if self.should_shrink(capacity_words, target) {
            target.min(capacity_words)
        } else {
            capacity_words
        }
    }

    /// The nursery size (in words) to use after a **minor**, from the `capacity_words` the
    /// nursery chain had when it was collected and the **young live set** the round evacuated —
    /// the words it promoted, `GcStats::words_copied`.
    ///
    /// A minor's copy volume **is** its young live set, so the capacity it aims at is
    /// [`young_minor_target_words`](Self::young_minor_target_words) of that figure — the
    /// smallest curated capacity at which the round's own survival does not by itself force the
    /// next collection. The rule is a three-way choice over that target, with the keep band
    /// `[target, max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)]`:
    ///
    /// * a capacity **above** the band's upper edge installs the target: the live set fell far
    ///   below what the nursery is sized for, so the memory goes back;
    /// * a capacity **below** the target installs the target — a minor *does* grow the nursery,
    ///   but only onto a capacity **derived from the live set**, never one derived from the
    ///   capacity it had or the fill it reached: both are ≈ the capacity at a collection, so a
    ///   rule reading either steps the growth table once per collection without bound. **The
    ///   growth branch has no floor**: a nursery below its live set's target grows onto it even
    ///   below [`MINOR_SHRINK_FLOOR_WORDS`] (`target > capacity` wins over the floor);
    /// * otherwise the capacity is kept, which makes the target a fixed point.
    ///
    /// # The keep band's two edges
    ///
    /// The upper edge is the **larger** of [`YOUNG_SHRINK_FACTOR`] × the target and
    /// [`MINOR_SHRINK_FLOOR_WORDS`], and the two exist for different reasons:
    ///
    /// * [`YOUNG_SHRINK_FACTOR`] × the target is what absorbs the live set's own swing across
    ///   collections — a narrower band oscillates on a live set that moves within one operation;
    /// * [`MINOR_SHRINK_FLOOR_WORDS`] is the legacy C's minor-shrink floor (`HEAP_SIZE(p) > 3000`,
    ///   `erts/emulator/beam/erl_gc.c:1517-1529`): a nursery at or below 3 000 words of young
    ///   capacity is **never** handed back, however small the round's live set is. After tenuring
    ///   a minor always leaves a small live portion of a small heap, so an ungated shrink rule
    ///   reads that trough as "give the memory back" and forces the growth seam to climb straight
    ///   back — the shrink/grow oscillation the C's comment describes.
    ///
    /// The floor is a constant, so the band is still a function of the live set alone
    /// (`max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)`), and every result is still
    /// exactly `target` or exactly `capacity_words` — a fixed point, never a third value.
    ///
    /// # Why the **fullsweep** rule has no floor
    ///
    /// The C reaches its fullsweep decision through a *different* resizer
    /// (`adjust_after_fullsweep`, `erts/emulator/beam/erl_gc.c:2061-2098`) gated on
    /// `HEAP_SIZE(p) > H_MIN_SIZE` — a minimum-heap test, not a 3 000-word one — so the 3 000-word
    /// floor is a **minor-only** rule and
    /// [`young_after_fullsweep`](Self::young_after_fullsweep) keeps its plain
    /// `[target, YOUNG_SHRINK_FACTOR × target]` band. A fullsweep has also measured the whole live
    /// set exactly, so its figure is not a post-tenuring trough.
    ///
    /// Because the growth branch installs the target, a collection can never leave a nursery
    /// below [`nursery_floor_words`](Self::nursery_floor_words): the floor is a real lower bound
    /// on every capacity this rule installs, not an emergent `max()`.
    ///
    /// A minor's figure is the **young-only** live set, an under-estimate of the heap's whole
    /// live set (the mature generation is not part of it) — which is why the minor target is
    /// [`young_minor_target_words`](Self::young_minor_target_words) (one times the live set) and
    /// not the [`young_target_words`](Self::young_target_words) the fullsweep applies.
    ///
    /// # The `need` term — [`young_after_collection_with_need`](Self::young_after_collection_with_need)
    ///
    /// [`young_after_collection_with_need`](Self::young_after_collection_with_need) is the same
    /// three-way rule with the legacy C's `need` term in the target; this method is its
    /// `need == 0` case, so every figure documented above is unchanged.
    pub fn young_after_collection(&self, capacity_words: usize, live_words: usize) -> usize {
        self.young_after_collection_with_need(capacity_words, live_words, 0)
    }

    /// [`young_after_collection`](Self::young_after_collection) with the legacy C's `need` term:
    /// the **allocation request that triggered the collection**, in words.
    ///
    /// The C never sizes a young heap from the live set alone. `adjust_after_minor`
    /// (`erts/emulator/beam/erl_gc.c:1490-1555`) computes
    ///
    /// ```c
    /// need_after = ((HEAP_TOP(p) - HEAP_START(p))   /* young occupancy after the collection */
    ///               + need                          /* the pending allocation request */
    ///               + stack_size);
    /// ...
    /// else if (need_after > HEAP_SIZE(p)) grow_new_heap(p, next_heap_size(p, need_after, 0), ...);
    /// ```
    ///
    /// and `adjust_after_fullsweep` (`erl_gc.c:2061-2098`) has the same shape
    /// (`need_after = (HEAP_TOP - HEAP_START) + need + stack_size + S_RESERVED`);
    /// `erts_garbage_collect(p, need, objv, nobj)` carries that `need` **into** the collection. In
    /// this crate the young occupancy *is* the live set a minor evacuated, and the C's
    /// `stack_size + S_RESERVED` map onto this policy's
    /// [`need_reserve_words`](Self::need_reserve_words), so the target's argument is
    /// `live + need + need_reserve_words` — exactly the shape
    /// [`size_after_fullsweep`](Self::size_after_fullsweep) already uses.
    ///
    /// `need_words` is the **allocation** request, unrelated to the promotion-room figure
    /// `Collector::minor_room_words` computes from the mature space's free room. The term is
    /// strictly **additive** (`need_words == 0` is
    /// [`young_after_collection`](Self::young_after_collection), byte for byte) and the target
    /// remains a function of the live set and **one** pending request — never of the call count —
    /// so the fixed-point property survives: a repeated `ensure_capacity(n)` at a constant live
    /// set and request reproduces the capacity it installed.
    ///
    /// The three-way rule, its keep band and [`MINOR_SHRINK_FLOOR_WORDS`] are **identical** to
    /// [`young_after_collection`](Self::young_after_collection); only the target the capacity is
    /// compared against gains the `need` term.
    pub fn young_after_collection_with_need(
        &self,
        capacity_words: usize,
        live_words: usize,
        need_words: usize,
    ) -> usize {
        let target = self.young_minor_target_words_with_need(live_words, need_words);
        // The legacy C's `HEAP_SIZE(p) > 3000` conjunct (`erl_gc.c:1517`): a small young heap is
        // kept even when the round left it nearly empty. Strict, exactly like the C's `>`.
        let shrink_band = MINOR_SHRINK_FLOOR_WORDS.max(YOUNG_SHRINK_FACTOR.saturating_mul(target));
        if capacity_words > shrink_band || capacity_words < target {
            // Either the nursery is far larger than the live set it just promoted (give the
            // memory back), or it is smaller than the live set's own target (enlarge it onto
            // that target — the floor never blocks a growth). Both branches land **on** the
            // target — a capacity the rule reproduces unchanged — so the capacity is a fixed
            // point, not a ratchet.
            target
        } else {
            // The nursery is inside its band: keep it. (Keeping — rather than re-deriving —
            // is what stops a collection from installing a space per round.)
            capacity_words
        }
    }

    /// The size (in words) a nursery aims at after a **minor**, given the **young live set** the
    /// round evacuated — its copy volume, `GcStats::words_copied`.
    ///
    /// It is **one times** the young live set plus the reserve:
    /// `next_words(live_words + need_reserve_words, 0)`. Deliberately *not*
    /// [`YOUNG_TARGET_LIVE_MULTIPLE`] times: a minor's figure is the young-**only** live set, an
    /// **under**-estimate of the heap's whole live set, and the curve
    /// `next_words(live + reserve)` is the smallest curated capacity at which the round's own
    /// survival does not itself force the next collection. The 2× memory-for-copy-amortisation
    /// trade belongs to the **fullsweep** ([`young_target_words`](Self::young_target_words)) —
    /// the one place the whole-heap live set is measured exactly.
    ///
    /// It is monotone in `live_words`, floored by
    /// [`min_heap_words`](Self::min_heap_words) and clamped by
    /// [`max_heap_words`](Self::max_heap_words) through [`next_words`](Self::next_words), and it
    /// is therefore never below [`nursery_floor_words`](Self::nursery_floor_words):
    /// `live_words + need_reserve_words >= need_reserve_words` already goes through the same
    /// monotone lookup. `young_minor_target_words(0) == nursery_floor_words()`.
    ///
    /// # The `need` term — [`young_minor_target_words_with_need`](Self::young_minor_target_words_with_need)
    ///
    /// [`young_minor_target_words_with_need`](Self::young_minor_target_words_with_need) is the
    /// same rule with the legacy C's `need` term added to the argument; this method is its
    /// `need == 0` case, so every figure documented above is unchanged.
    pub fn young_minor_target_words(&self, live_words: usize) -> usize {
        self.young_minor_target_words_with_need(live_words, 0)
    }

    /// [`young_minor_target_words`](Self::young_minor_target_words) with the legacy C's `need`
    /// term: the **allocation request that triggered the collection**, in words.
    ///
    /// The C's minor resizer sizes from `need_after = (HEAP_TOP - HEAP_START) + need +
    /// stack_size` — `(HEAP_TOP - HEAP_START)` is the young occupancy the collection leaves,
    /// `need` the pending request, and `stack_size + S_RESERVED` map onto this policy's
    /// [`need_reserve_words`](Self::need_reserve_words) — and grows onto
    /// `next_heap_size(p, need_after, 0)` when `need_after > HEAP_SIZE(p)`
    /// (`erts/emulator/beam/erl_gc.c:1490-1555`). This is that rule: `next_words(live +
    /// need + need_reserve_words, 0)`.
    ///
    /// `need_words` is the **allocation** request that triggered the collection, unrelated to the
    /// promotion-room figure `Collector::minor_room_words`
    /// computes from the mature space's free room. The term is **additive**:
    /// `need_words == 0` is exactly
    /// [`young_minor_target_words`](Self::young_minor_target_words), so the documented figures
    /// (`610` for a 401-word live set on the default policy) are unchanged, and the target is
    /// still a function of the live set and **one** pending request — never of the call count —
    /// so it stays a fixed point rather than a ratchet. Because
    /// [`next_words`](Self::next_words) is monotone and the argument only grows, a positive
    /// `need_words` never lowers the target.
    pub fn young_minor_target_words_with_need(
        &self,
        live_words: usize,
        need_words: usize,
    ) -> usize {
        self.next_words(
            live_words
                .saturating_add(need_words)
                .saturating_add(self.need_reserve_words),
            0,
        )
    }

    /// The smallest capacity this policy's nursery rule can install, in words —
    /// `next_words(need_reserve_words, 0)`.
    ///
    /// It is the target of an **empty** live set, and both targets reduce to it there:
    /// `live_words == 0` makes [`young_minor_target_words`](Self::young_minor_target_words) and
    /// [`young_target_words`](Self::young_target_words) return exactly this value. Every other
    /// target is at least it — both go through the monotone [`next_words`](Self::next_words) on an
    /// argument of at least `need_reserve_words` — and
    /// [`young_after_collection`](Self::young_after_collection)'s keeping branch only keeps a
    /// capacity that already sits at or above the target. The floor is therefore a real lower
    /// bound on every capacity the policy's collection rule installs, not an emergent `max()`.
    ///
    /// # The guarantee, as a derivation
    ///
    /// Both targets are this figure's own lookup applied to a non-negative augmentation of
    /// [`need_reserve_words`], so `young_minor_target_words(live_words) >=
    /// nursery_floor_words()` and `young_target_words(live_words) >= nursery_floor_words()` hold
    /// for **every** `live_words` — `next_words` is monotone in its argument and both call it on
    /// `live_words + need_reserve_words` (`=> need_reserve_words`) and
    /// `2 * live_words + need_reserve_words` respectively — and both collapse to it exactly at
    /// `live_words == 0`.
    /// Because each collapse seam **installs** its target whenever the capacity is below it, no
    /// collection can leave a nursery below this figure either: the floor is a lower bound on the
    /// result of a collection, not only on a derivation.
    /// That is the invariant a dependent re-attempting a refused allocation can assert against
    /// (the `beam_process` collect-and-retry behind `Process::ensure_capacity` /
    /// `Process::deliver`), rather than a number every caller has to check.
    ///
    /// Concrete figures: [`HeapPolicy::default`] (no `min_heap_words`) answers **90** words;
    /// a spawn policy ([`HeapPolicy::from_spawn_opts`] / [`HeapPolicy::default`]
    /// [`.spawn_policy`](Self::spawn_policy), whose minimum is [`DEFAULT_MIN_HEAP_WORDS`])
    /// answers exactly **233** words — i.e. `young_minor_target_words(0) == 233`.
    ///
    /// The one way below the reserve is the caller's own ceiling: a `max_heap_words` under
    /// `next_words(need_reserve_words, 0)` clamps it down through
    /// [`next_words`](Self::next_words).
    ///
    /// [`initial_heap_words`](Self::initial_heap_words) is a different quantity and is
    /// deliberately not derived from this one: a fresh process's **first** heap is the minimum
    /// the oracle reports as `heap_size`, which for the default policy must stay exactly
    /// [`DEFAULT_MIN_HEAP_WORDS`].
    pub fn nursery_floor_words(&self) -> usize {
        self.next_words(self.need_reserve_words, 0)
    }

    /// The size (in words) one nursery aims at, given the **whole-heap live set** a collection
    /// kept: the curated size covering [`YOUNG_TARGET_LIVE_MULTIPLE`] x `live_words` plus the
    /// reserve.
    ///
    /// This is the nursery's sizing target after a **fullsweep** — the whole-heap rule, whose
    /// young-only counterpart a minor uses
    /// ([`young_minor_target_words`](Self::young_minor_target_words)) — and
    /// [`young_after_fullsweep`](Self::young_after_fullsweep)'s three-way choice compares a
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
    /// `+hms`/`spawn_opt` minimum still sizes every nursery a policy produces. Like
    /// [`young_minor_target_words`](Self::young_minor_target_words) it is never below
    /// [`nursery_floor_words`](Self::nursery_floor_words) for any `live_words` — its argument
    /// `2 * live_words + need_reserve_words` is at least the reserve, and `next_words` is
    /// monotone — and an empty live set makes it *exactly* the floor.
    ///
    /// # The `need` term — [`young_target_words_with_need`](Self::young_target_words_with_need)
    ///
    /// [`young_target_words_with_need`](Self::young_target_words_with_need) is the same rule with
    /// the legacy C's `need` term inside the multiple; this method is its `need == 0` case, so
    /// every figure documented above is unchanged.
    pub fn young_target_words(&self, live_words: usize) -> usize {
        self.young_target_words_with_need(live_words, 0)
    }

    /// [`young_target_words`](Self::young_target_words) with the legacy C's `need` term: the
    /// **allocation request that triggered the collection**, in words.
    ///
    /// `adjust_after_fullsweep` (`erts/emulator/beam/erl_gc.c:2061-2098`) computes
    /// `need_after = (HEAP_TOP - HEAP_START) + need + stack_size + S_RESERVED` and grows onto
    /// `next_heap_size(p, need_after, 0)`; `erts_garbage_collect(p, need, objv, nobj)` carries
    /// that `need` into the collection. `(HEAP_TOP - HEAP_START)` is the occupancy the collection
    /// leaves (the live set here) and `stack_size + S_RESERVED` map onto this policy's
    /// [`need_reserve_words`](Self::need_reserve_words). With the
    /// [`YOUNG_TARGET_LIVE_MULTIPLE`] amortisation this is
    /// `next_words(YOUNG_TARGET_LIVE_MULTIPLE * (live + need) + need_reserve_words, 0)`.
    ///
    /// `need_words` is the **allocation** request that triggered the collection, unrelated to the
    /// promotion-room figure `Collector::minor_room_words`
    /// computes. The term is **additive** — `need_words == 0` reproduces
    /// [`young_target_words`](Self::young_target_words) exactly, so `987` for a 401-word live set
    /// on the default policy is unchanged — and the target remains a function of the live set and
    /// **one** pending request, never of the call count. `next_words` is monotone in an argument
    /// that only grows, so a positive `need_words` never lowers it.
    pub fn young_target_words_with_need(&self, live_words: usize, need_words: usize) -> usize {
        self.next_words(
            live_words
                .saturating_add(need_words)
                .saturating_mul(YOUNG_TARGET_LIVE_MULTIPLE)
                .saturating_add(self.need_reserve_words),
            0,
        )
    }

    /// The nursery size (in words) to use after a **fullsweep**, from the `capacity_words` the
    /// nursery chain had when it was collected and the **whole-heap live set** the collection
    /// kept.
    ///
    /// A fullsweep is the one collection that measures the live set **exactly** (it copies
    /// every live word), so it is where the nursery learns how large it should be. The target
    /// is [`young_target_words`](Self::young_target_words) of the live set, and the capacity is
    /// **grown** to that target whenever [`should_grow`](Self::should_grow) holds (`capacity <
    /// target` or `3 * capacity < 4 * target`), **shrunk** back to it once the live set has
    /// fallen [`YOUNG_SHRINK_FACTOR`] times below it, and otherwise **kept**.
    ///
    /// The C reaches the same decision in two steps — `major_collection` sizes the receiving
    /// young heap from the **pre-copy** occupancy (`erl_gc.c:1866`) and
    /// `adjust_after_fullsweep` re-sizes from the **post-copy** live set (`erl_gc.c:2062-2098`)
    /// — because a C young heap is only part of the process heap, so the occupancy a
    /// collection samples is a demand signal of its own. Here the two generations are separate
    /// spaces and the nursery's occupancy is bounded by its own capacity
    /// (`Nursery::used_words() <= Nursery::capacity_words()`), so the occupancy can never
    /// signal growth beyond the live set: the pre-copy step collapses into the live-set
    /// decision.
    ///
    /// # Boundedness
    ///
    /// Every branch returns either the current capacity or
    /// [`young_target_words`](Self::young_target_words) = `next_words(k * live + reserve)`,
    /// which depends **only on the live set** — never on the capacity and never on the fill.
    /// Re-deriving it at the same live set therefore yields the same value, so the capacity is
    /// a fixed point as soon as the live set stops growing (a "fill ≈ capacity ⇒ grow" rule
    /// steps the growth table once per collection instead, which is unbounded).
    ///
    /// The rule is also idempotent **in one application**: a result of `target` is reproduced
    /// by the next call (`3 * target < 4 * target` holds at `capacity == target`), and a kept
    /// capacity stays inside `[4/3 * target, YOUNG_SHRINK_FACTOR * target]`, where neither
    /// branch fires. The nursery therefore never exceeds [`YOUNG_SHRINK_FACTOR`] times its
    /// target, and a burst that outlived its data is corrected at the next fullsweep.
    ///
    /// # The `need` term — [`young_after_fullsweep_with_need`](Self::young_after_fullsweep_with_need)
    ///
    /// [`young_after_fullsweep_with_need`](Self::young_after_fullsweep_with_need) is the same
    /// three-way rule with the legacy C's `need` term in the target; this method is its
    /// `need == 0` case, so every figure documented above is unchanged.
    pub fn young_after_fullsweep(&self, capacity_words: usize, live_words: usize) -> usize {
        self.young_after_fullsweep_with_need(capacity_words, live_words, 0)
    }

    /// [`young_after_fullsweep`](Self::young_after_fullsweep) with the legacy C's `need` term:
    /// the **allocation request that triggered the collection**, in words.
    ///
    /// The C's `adjust_after_fullsweep` decision (`erts/emulator/beam/erl_gc.c:2061-2098`) is
    /// keyed on `need_after = (HEAP_TOP - HEAP_START) + need + stack_size + S_RESERVED`, where
    /// `(HEAP_TOP - HEAP_START)` is the occupancy the collection leaves (the live set here),
    /// `need` the pending request, and `stack_size + S_RESERVED` map onto this policy's
    /// [`need_reserve_words`](Self::need_reserve_words);
    /// `erts_garbage_collect(p, need, objv, nobj)` carries that `need` into the collection.
    ///
    /// The three-way rule is **identical** to
    /// [`young_after_fullsweep`](Self::young_after_fullsweep), including its floor-free
    /// `[target, YOUNG_SHRINK_FACTOR × target]` band (the C's fullsweep gate is `H_MIN_SIZE`, not
    /// the minor-only 3 000-word one) and its [`should_grow`](Self::should_grow) bracket; only the
    /// `target` it compares against gains the `need` term
    /// ([`young_target_words_with_need`](Self::young_target_words_with_need)).
    ///
    /// `need_words` is the **allocation** request, unrelated to the promotion-room figure
    /// `Collector::minor_room_words` computes. The term is
    /// **additive** — `need_words == 0` is
    /// [`young_after_fullsweep`](Self::young_after_fullsweep) exactly — and the target stays a
    /// function of the live set and **one** pending request, never of the call count, so the
    /// capacity remains a fixed point.
    pub fn young_after_fullsweep_with_need(
        &self,
        capacity_words: usize,
        live_words: usize,
        need_words: usize,
    ) -> usize {
        let target = self.young_target_words_with_need(live_words, need_words);
        if capacity_words > YOUNG_SHRINK_FACTOR.saturating_mul(target)
            || self.should_grow(capacity_words, target)
        {
            // Either the nursery re-copies the live set too often to pay for itself, or the
            // burst it was stepped up for is long dead: both want the live set's target.
            target
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

        // Grown: a mature space below its target is installed onto it. The target is
        // `MATURE_TARGET_LIVE_MULTIPLE * need_after` through the growth table, so
        // `mature_after_fullsweep(12, 13, 0)` asks for `2 * (13 + 64) = 154` -> 233, and an
        // empty live set asks for `2 * 64 = 128` -> 142.
        assert_eq!(policy.mature_after_fullsweep(12, 13, 0), 233);
        assert_eq!(policy.mature_after_fullsweep(0, 0, 0), 142);
        assert_eq!(
            policy.mature_after_fullsweep(12, 13, 0),
            policy.mature_target_words(13, 0),
            "the grown result *is* the target"
        );

        // Kept: 610 words with a 100-word live set has a 376-word target — inside
        // `[target, 4 * target)`, so neither bracket fires.
        assert_eq!(policy.mature_target_words(100, 0), 376);
        assert_eq!(policy.mature_after_fullsweep(610, 100, 0), 610);
        // 233 words, on the other hand, is *below* the 376-word target and is grown onto it.
        assert_eq!(policy.mature_after_fullsweep(233, 100, 0), 376);

        // Shrunk: 1000 words keeping 0 has a 142-word target, more than four times below the
        // capacity, so `should_shrink` fires and the capacity becomes the target — never more
        // than the old capacity.
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
        // 8 192 words keeping 100 has a 376-word target floored to the 4 096-word minimum — but
        // 8 192 is only twice that floor, inside the four-times shrink bracket, so it is kept.
        assert_eq!(floored.mature_after_fullsweep(8_192, 100, 0), 8_192);
        // Four times the floored target is where the shrink fires: 32 768 > 4 * 4 096.
        assert_eq!(floored.mature_after_fullsweep(32_768, 100, 0), 4_096);
        // A capacity already at the floor is never "shrunk" further.
        assert_eq!(floored.mature_after_fullsweep(4_096, 100, 0), 4_096);

        // A maximum clamps a would-be grow: 12 words needing 464 would go to 987
        // (`2 * 464`), capped to 512.
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
            default.next_words(
                MATURE_TARGET_LIVE_MULTIPLE * (100 + 100 + default.need_reserve_words),
                0
            )
        );
        // The clamped policy's fixed point is the clamped target: a second fullsweep at the same
        // live set keeps what the first one installed.
        assert_eq!(capped.mature_after_fullsweep(512, 400, 0), 512);
        assert_eq!(floored.mature_after_fullsweep(4_096, 13, 0), 4_096);
    }

    /// The **minor** rule: the capacity is compared with the **young live set's** target
    /// (`young_minor_target_words`, one times the promoted words) and is installed from either
    /// side of it — grown onto it when the nursery is smaller, shrunk back to it once the live
    /// set has fallen `YOUNG_SHRINK_FACTOR` times below it **and** the nursery is above
    /// [`MINOR_SHRINK_FLOOR_WORDS`], kept in between. Every result is the capacity or the target,
    /// never a third value, so the rule has one fixed point per live set; and no result is below
    /// the nursery floor.
    #[test]
    fn young_after_collection_tracks_the_minor_target_from_both_sides() {
        let policy = HeapPolicy::default();
        let floor = policy.nursery_floor_words();

        for &capacity in &[12usize, 90, 233, 376, 1_000, 4_185] {
            for live in [0usize, 13, 60, 233, 1_000, 100_000] {
                let target = policy.young_minor_target_words(live);
                let after = policy.young_after_collection(capacity, live);
                // A kept capacity *is* the capacity and an installed one is the live set's
                // target — never a third value.
                assert!(
                    after == capacity || after == target,
                    "{capacity}/{live}: {after} is neither the capacity nor the minor target \
                     {target}"
                );
                // The two structural bounds: a collection never leaves the nursery below the
                // floor, and never above the shrink band around the target — the band's upper
                // edge being the larger of `YOUNG_SHRINK_FACTOR` x the target and the minor-shrink
                // floor.
                assert!(
                    after >= floor,
                    "{capacity}/{live}: {after} is below the nursery floor {floor}"
                );
                assert!(
                    after <= MINOR_SHRINK_FLOOR_WORDS.max(YOUNG_SHRINK_FACTOR * target),
                    "{capacity}/{live}: {after} exceeds the band around {target}"
                );
                assert!(
                    after >= target,
                    "{capacity}/{live}: {after} is below the minor target {target}"
                );
                // One application is a fixed point: the installed capacity is the target (which
                // the rule reproduces) or a kept one inside the band (which it keeps again).
                assert_eq!(
                    policy.young_after_collection(after, live),
                    after,
                    "{capacity}/{live}: {after} is not a fixed point"
                );
            }
        }

        // A capacity **inside** the band is untouched: 233 is inside
        // `[142, max(3 000, 8 * 142)]` for the 60-word live set, and at the target for the empty
        // one (90, where it is kept verbatim).
        assert_eq!(policy.young_minor_target_words(60), 142);
        assert_eq!(policy.young_after_collection(233, 60), 233);
        assert_eq!(policy.young_minor_target_words(0), 90);
        assert_eq!(policy.young_after_collection(233, 0), 233);
        assert_eq!(policy.young_after_collection(90, 0), 90);

        // A capacity **below** the target installs the target: a minor really does grow the
        // nursery, and only onto a live-set-derived capacity. 233 is below `next_words(297)`.
        assert_eq!(policy.young_minor_target_words(233), 376);
        assert_eq!(policy.young_after_collection(233, 233), 376);
        assert_eq!(policy.young_minor_target_words(175), 376);
        assert_eq!(policy.young_after_collection(233, 175), 376);
        assert_eq!(policy.young_after_collection(12, 0), 90);
        assert_eq!(policy.young_after_collection(51, 13), 90);
        assert_eq!(policy.young_after_collection(90, 13), 90);

        // The gate is strict and its upper edge is `max(MINOR_SHRINK_FLOOR_WORDS,
        // YOUNG_SHRINK_FACTOR x target)` = `max(3 000, 720)` = 3 000 for the 13-word live set: a
        // nursery exactly at the boundary is still kept, one word above it is handed back.
        assert_eq!(policy.young_minor_target_words(13), 90);
        assert!(
            YOUNG_SHRINK_FACTOR * 90 < MINOR_SHRINK_FLOOR_WORDS,
            "the floor is the band's upper edge for a small live set"
        );
        assert_eq!(
            policy.young_after_collection(MINOR_SHRINK_FLOOR_WORDS, 13),
            MINOR_SHRINK_FLOOR_WORDS
        );
        assert_eq!(
            policy.young_after_collection(MINOR_SHRINK_FLOOR_WORDS + 1, 13),
            90
        );
        // A capacity in `(YOUNG_SHRINK_FACTOR x target, MINOR_SHRINK_FLOOR_WORDS]` is **kept** —
        // the band the floor adds over the bare 8x rule.
        assert_eq!(
            policy.young_after_collection(YOUNG_SHRINK_FACTOR * 90, 13),
            YOUNG_SHRINK_FACTOR * 90
        );
        assert_eq!(
            policy.young_after_collection(YOUNG_SHRINK_FACTOR * 90 + 1, 13),
            YOUNG_SHRINK_FACTOR * 90 + 1
        );
        assert_eq!(policy.young_after_collection(1_000, 13), 1_000);
        assert_eq!(policy.young_after_collection(1_000, 0), 1_000);
        // A grossly over-sized nursery still shrinks back to the live set's target.
        assert_eq!(policy.young_after_collection(4_185, 13), 90);
        assert_eq!(policy.young_after_collection(4_185, 0), 90);
        assert_eq!(policy.young_after_collection(100_000, 60), 142);
        // The rule is monotone in the live set for a fixed capacity: as the target rises through
        // the keep band the capacity is kept, and once the target passes it the target is
        // installed — never a smaller result than for a smaller live set.
        let mut previous = 0usize;
        for live in [0usize, 1, 13, 60, 175, 233, 464, 1_000, 10_000] {
            let after = policy.young_after_collection(233, live);
            assert!(
                after >= previous,
                "young_after_collection(233, {live}) = {after} < {previous}"
            );
            previous = after;
        }

        // The result is always bounded by the policy.
        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        assert_eq!(
            floored.nursery_floor_words(),
            500,
            "the minimum floors the empty live set's target"
        );
        assert_eq!(
            floored.young_after_collection(100_000, 0),
            500,
            "the minimum floors the shrunk nursery"
        );
        assert_eq!(
            floored.young_after_collection(233, 0),
            500,
            "a capacity below the floored target is grown onto it"
        );
        let capped = HeapPolicy {
            max_heap_words: 300,
            ..HeapPolicy::default()
        };
        // The maximum clamps both targets, but never forces the shrink of a nursery that is
        // already inside its band.
        assert_eq!(capped.young_target_words(1_000), 300);
        assert_eq!(capped.young_minor_target_words(1_000), 300);
        assert_eq!(capped.young_after_collection(233, 1_000), 300);
        assert_eq!(capped.young_after_collection(40_000, 1_000), 300);
        assert_eq!(capped.young_after_collection(233, 0), 233);
    }

    /// The **minor-shrink floor** — the legacy C's `HEAP_SIZE(p) > 3000` conjunct of the minor
    /// resizer's shrink gate (`erts/emulator/beam/erl_gc.c:1517-1529`).
    ///
    /// A minor never hands a young heap at or below [`MINOR_SHRINK_FLOOR_WORDS`] words back,
    /// whatever the round's live set — after tenuring a small heap always looks nearly empty, so
    /// an ungated rule would give its capacity back and make the growth seam climb straight back.
    /// The floor gates the **shrink** branch only: below its target a nursery still grows, even
    /// from far below the floor.
    #[test]
    fn the_minor_shrink_floor_is_the_c_heapsize_gate() {
        let policy = HeapPolicy::default();
        let spawn_policy = HeapPolicy::from_spawn_opts(0, 0, 65_535);

        // --- (f) Where the floor sits on the crate's own scale -------------------------------
        //
        // It is the C's literal 3 000 words; it is above every capacity either policy's own
        // nursery floor can produce (so the default 233-word spawn nursery is unaffected — it
        // is kept by the plain band long before the floor could matter); and it falls between
        // the growth table's 2 586- and 4 185-word entries, so 4 185 is the first curated
        // capacity that can shrink at all against a small live set.
        assert_eq!(MINOR_SHRINK_FLOOR_WORDS, 3_000);
        assert!(MINOR_SHRINK_FLOOR_WORDS > policy.nursery_floor_words());
        assert!(MINOR_SHRINK_FLOOR_WORDS > spawn_policy.nursery_floor_words());
        assert!(heap_sizes().contains(&2_586));
        assert!(heap_sizes().contains(&4_185));
        assert!(MINOR_SHRINK_FLOOR_WORDS > 2_586);
        assert!(MINOR_SHRINK_FLOOR_WORDS < 4_185);
        // The default (233-word `min_heap_size`) nursery's whole keep band is untouched: its
        // capacity is kept at every live set it can measure here, exactly as before the floor.
        assert_eq!(policy.young_minor_target_words(0), 90);
        assert_eq!(policy.young_after_collection(233, 0), 233);
        assert_eq!(policy.young_after_collection(233, 13), 233);

        // --- (a) Above the floor and above the factor band: the capacity is given back -------
        assert_eq!(policy.young_minor_target_words(13), 90);
        assert!(4_185 > MINOR_SHRINK_FLOOR_WORDS);
        assert!(4_185 > YOUNG_SHRINK_FACTOR * 90);
        assert_eq!(policy.young_after_collection(4_185, 13), 90);

        // --- (b) Exactly at the floor: kept (the C's gate is the strict `>`) -----------------
        assert_eq!(
            policy.young_after_collection(MINOR_SHRINK_FLOOR_WORDS, 13),
            MINOR_SHRINK_FLOOR_WORDS
        );
        assert_eq!(
            policy.young_after_collection(MINOR_SHRINK_FLOOR_WORDS, 0),
            MINOR_SHRINK_FLOOR_WORDS
        );

        // --- (c) Inside `(YOUNG_SHRINK_FACTOR x target, MINOR_SHRINK_FLOOR_WORDS]`: kept ------
        //
        // This is the band the floor adds. Every capacity here is past the bare 8x rule
        // (`8 x 90 = 720` for the empty live set) and would have shrunk before the floor
        // existed; 721 is the first of them.
        for capacity in [721usize, 987, 1_000, 1_598, 2_586, 2_999, 3_000] {
            for live in [0usize, 13] {
                assert!(
                    capacity > YOUNG_SHRINK_FACTOR * policy.young_minor_target_words(live),
                    "{capacity} must be past the bare factor band for live {live}"
                );
                assert_eq!(
                    policy.young_after_collection(capacity, live),
                    capacity,
                    "{capacity} is at or below the {MINOR_SHRINK_FLOOR_WORDS}-word floor and \
                     must be kept (live {live})"
                );
            }
        }

        // --- (d) The floor never blocks a growth --------------------------------------------
        //
        // A nursery below the live set's target grows onto it even when it is far below the
        // floor: `target > capacity` wins.
        assert_eq!(policy.young_minor_target_words(400), 610);
        assert_eq!(policy.young_minor_target_words(1_000), 1_598);
        assert!(233 < MINOR_SHRINK_FLOOR_WORDS);
        assert!(500 < MINOR_SHRINK_FLOOR_WORDS);
        assert_eq!(policy.young_after_collection(233, 400), 610);
        assert_eq!(policy.young_after_collection(500, 1_000), 1_598);
        assert_eq!(policy.young_after_collection(12, 400), 610);

        // --- (e) Still a fixed point, never a third value ------------------------------------
        for capacity in [4_185usize, MINOR_SHRINK_FLOOR_WORDS + 1, 3_000, 721, 233, 12] {
            for live in [0usize, 13, 400, 1_000, 100_000] {
                let target = policy.young_minor_target_words(live);
                let after = policy.young_after_collection(capacity, live);
                assert!(
                    after == capacity || after == target,
                    "{capacity}/{live}: {after} is neither the capacity nor the target {target}"
                );
                assert_eq!(
                    policy.young_after_collection(after, live),
                    after,
                    "{capacity}/{live}: {after} is not a fixed point"
                );
            }
        }

        // A capped policy keeps the floor's shape: the clamp moves both edges down, and the
        // result is still the capacity or the (clamped) target.
        let capped = HeapPolicy {
            max_heap_words: 400,
            ..HeapPolicy::default()
        };
        assert_eq!(capped.young_minor_target_words(0), 90);
        assert_eq!(capped.young_after_collection(400, 0), 400);
        assert_eq!(capped.young_after_collection(4_185, 0), 90);
    }
    /// The **minor** target is ONE times the young live set plus the reserve — the smallest
    /// curated capacity at which the round's own survival does not itself force the next
    /// collection — while the fullsweep keeps its `YOUNG_TARGET_LIVE_MULTIPLE` x rule for the
    /// same live set. Both are monotone in the live set and never fall below the reserve floor.
    #[test]
    fn the_minor_target_is_one_times_the_young_live_set() {
        let policy = HeapPolicy::default();

        assert_eq!(policy.young_minor_target_words(401), 610);
        assert_eq!(next_heap_size(401 + policy.need_reserve_words, 0), 610);
        // The fullsweep's 2x rule is untouched: `next_words(2 * 401 + 64) = next_words(866)`.
        assert_eq!(policy.young_target_words(401), 987);
        assert!(policy.young_target_words(401) > policy.young_minor_target_words(401));

        // The empty live set's target IS the floor, and the floor is the reserve's own curated
        // size — so a nursery sized by this rule can never be smaller than the reserve needs.
        assert_eq!(policy.young_minor_target_words(0), 90);
        assert_eq!(
            policy.young_minor_target_words(0),
            policy.nursery_floor_words()
        );
        assert_eq!(policy.nursery_floor_words(), next_heap_size(64, 0));
        assert!(policy.nursery_floor_words() >= policy.need_reserve_words);
        assert!(policy.young_target_words(0) >= policy.nursery_floor_words());
        assert_eq!(
            policy.young_target_words(0),
            policy.nursery_floor_words(),
            "both targets reduce to the floor at an empty live set"
        );

        let mut previous = 0usize;
        for live in [0usize, 1, 12, 13, 60, 233, 401, 464, 1_000, 10_000, 100_000] {
            let target = policy.young_minor_target_words(live);
            assert!(
                target >= previous,
                "the minor target must be monotone in the live set: {live} -> {target}"
            );
            assert!(target >= policy.nursery_floor_words());
            assert!(
                target > live,
                "the target must hold the live set plus the reserve: {live} -> {target}"
            );
            previous = target;
        }
    }

    /// The **nursery floor** is a real lower bound on every capacity the rule installs: `90` for
    /// the default policy (no `min_heap_words`), exactly `DEFAULT_MIN_HEAP_WORDS` (`233`) for a
    /// spawn policy — where it is also `young_minor_target_words(0)` — and clamped down only by a
    /// caller's own `max_heap_words`. The fresh-process seed [`HeapPolicy::initial_heap_words`]
    /// stays exactly `233`, which is what the oracle reports as a new process's `heap_size`.
    #[test]
    fn the_nursery_floor_is_the_empty_live_set_target() {
        let policy = HeapPolicy::default();
        assert_eq!(policy.min_heap_words, 0);
        assert_eq!(policy.nursery_floor_words(), 90);
        assert!(policy.nursery_floor_words() >= policy.need_reserve_words);

        // A spawn policy floors at the C `H_MIN_SIZE`, so the floor and the empty target are
        // exactly the default minimum heap size.
        let spawn = HeapPolicy::from_spawn_opts(0, 0, 65_535);
        assert_eq!(spawn.min_heap_words, DEFAULT_MIN_HEAP_WORDS);
        assert_eq!(spawn.nursery_floor_words(), DEFAULT_MIN_HEAP_WORDS);
        assert_eq!(spawn.nursery_floor_words(), 233);
        assert_eq!(spawn.young_minor_target_words(0), 233);
        assert_eq!(spawn.young_target_words(0), 233);
        assert_eq!(
            HeapPolicy::default()
                .spawn_policy(0, 0, None)
                .nursery_floor_words(),
            233
        );
        // ... and a caller-set `+hms` floors it at that minimum instead.
        assert_eq!(
            HeapPolicy::from_min_max(4_185, 0).nursery_floor_words(),
            4_185
        );

        // The fresh-process seed is a different quantity and is unchanged.
        assert_eq!(HeapPolicy::default().initial_heap_words(), 233);
        assert_eq!(spawn.initial_heap_words(), 233);
        assert_eq!(HeapPolicy::from_min_max(142, 0).initial_heap_words(), 142);

        // Only the caller's own ceiling can put the floor below the reserve.
        let tiny_cap = HeapPolicy {
            max_heap_words: 51,
            ..HeapPolicy::default()
        };
        assert_eq!(tiny_cap.nursery_floor_words(), 51);
        assert!(tiny_cap.nursery_floor_words() < tiny_cap.need_reserve_words);
    }
    /// The **minimum/reserve-capacity guarantee** over the whole growth table and over
    /// min/max-clamped policies: both targets stay at or above `nursery_floor_words()`, and so
    /// does every capacity either collapse seam installs.
    ///
    /// This is a correctness property rather than a number pin — it is the invariant a dependent
    /// re-attempting a refused allocation asserts against (`beam_process`'s collect-and-retry) —
    /// so it is checked at every growth-table entry and both of its neighbours, under the
    /// unconfigured default, a spawn policy, a `+hms` floor, a ceiling that does not bind the
    /// floor, and a ceiling *below* the reserve (the one shape allowed to answer a floor under
    /// `need_reserve_words`, the caller's own explicit choice).
    #[test]
    fn the_nursery_floor_bounds_every_target_and_every_collection() {
        let policies = [
            HeapPolicy::default(),
            HeapPolicy::from_spawn_opts(0, 0, 65_535),
            HeapPolicy::from_min_max(4_185, 0),
            HeapPolicy {
                min_heap_words: 500,
                ..HeapPolicy::default()
            },
            HeapPolicy {
                max_heap_words: 300,
                ..HeapPolicy::default()
            },
            HeapPolicy {
                max_heap_words: 51,
                ..HeapPolicy::default()
            },
        ];

        // Every growth-table entry and both of its neighbours: a target derived from a live set
        // one word off an entry is the case a hand-picked sample list cannot cover.
        let mut lives: Vec<usize> = vec![0, 1];
        let mut capacities: Vec<usize> = vec![0];
        for &size in heap_sizes() {
            lives.push(size.saturating_sub(1));
            lives.push(size);
            lives.push(size.saturating_add(1));
            capacities.push(size);
        }
        lives.sort_unstable();
        lives.dedup();

        for policy in &policies {
            let floor = policy.nursery_floor_words();
            // The floor is the reserve's own curated size, so an unbounded policy is never below
            // the reserve it exists to cover; only a caller's `+hmax` can push it underneath.
            if policy.max_heap_words == 0 {
                assert!(
                    floor >= policy.need_reserve_words,
                    "{policy:?}: the floor {floor} is below the reserve {}",
                    policy.need_reserve_words
                );
            }
            // Both targets reduce to the floor at an empty live set — it *is* the degenerate
            // case of each rule, not a separate quantity.
            assert_eq!(policy.young_minor_target_words(0), floor, "{policy:?}");
            assert_eq!(policy.young_target_words(0), floor, "{policy:?}");

            let (mut previous_minor, mut previous_young) = (0usize, 0usize);
            for &live in &lives {
                let minor = policy.young_minor_target_words(live);
                let young = policy.young_target_words(live);
                // The guarantee itself.
                assert!(
                    minor >= floor,
                    "{policy:?}: young_minor_target_words({live}) = {minor} < floor {floor}"
                );
                assert!(
                    young >= floor,
                    "{policy:?}: young_target_words({live}) = {young} < floor {floor}"
                );
                // Both are monotone in the live set and the fullsweep's 2x rule never lands
                // below the minor's 1x rule for the same live set.
                assert!(
                    minor >= previous_minor && young >= previous_young,
                    "{policy:?}: the targets are not monotone at live {live}"
                );
                assert!(
                    young >= minor,
                    "{policy:?}: young_target_words({live}) = {young} < {minor}"
                );
                (previous_minor, previous_young) = (minor, young);

                // And no collection leaves the nursery below the floor, from any capacity on the
                // table: the grow branch installs the target, which is itself at or above it.
                for &capacity in &capacities {
                    assert!(
                        policy.young_after_collection(capacity, live) >= floor,
                        "{policy:?}: a minor at capacity {capacity} / live {live} left the \
                         nursery below the floor {floor}"
                    );
                    assert!(
                        policy.young_after_fullsweep(capacity, live) >= floor,
                        "{policy:?}: a fullsweep at capacity {capacity} / live {live} left the \
                         nursery below the floor {floor}"
                    );
                }
            }
        }
    }

    /// The nursery follows the **live set**: a fullsweep sizes it to    /// `young_target_words(live)`, keeps it inside the band, and is a fixed point — the
    /// property a fill- or capacity-derived rule cannot have.
    #[test]
    fn young_after_fullsweep_tracks_the_live_set_and_is_a_fixed_point() {
        let policy = HeapPolicy::default();

        // Grown: a nursery below the live set's target no longer amortizes the copy, so a
        // nursery of 609 words against a 464-word live set is stepped up to the target
        // (next_words(2 * 464 + 64) = next_words(992) = 1 598).
        assert_eq!(policy.young_target_words(464), 1_598);
        assert_eq!(policy.young_after_fullsweep(609, 464), 1_598);
        // The `should_grow` slack is the C's `3 * HEAP_SIZE < 4 * need_after`
        // (`erl_gc.c:2073-2077`): 376 is below 4/3 of the 610 target, so it grows ...
        assert_eq!(policy.young_target_words(233), 610);
        assert_eq!(policy.young_after_fullsweep(376, 233), 610);
        // ... while a nursery at 4/3 of the target or above (and inside the shrink band) is
        // kept verbatim: 4 000 words for a 1 000-word live set.
        assert_eq!(policy.young_target_words(1_000), 2_586);
        assert_eq!(policy.young_after_fullsweep(4_000, 1_000), 4_000);
        // A nursery the target lands exactly on is reproduced, not stepped up.
        assert_eq!(policy.young_after_fullsweep(1_598, 464), 1_598);
        assert_eq!(policy.young_after_fullsweep(2_586, 1_000), 2_586);
        // Shrunk: the burst the capacity was stepped up for is long dead.
        assert_eq!(policy.young_after_fullsweep(2_819, 60), 233);
        assert_eq!(policy.young_after_fullsweep(100_000, 0), 90);
        // The shrink gate too is strict and unrelated to the capacity (it is
        // `YOUNG_SHRINK_FACTOR` x the target).
        assert_eq!(
            policy.young_after_fullsweep(YOUNG_SHRINK_FACTOR * 233, 60),
            YOUNG_SHRINK_FACTOR * 233
        );
        assert_eq!(
            policy.young_after_fullsweep(YOUNG_SHRINK_FACTOR * 233 + 1, 60),
            233
        );

        // The whole contract over the growth table: the result is always the capacity or the
        // target, never above `YOUNG_SHRINK_FACTOR` x the target, never below the live set
        // (the policy has no ceiling here), and it is a fixed point of the next fullsweep —
        // i.e. the capacity converges instead of ratcheting one growth-table step per
        // collection.
        for &capacity in &[12usize, 38, 90, 142, 233, 376, 610, 987, 1_598, 2_586, 100_000] {
            for live in [0usize, 1, 60, 233, 464, 1_000, 10_000] {
                let target = policy.young_target_words(live);
                let once = policy.young_after_fullsweep(capacity, live);
                assert!(
                    once == capacity || once == target,
                    "{capacity}/{live}: {once} is neither the capacity nor the target {target}"
                );
                assert!(
                    once >= live,
                    "{capacity}/{live}: {once} does not cover the live set"
                );
                assert!(
                    once <= YOUNG_SHRINK_FACTOR * target,
                    "{capacity}/{live}: {once} exceeds the band around {target}"
                );
                assert_eq!(
                    policy.young_after_fullsweep(once, live),
                    once,
                    "{capacity}/{live}: {once} is not a fixed point"
                );
            }
        }

        // `+hms` (`min_heap_words`) floors the target ...
        let floored = HeapPolicy {
            min_heap_words: 500,
            ..HeapPolicy::default()
        };
        assert_eq!(floored.young_after_fullsweep(100_000, 10), 500);
        // ... and `+hmax` (`max_heap_words`) clamps it down, even below a live set the policy's
        // own ceiling cannot hold (the caller's explicit choice, never a fabricated larger
        // target). A target the ceiling does not bind is left alone.
        let capped = HeapPolicy {
            max_heap_words: 300,
            ..HeapPolicy::default()
        };
        assert_eq!(capped.young_after_fullsweep(100_000, 10), 90);
        assert_eq!(capped.young_after_fullsweep(233, 5_000), 300);
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

    // -----------------------------------------------------------------------------------------
    // The `need` term (the C's `need_after = occupancy + need + stack_size [+ S_RESERVED]`)
    // -----------------------------------------------------------------------------------------

    /// The policies the `need`-term tests sweep: the unconfigured default, a spawn policy (whose
    /// floor is `DEFAULT_MIN_HEAP_WORDS`), a `+hms` policy (whose floor is its own minimum) and a
    /// capped policy (whose ceiling clamps every target).
    fn need_term_policies() -> [HeapPolicy; 4] {
        [
            HeapPolicy::default(),
            HeapPolicy::from_spawn_opts(0, 0, 65_535),
            HeapPolicy::from_min_max(4_185, 0),
            HeapPolicy {
                max_heap_words: 1_000,
                ..HeapPolicy::default()
            },
        ]
    }

    /// **The blast-radius control.** At `need_words == 0` every `_with_need` method is *exactly*
    /// its existing counterpart, so the pinned figures (`233`, `610`, `843`, `987`), the floor
    /// band and every dependent's capacity pin are untouched: the new term is strictly additive.
    #[test]
    fn the_need_term_is_identically_zero_at_zero_need() {
        let capacities = [
            0usize, 12, 90, 142, 233, 376, 610, 720, 721, 842, 843, 987, 1_000, 1_598, 2_586,
            2_999, 3_000, 3_001, 4_185, 7_896, 7_897, 12_784, 12_785, 100_000,
        ];
        let lives = [
            0usize, 1, 12, 13, 60, 201, 233, 401, 464, 1_000, 10_000, 100_000,
        ];

        for policy in need_term_policies() {
            for live in lives {
                assert_eq!(
                    policy.young_minor_target_words_with_need(live, 0),
                    policy.young_minor_target_words(live),
                    "{policy:?}: the minor target changed at live {live}"
                );
                assert_eq!(
                    policy.young_target_words_with_need(live, 0),
                    policy.young_target_words(live),
                    "{policy:?}: the fullsweep target changed at live {live}"
                );
                for capacity in capacities {
                    assert_eq!(
                        policy.young_after_collection_with_need(capacity, live, 0),
                        policy.young_after_collection(capacity, live),
                        "{policy:?}: a minor at capacity {capacity} / live {live} changed"
                    );
                    assert_eq!(
                        policy.young_after_fullsweep_with_need(capacity, live, 0),
                        policy.young_after_fullsweep(capacity, live),
                        "{policy:?}: a fullsweep at capacity {capacity} / live {live} changed"
                    );
                }
            }
        }

        // The documented figures themselves, restated through the `_with_need` entry point.
        let policy = HeapPolicy::default();
        assert_eq!(policy.young_minor_target_words_with_need(401, 0), 610);
        assert_eq!(policy.young_target_words_with_need(401, 0), 987);
        assert_eq!(policy.young_minor_target_words_with_need(0, 0), 90);
        assert_eq!(policy.young_target_words_with_need(0, 0), 90);
        assert_eq!(
            HeapPolicy::from_spawn_opts(0, 0, 65_535).young_minor_target_words_with_need(0, 0),
            233
        );
    }

    /// The **motivating case**: a collection triggered by a 321-word copy-out installs the
    /// capacity the copy-out actually needs, instead of one the next copy-out must immediately
    /// grow again. The augmented argument is `live + need + need_reserve_words`, so a 401-word
    /// live set plus a 321-word request lands on `next_words(786) = 987` — the very capacity the
    /// un-augmented fullsweep target already needed for the live set alone.
    #[test]
    fn the_need_term_is_the_pending_allocation_request() {
        let policy = HeapPolicy::default();
        assert_eq!(policy.need_reserve_words, 64);

        assert_eq!(policy.young_minor_target_words(401), 610);
        assert_eq!(policy.young_minor_target_words_with_need(401, 321), 987);
        assert_eq!(next_heap_size(401 + 321 + 64, 0), 987);
        // The un-augmented 610-word nursery would have to grow again on exactly that request.
        assert!(610 < 401 + 321);

        // The fullsweep's 2x rule takes the same term inside the multiple.
        assert_eq!(policy.young_target_words(401), 987);
        assert_eq!(policy.young_target_words_with_need(401, 321), 1_598);
        assert_eq!(next_heap_size(2 * (401 + 321) + 64, 0), 1_598);

        // A spawn policy floors the result at its own minimum, need or no need.
        let spawn = HeapPolicy::from_spawn_opts(0, 0, 65_535);
        assert_eq!(spawn.young_minor_target_words_with_need(0, 0), 233);
        assert_eq!(spawn.young_minor_target_words_with_need(0, 64), 233);
        assert!(spawn.young_minor_target_words_with_need(0, 1_000) > 233);
    }

    /// **Monotone in `need`**: a positive request never lowers a target or an installed capacity —
    /// `next_words` is monotone and the argument only grows, and the three-way rule can only move
    /// *towards* a larger target.
    #[test]
    fn a_positive_need_never_lowers_a_target_or_a_capacity() {
        let capacities = [0usize, 233, 610, 987, 1_598, 3_000, 7_896, 12_784, 100_000];
        let lives = [0usize, 13, 401, 1_000, 10_000];
        let needs = [1usize, 12, 64, 321, 1_000, 20_000];

        for policy in need_term_policies() {
            for live in lives {
                for need in needs {
                    assert!(
                        policy.young_minor_target_words_with_need(live, need)
                            >= policy.young_minor_target_words_with_need(live, 0),
                        "{policy:?}: the minor target fell at live {live} / need {need}"
                    );
                    assert!(
                        policy.young_target_words_with_need(live, need)
                            >= policy.young_target_words_with_need(live, 0),
                        "{policy:?}: the fullsweep target fell at live {live} / need {need}"
                    );
                    for capacity in capacities {
                        assert!(
                            policy.young_after_collection_with_need(capacity, live, need)
                                >= policy.young_after_collection_with_need(capacity, live, 0),
                            "{policy:?}: a minor at capacity {capacity} / live {live} shrank \
                             when the need term grew to {need}"
                        );

                        // The fullsweep rule is monotone in the need term too, **except** for
                        // one pre-existing (`need`-independent) corner: its grow branch is
                        // `should_grow`, i.e. `3 * capacity < 4 * target`, which fires for a
                        // capacity just *above* the target and installs the target — so a target
                        // rising from below a kept capacity into that headroom window lowers the
                        // installed capacity. That is the only way the term can lower a figure,
                        // and the branch itself is untouched by this change.
                        let swept = policy.young_after_fullsweep_with_need(capacity, live, need);
                        let swept_zero = policy.young_after_fullsweep_with_need(capacity, live, 0);
                        if swept < swept_zero {
                            let target = policy.young_target_words_with_need(live, need);
                            assert_eq!(
                                swept, target,
                                "{policy:?}: a fullsweep at capacity {capacity} / live {live} \
                                 / need {need} answered neither the capacity nor the target"
                            );
                            assert!(
                                capacity > target && 3 * capacity < 4 * target,
                                "{policy:?}: the fullsweep shrank at capacity {capacity} / \
                                 live {live} / need {need} outside the should_grow headroom \
                                 window (target {target})"
                            );
                        }
                    }
                }
            }
        }

        // The targets are monotone in the need term itself, not only against the `need == 0` row.
        let policy = HeapPolicy::default();
        let (mut previous_minor, mut previous_young) = (0usize, 0usize);
        for need in [0usize, 1, 64, 128, 321, 1_000, 20_000] {
            let minor = policy.young_minor_target_words_with_need(401, need);
            let young = policy.young_target_words_with_need(401, need);
            assert!(minor >= previous_minor, "minor target fell at need {need}");
            assert!(
                young >= previous_young,
                "fullsweep target fell at need {need}"
            );
            assert!(young >= minor);
            (previous_minor, previous_young) = (minor, young);
        }
    }

    /// **Still a fixed point**: the result is still exactly the need-aware `target` or exactly the
    /// capacity, never a third value, so `ensure_capacity(n)` repeated at a constant live set and a
    /// constant request reproduces the capacity it installed instead of ratcheting.
    #[test]
    fn the_need_aware_rules_are_still_fixed_points() {
        let capacities = [
            0usize, 12, 233, 610, 842, 843, 987, 1_598, 3_000, 7_896, 100_000,
        ];
        let lives = [0usize, 13, 401, 1_000, 100_000];
        let needs = [0usize, 321, 1_000];

        for policy in need_term_policies() {
            for live in lives {
                for need in needs {
                    let minor_target = policy.young_minor_target_words_with_need(live, need);
                    let young_target = policy.young_target_words_with_need(live, need);
                    for capacity in capacities {
                        let minor = policy.young_after_collection_with_need(capacity, live, need);
                        assert!(
                            minor == capacity || minor == minor_target,
                            "{policy:?}: minor {capacity}/{live}/{need} -> {minor} is neither \
                             the capacity nor the target {minor_target}"
                        );
                        assert_eq!(
                            policy.young_after_collection_with_need(minor, live, need),
                            minor,
                            "{policy:?}: minor {capacity}/{live}/{need} -> {minor} is not a \
                             fixed point"
                        );

                        let young = policy.young_after_fullsweep_with_need(capacity, live, need);
                        assert!(
                            young == capacity || young == young_target,
                            "{policy:?}: fullsweep {capacity}/{live}/{need} -> {young} is \
                             neither the capacity nor the target {young_target}"
                        );
                        assert_eq!(
                            policy.young_after_fullsweep_with_need(young, live, need),
                            young,
                            "{policy:?}: fullsweep {capacity}/{live}/{need} -> {young} is not a \
                             fixed point"
                        );
                    }
                }
            }
        }
    }

    /// **The floor keeps winning where it already won.** `MINOR_SHRINK_FLOOR_WORDS` is still a
    /// constant gate on the *capacity*, and the keep band's upper edge is still
    /// `max(MINOR_SHRINK_FLOOR_WORDS, YOUNG_SHRINK_FACTOR × target)` with the **need-aware**
    /// target; the fullsweep keeps its floor-free `[target, YOUNG_SHRINK_FACTOR × target]` shape.
    #[test]
    fn the_minor_floor_and_band_survive_the_need_term() {
        let policy = HeapPolicy::default();

        // `live = 401, need = 321` -> target 987, band `max(3000, 8 * 987) = 7896`.
        let target = policy.young_minor_target_words_with_need(401, 321);
        assert_eq!(target, 987);
        let band = MINOR_SHRINK_FLOOR_WORDS.max(YOUNG_SHRINK_FACTOR * target);
        assert_eq!(band, 7_896);
        // The band's edges: at the edge the capacity is kept, one word past it installs the
        // target, one word below the target grows onto it, and the target is its own fixed point.
        assert_eq!(
            policy.young_after_collection_with_need(band, 401, 321),
            band
        );
        assert_eq!(
            policy.young_after_collection_with_need(band + 1, 401, 321),
            target
        );
        assert_eq!(
            policy.young_after_collection_with_need(target - 1, 401, 321),
            target
        );
        assert_eq!(
            policy.young_after_collection_with_need(target, 401, 321),
            target
        );

        // A capacity at or below the 3 000-word floor is still **kept** — even when the band's
        // factor edge would have handed it back: `8 * 233 = 1864 < 3000` and
        // `8 * 90 = 720 < 3000`, and the huge-need rows keep it too because `target <= 3000`.
        assert!(8 * policy.young_minor_target_words_with_need(0, 100) < MINOR_SHRINK_FLOOR_WORDS);
        assert_eq!(
            policy.young_after_collection_with_need(MINOR_SHRINK_FLOOR_WORDS, 0, 100),
            MINOR_SHRINK_FLOOR_WORDS
        );
        assert_eq!(
            policy.young_after_collection_with_need(MINOR_SHRINK_FLOOR_WORDS, 0, 0),
            MINOR_SHRINK_FLOOR_WORDS
        );
        assert_eq!(
            policy.young_after_collection_with_need(MINOR_SHRINK_FLOOR_WORDS, 401, 321),
            MINOR_SHRINK_FLOOR_WORDS
        );
        // ... while the floor still never blocks a **growth**: a target above the capacity wins,
        // even a capacity far below the floor.
        assert_eq!(
            policy.young_after_collection_with_need(233, 401, 321),
            target
        );
        // A big-enough need does push the target past the floor, but that is a *growth* onto the
        // need-aware target — the floor gates the shrink side only.
        assert!(policy.young_minor_target_words_with_need(0, 4_000) > MINOR_SHRINK_FLOOR_WORDS);
        assert_eq!(
            policy.young_after_collection_with_need(MINOR_SHRINK_FLOOR_WORDS, 0, 4_000),
            policy.young_minor_target_words_with_need(0, 4_000)
        );

        // The fullsweep's shape is unchanged: no 3 000-word floor, its band is the bare factor.
        let young_target = policy.young_target_words_with_need(401, 321);
        assert_eq!(young_target, 1_598);
        let young_band = YOUNG_SHRINK_FACTOR * young_target;
        assert_eq!(
            policy.young_after_fullsweep_with_need(young_band, 401, 321),
            young_band
        );
        assert_eq!(
            policy.young_after_fullsweep_with_need(young_band + 1, 401, 321),
            young_target
        );
        assert_eq!(
            policy.young_after_fullsweep_with_need(young_target, 401, 321),
            young_target
        );
        // The 3 000-word floor is a **minor-only** rule and stays so with the need term: the same
        // capacity the minor keeps, the fullsweep hands back.
        // The 3 000-word floor is a **minor-only** rule and stays so: with `need == 0` the same
        // capacity the minor keeps (3 000) is handed back by the fullsweep, whose band edge there
        // is the bare `8 x 90 = 720` — no floor is involved on that side at all.
        assert_eq!(
            policy.young_after_collection_with_need(MINOR_SHRINK_FLOOR_WORDS, 0, 0),
            MINOR_SHRINK_FLOOR_WORDS
        );
        assert_eq!(
            policy.young_after_fullsweep_with_need(MINOR_SHRINK_FLOOR_WORDS, 0, 0),
            policy.young_target_words_with_need(0, 0)
        );
        // A need that lifts the fullsweep's target to 376 lifts its **band edge** to
        // `8 x 376 = 3 008`, above the 3 000-word capacity: the band — not any floor — is what
        // keeps that capacity there, exactly as the floor-free shape requires.
        assert_eq!(policy.young_target_words_with_need(0, 100), 376);
        assert_eq!(YOUNG_SHRINK_FACTOR * 376, 3_008);
        assert_eq!(
            policy.young_after_fullsweep_with_need(MINOR_SHRINK_FLOOR_WORDS, 0, 100),
            MINOR_SHRINK_FLOOR_WORDS
        );
        // ... and a need small enough to keep the fullsweep's target under a quarter of that
        // capacity hands it back, exactly as the floor-free rule demands.
        assert_eq!(policy.young_target_words_with_need(0, 20), 142);
        assert_eq!(
            policy.young_after_fullsweep_with_need(MINOR_SHRINK_FLOOR_WORDS, 0, 20),
            policy.young_target_words_with_need(0, 20)
        );
    }
}
