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
//! The pooled BIF-heap frontier's **need-aware reservation** — the policy
//! [`RuntimeContext::take_bif_frontier`](super::RuntimeContext::take_bif_frontier) applies, and the
//! answer to the spurious `error:system_limit` it used to raise.
//!
//! # The defect this exists for
//!
//! A registered BIF allocates its result on a pooled region whose words are **rewound** after the
//! call (see [`super::RuntimeContext::finish_registered_call`]), so per-call demand must be
//! expected to be a whole result at once. The frontier used to be grown only when the region
//! popped from the pool had less than `capacity() / 16` free words — a check that cannot foresee a
//! BIF needing far more than that fraction:
//!
//! ```text
//! spawn(fun() -> length(lists:duplicate(1 bsl 21, gurka)) end)
//! ```
//!
//! `lists:duplicate/2` builds `2^22` words (32 MiB) in one call. When the pooled region happened
//! to start that call with, say, `capacity()/8` free words — above the old floor — the BIF ran
//! out mid-allocation and the call raised `error:system_limit`, which the oracle has no analogue
//! for: every word such a BIF builds is transient. Measured pre-fix, the shape failed on exactly
//! one iteration per capacity step (the failure itself leaves the used words in place, so the
//! *next* call trips the old floor, grows the region and succeeds) — 1 failure in the 50-round
//! loop below, and 0 on the oracle.
//!
//! # The rule
//!
//! A call may only use a region that can serve the **reservation** the frontier needs:
//!
//! ```text
//! need = max(learned_at_this_depth, min(BIF_FRONTIER_MAX_WORDS, capacity / 4))
//! ```
//!
//! * the **entitlement** (`min(64 MiB, a quarter of the region)`) is what one call is guaranteed
//!   room for before anything has been observed. It is a *fraction of the region's own capacity*
//!   so it can never exceed what the region physically is — a small or `+hmax`-capped runtime
//!   (a 1 024- or 2 048-word region) keeps a proportionally small reservation instead of
//!   demanding a 64 MiB region it may not build — and it is capped in absolute terms so a region
//!   that has grown large does not reserve a quarter of itself for one call;
//! * the **learned** figure is the largest number of words a call at that depth has actually
//!   handed out of its region in one go ([`note_demand`]) — so a workload whose single-call demand
//!   exceeds the entitlement pays for it **once** and is reserved for from then on, instead of
//!   once per capacity step. Two depths are learned separately (outermost, nested) because a
//!   large outermost demand must not inflate every nested region's reservation.
//!
//! The growth step itself ([`grown_capacity`]) keeps the pre-existing geometric rule — **twice
//! the region it replaces** — as its floor and takes the reservation as a lower bound, so one
//! growth serves many calls. A `+hmax` (`max_heap_words`) ceiling still refuses growth below it,
//! exactly as before: that case is a configured limit, and the call keeps today's `system_limit`.
//!
//! # What is *not* done: a mid-BIF retry
//!
//! A BIF that exhausts the region raises `BeamError::Alloc(AllocKind::Heap)` — its words are gone
//! with the `BifContext` and there is no resume point — so the only recovery shape available here
//! would be to re-run the whole BIF on a wider frontier. That is **not** implemented, because no
//! seam exposes "this BIF has not yet mutated observable state", and a large share of the
//! registry *does* mutate it before it can fail:
//!
//! * the `SendHook`/`PortHook` families (`erlang:send/2,3`, `!`, `port_command/2,3`) — a message
//!   sent twice is a duplicated signal the oracle never produces;
//! * the `ets` family (`ets:insert/2`, `insert_new/2`, `delete/1`, `update_counter/3,4`,
//!   `select_delete/2`) — a second insert of a keyed object, or a second counter increment, is
//!   observable;
//! * the `DictHook`/`PersistentTerm` families (`put/2`, `erase/1`, `persistent_term:put/2`) and the
//!   timer heap (`send_after/3`, `start_timer/3`, `cancel_timer/1,2`) — a re-run would fire twice;
//! * the `ProcessHook` family (`spawn*`, `exit/2`, `link/1`, `monitor/2,3`, `register/2`) — a
//!   duplicate process or monitor is exactly the divergence class the differential gate hunts;
//! * the `NifLoader`/`OutputSink` families (`load_nif/2`, `io:format/2` and the rest of the io
//!   surface) — a NIF `load` callback and a printed line are both effects;
//! * anything reached through `RuntimeHooks::call` (the runtime's own floor) and the fun-applying
//!   `lists`/`maps` BIFs, whose fun may itself send or write.
//!
//! The remaining ones — the pure constructors (`lists:duplicate/2`, `make_tuple/2`,
//! `binary:copy/2`, `lists:seq/2`, the `maps:*` rebuild arms) — *are* retry-safe in isolation, but
//! keying a retry on a BIF roster would put the answer in a hand-maintained table that the next
//! BIF added to `beam_bifs` silently lands on the wrong side of. So the reservation is what must
//! be right, and it is: with the learned figure, the measured shape reserves its own demand and
//! never re-fails.
//!
//! # The honest residual
//!
//! A **first** call whose demand exceeds the entitlement *and* whose region lacks that much free
//! space still raises `error:system_limit` once; the demand is learned from the failure and every
//! later call of that shape is reserved for. The entitlement (64 MiB of transient words in one
//! call, or a quarter of a smaller region) is what makes that a rarity rather than the rule.
use crate::runtime::Runtime;
/// The largest number of words a single call is guaranteed to find free in the region it is given,
/// however large the region has grown.
///
/// `1 << 23` words = 64 MiB, twice the largest measured transient demand (`lists:duplicate/2` at
/// `1 bsl 21` builds `2^22` words). It is a **ceiling on the entitlement**, not on a call: a call
/// needing more than this is served from the region's remaining room, and the surplus is learned
/// ([`note_demand`]) after it succeeds.
pub(super) const BIF_FRONTIER_MAX_WORDS: usize = 1 << 23;
/// The fraction of a region's own capacity the entitlement is derived from — `capacity / 4`,
/// i.e. a call must find a quarter of the region free.
///
/// A quarter rather than the old `capacity() / 16`: the old floor was small enough that a single
/// BIF building a 32 MiB term could run past it. It is not the whole capacity, so an escalation
/// regime (a caller that legitimately retains its words: the in-process compiler's escaping route)
/// still fills a region to 75 % before the frontier moves on, which keeps the retired-region chain
/// (and the words `Runtime::for_each_off_heap_region` walks) at the same order as before.
pub(super) const BIF_FRONTIER_HEADROOM_DIVISOR: usize = 4;
/// The words a call at the runtime's current nesting depth must find free before it may use the
/// region it was handed — [`RuntimeContext::take_bif_frontier`](super::RuntimeContext::take_bif_frontier)'s
/// test, read from the runtime's learned figures and the region's own capacity.
///
/// `capacity` is the region the pool just handed over, so the entitlement scales with what the
/// runtime actually reserved (see [`BIF_FRONTIER_HEADROOM_DIVISOR`]).
///
/// `pub(crate)` because the pool's own tests derive their expected retirement point from it
/// (`crate::context::pool::tests`) instead of re-typing the arithmetic: an assertion about *when*
/// the frontier moves must read the same rule the taker applies.
#[inline]
pub(crate) fn need(runtime: &Runtime, capacity: usize) -> usize {
    let learned = if runtime.bif_heaps_taken <= 1 {
        runtime.bif_frontier_need_outer
    } else {
        runtime.bif_frontier_need_nested
    };
    learned.max(entitlement(capacity))
}
/// The reservation one call is guaranteed **before** anything has been learned — the
/// capacity-relative, absolutely capped half of [`need`].
#[inline]
pub(crate) fn entitlement(capacity: usize) -> usize {
    (capacity / BIF_FRONTIER_HEADROOM_DIVISOR).min(BIF_FRONTIER_MAX_WORDS)
}
/// Learns that the call currently holding a region at the runtime's nesting depth needed `words`
/// words of it at once.
///
/// Called from the registered-call tail with the region's `used() - mark` — the words this call
/// actually handed out — so the figure is a per-call demand and never a region high-water mark
/// (`Heap::peak_used` includes words below this call's `mark`, which belong to another caller and
/// would ratchet the reservation toward the whole capacity).
///
/// A `max`, so it never shrinks: the cost of learning is the growth step that follows.
#[inline]
pub(super) fn note_demand(runtime: &mut Runtime, words: usize) {
    if runtime.bif_heaps_taken <= 1 {
        runtime.bif_frontier_need_outer = runtime.bif_frontier_need_outer.max(words);
    } else {
        runtime.bif_frontier_need_nested = runtime.bif_frontier_need_nested.max(words);
    }
}
/// The capacity a frontier must be replaced by to serve a `need`-word reservation, or `None` when
/// no legal growth reaches it.
///
/// The pre-existing rule — **twice the region it replaces** — stays the floor, and the reservation
/// is a lower bound on top of it: one growth then serves a whole run of calls instead of a single
/// step. `ceiling` is the `+hmax` figure (`0` = no configured cap); a ceiling that cannot exceed
/// the current capacity is the documented refusal (the region is kept, and a call that really
/// overflows it raises `system_limit` exactly as before).
#[inline]
pub(super) fn grown_capacity(capacity: usize, need: usize, ceiling: usize) -> Option<usize> {
    let doubled = capacity.saturating_mul(2).max(need);
    let grown = if ceiling == 0 {
        doubled
    } else {
        doubled.min(ceiling)
    };
    (grown > capacity).then_some(grown)
}
#[cfg(test)]
mod tests;
