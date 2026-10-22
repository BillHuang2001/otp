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

//! **The store move** — Phase 1b: evacuating the live words of a *partially* live store region.
//!
//! # The case Phase 1a cannot act on
//!
//! [`crate::runtime::reclaim`]'s whole-region pass asks one boolean question per candidate range:
//! *does anything reachable stand inside it?* A range with no hit is dropped whole, which is the
//! cheap and complete answer for a region whose words all died together — the common shape of a
//! `-eval` compile, whose retired chain collapses without moving a word. What it cannot touch is
//! the **interleaved** shape: a statement that binds several values (`A = <build>, B = <build>, …`)
//! keeps one live word in each of several otherwise-dead regions, so every one of them is
//! *partially* live and each is retained **whole** — one live term pins an entire region.
//!
//! The move answers that shape: it evacuates every word reachable from the runtime's mutable root
//! corpus out of the store's current region and every retired candidate into **one** fresh
//! right-sized region, then drops the sources.
//!
//! # The corpus (what makes the move sound)
//!
//! A drop frees words; a move additionally *rewrites* them. So the enumeration must be complete
//! in a stronger sense than Phase 1a's: not only must every live word be *seen* (so no region is
//! dropped with a live word in it), its **slot** must be writable so the new address can be
//! installed. The corpus is therefore:
//!
//! * [`Runtime::for_each_root_mut`](crate::runtime::for_each_root_mut) — the runtime's eighteen
//!   host-side root families, the *complete* enumeration at exactly the two sites a pass runs at
//!   (`src/runtime/roots.rs`, *Completeness — this walk is whole only at a non-running site*);
//! * the **caller's own slots**, handed in as [`MoveSlots`]: the evaluator's `Env` values at the
//!   statement boundary, and at the driver exit those plus the driver's result — or a raise's
//!   reason term and its stacktrace head. Those are Rust locals in `crate::eval`, so they are not
//!   in the runtime's families; they are forwarded **in place**, which is what makes the driver's
//!   own live terms move with the store instead of being left naming an evacuated address.
//!
//! The **value gather alone is not sufficient for a move.** [`Runtime::gather_off_heap_roots`]
//! is a *liveness* enumeration: it deliberately omits the families whose words are a second copy,
//! already stable, or flattened behind another mechanism
//! ([`RootFamily::in_value_gather`](crate::runtime::RootFamily::in_value_gather) is the
//! machine-checkable record). A prune may lean on that; a mover may not — a family it skipped
//! still holds a word that has to be rewritten. That is why the move is built on the *moving*
//! twin, whose eighteen families are a superset of the value gather's.
//!
//! # The post-move verification, and why it is not a rollback
//!
//! [`beam_gc`]'s relocator writes a **forwarding marker** into every source object it copies, so
//! "undo the copy and leave the old regions untouched" is not achievable once a word has moved.
//! The sound sequence is instead:
//!
//! 1. build the destination fresh (nothing installed yet, nothing mutated);
//! 2. forward the corpus, run the destination scan to completion;
//! 3. **re-run the value descent from the forwarded roots** over a [`FromSet`] covering the OLD
//!    ranges ([`crate::eval::reclaim::walk`] with [`RangeProbe`], the same walker and the same sink
//!    the whole-region pass uses);
//! 4. on **any** hit — or on a copy that failed mid-way — keep **both**: the destination is
//!    installed as a *retired* store region (so the copies its rewritten roots name stay valid),
//!    every source region is left exactly where it is, and the pass declines with the reason. A
//!    source object the corpus did not enumerate is untouched; an object that did move is an
//!    indirection the next pass re-classifies on evidence. Nothing dangles;
//! 5. only on **zero** hits are the sources dropped and the destination installed as the current
//!    region.
//!
//! The one irreversible act is *dropping* the sources, and step 4 gates exactly that. The residual
//! is stated rather than hidden: a hit means some holder names a store word the corpus does not
//! enumerate. Keeping the sources makes that holder's word valid again except for an object that
//! was **also** reachable from the corpus and therefore moved (its source word is now a forwarding
//! marker, which only a marker-protocol reader follows). That is the residual cost of the copy
//! having happened, and it is why the move is gated on every pin and every doubt below — with a
//! complete corpus it cannot occur.
//!
//! # The hit is attributed, because "a holder outside the corpus" is not a diagnosis
//!
//! A hit that names no holder leaves a fix with nothing to act on, so step 3's verdict is followed
//! by an **attribution** ([`attribute_hit`], run only on the hit path): the corpus is re-walked one
//! candidate at a time — the caller's slots, then each runtime family, and within a family that
//! reproduces the hit, its individual roots — and the first reproduction is published as
//! [`RegionReclaimStats::store_move_hit`](crate::runtime::RegionReclaimStats::store_move_hit)
//! (`Family(family, slot)`, `CallerSlot(index)`, or `OutsideCorpus` when no slot of the corpus
//! reproduces it). Each variant answers a different question for whoever fixes the corpus: a family
//! slot means the walk *has* a slot for that word, so the copy skipped or mis-copied it; a caller
//! slot means the caller's own corpus did; `OutsideCorpus` means the holder is a store neither
//! enumerates. A walking decline (the visit budget, an unknown boxed header) is still a hit, and its
//! unknown header rides the pass's `reclaim_last_decline_header` field so one run reports both the
//! move's refusal and the header it met.
//!
//! # Thresholds
//!
//! The move costs one count walk, one copy and two verification descents. It therefore runs only
//! when it is clearly worth it, and the decision is made **before the first word moves**:
//!
//! * the candidate sources must hold at least [`MIN_WINDOW_WORDS`] words in total, and
//! * at least **half** of them must be dead (`total - live`, by the exact count
//!   [`beam_gc::live_words_in`] gives over the same corpus — it is exactly the `words_copied` the
//!   relocator will report, so the destination is sized from a real figure rather than a guess),
//!   and
//! * it must **drop at least one region** — otherwise the move would install a fresh region
//!   without releasing one, which is a pure loss. A current region with a non-empty below-cursor
//!   prefix survives as a retired region (that prefix is not a candidate and must never move), so
//!   the move needs either a retired candidate or a current region that starts at word zero.
//!
//! Below a threshold the pass is **skipped** silently (the whole-region action still runs); above
//! it a refusal is recorded on the move's own counters
//! ([`RegionReclaimStats::store_move_declines`](crate::runtime::RegionReclaimStats)).
//!
//! # Decline list (each recorded, both regions left usable)
//!
//! * the move switch is off ([`RegionReclaimConfig::store_move`](crate::runtime::RegionReclaimConfig));
//! * a **runtime-lifetime pin** latched the window at either site — a `stabilize_term` store that
//!   the family walk may not cover, or a decoded port caller tag. This refusal is **empirically
//!   necessary, not conservative-by-accident**: with it bypassed the post-move verification below
//!   *hits* on `cerl_clauses` (a live store word really does stand in an old region that the
//!   mutable corpus cannot rewrite) and the compile then fails with `system_limit`, so the pin is
//!   the guard that keeps an incomplete corpus from costing a live term;
//! * a machine in flight — decided by the pass's shared refusal before this module runs, because
//!   a running machine's roots are unpublished;
//! * a recorded raise's live reason word whose **address** falls inside a candidate range: it has
//!   a reader ([`RunnerState::for_each_error_live_reason`]) and no writable slot, so a region it
//!   stands in may not move (mere presence is not a decline — a reason outside every candidate
//!   range is unaffected);
//! * the destination could not be allocated, or would exceed the store's own ceiling;
//! * the destination could not reserve the live set — the preflight
//!   ([`Relocator::new_checked`]) refuses **before** the first word moves;
//! * the corpus enumeration or the value descent could not complete;
//! * the verification hit (above), or a copy that failed after the first word moved.
//!
//! # Region bookkeeping
//!
//! The destination becomes [`Runtime::eval_heap`]. The old current region is `reset_to(mark)` and
//! pushed onto [`Runtime::eval_heap_retired`] — or dropped when `mark == 0`, which is what makes
//! the move release a carrier. Every retired candidate is dropped. Two address-valued pieces of
//! bookkeeping are then **re-anchored**, because both name a region that no longer exists as the
//! current one: the pass's own cursor ([`super::advance_cursor`]) and the statement window
//! ([`ScratchState::stop`](crate::eval::scratch::ScratchState::stop) — a mark is an offset into the
//! region it was taken in, so the window is dropped and the next statement opens a fresh one above
//! the destination's frontier).
//!
//! # The address-keyed maps
//!
//! `Eterm`'s `Hash`/`Eq` are the raw word, so every term-keyed map in the runtime is keyed by
//! **address**. Four exist; one is store-keyed and three are not:
//!
//! * [`SelectWatch::by_fd_handle`](crate::port::SelectWatch) is keyed `(fd, handle.raw())` while
//!   the handle term itself **is** forwarded by the `SelectPayload` root family, so the index is
//!   **recomputed** in the same pass ([`PortRegistry::rekey_select_handles`]). Every entry is one
//!   live registration, so a rebuild from the registrations is exact.
//! * `Runtime::references`, `Timers::refs` and `TimerWakes::deliver` need **no** re-keying, and the
//!   reason is structural rather than a judgement call: each key is minted on a **pooled BIF heap**
//!   (`src/native/process.rs`'s `reference_term`, `src/native.rs`'s `mint_timer_ref`), which is a
//!   different region family from the store. `eval_heap`/`eval_heap_retired` are the only ranges
//!   this move evacuates, so a pooled-BIF-heap word can never be one of its keys.
//!
//! # Off-heap payloads
//!
//! A `[BinRef][ErlSubBits]` pair moves as an **ordinary block**: the relocator classifies the
//! `BinRef` as a leaf and `BinRef::val` is not a heap word, so the payload pointer is left exactly
//! as it is (`Target::Foreign`). The payload *handles* live in `Runtime::eval_off_heap` and are
//! released only by `crate::eval::reclaim::prune`, which re-derives reachability from
//! [`Runtime::for_each_off_heap_region`](crate::runtime::for_each_off_heap_region). The destination
//! is `eval_heap` (or, on a post-copy refusal, a retired region), both of which that enumeration
//! covers — so a move can neither orphan nor over-release a payload.

// The move is one of the workspace's explicitly-marked `unsafe` modules: it builds the two
// `beam_gc` primitives whose contract is the caller's own completeness claim (the evacuated
// range set and the destination region), and it advances the destination's bump frontier over
// words the relocator already wrote. Every site carries its `// SAFETY:` rationale.
#![allow(unsafe_code)]

use core::mem::size_of;

use beam_gc::{FromSet, LinearDest, Relocator};
use beam_term::Eterm;

use super::{advance_cursor, heap_region_base, MoveHitOwner, PassInput};
use crate::eval::reclaim::{self, Reach, Walk};
use crate::eval::scratch::{RangeProbe, MIN_WINDOW_WORDS};
use crate::runtime::Runtime;
/// The move's own decline reasons, one `&'static str` each so a measurement can grep the reason
/// out of the `[rt-mem]` line.
const R_DEST_ALLOC: &str = "the store move could not allocate a destination region";
const R_DEST_CEILING: &str = "the store move's destination would exceed the store's region ceiling";
const R_CAPACITY: &str =
    "the destination region cannot reserve the live set, so the move is refused before the first word moves";
const R_COPY: &str = "the destination refused a word after the first copy, so every source region is kept";
const R_SCAN: &str = "the destination scan refused a word, so every source region is kept";
/// The verification hit's reason. `pub(super)` so the tripwire's test can name it: the reason a
/// forced hit records is part of what that test asserts.
pub(super) const R_VERIFY_HIT: &str =
    "the post-move verification found a live word in an old store region, so every source region is kept";
const R_ENUMERATION: &str = "the runtime's root enumeration refused to complete";
const R_REASON: &str =
    "a recorded raise's live reason word stands inside a region the move would evacuate";
const R_PIN: &str = "a runtime-lifetime store latched the window, and a move cannot rewrite its slot";
const R_SWITCH: &str = "the store move switch is off";

/// The caller's own live terms as **writable slots** — the half of the move's corpus that is not
/// a runtime root family.
///
/// Two callers build one: the `-eval` statement boundary hands over its environment's values, and
/// the driver exit hands over those plus the driver's own result (or a raise's reason term and its
/// stacktrace head). The slots are forwarded **in place** by the move, so the caller's terms name
/// the destination rather than an evacuated address.
///
/// An empty list is a valid corpus half (a pass with no caller-owned roots).
#[derive(Default)]
pub(crate) struct MoveSlots<'a> {
    /// The writable slots, in the caller's order.
    slots: Vec<&'a mut Eterm>,
}

impl<'a> MoveSlots<'a> {
    /// A corpus half over `slots`.
    pub(crate) fn new(slots: Vec<&'a mut Eterm>) -> MoveSlots<'a> {
        MoveSlots { slots }
    }

    /// The **current values** of the slots, in order — the immutable snapshot the count walk and
    /// the value descent use. A move rewrites the slots, never this copy.
    pub(crate) fn values(&self) -> Vec<Eterm> {
        self.slots.iter().map(|slot| **slot).collect()
    }

    /// Hands `f` each slot in turn, stopping at the first error.
    ///
    /// # Errors
    ///
    /// Whatever `f` answers.
    fn for_each(
        &mut self,
        f: &mut dyn FnMut(&mut Eterm) -> beam_common::Result<()>,
    ) -> beam_common::Result<()> {
        for slot in self.slots.iter_mut() {
            f(&mut **slot)?;
        }
        Ok(())
    }
}

/// What one move attempt decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MoveOutcome {
    /// The live words were evacuated and the sources dropped.
    Moved,
    /// Nothing to do (below a threshold, nothing live, nothing to gain) — not a decline, and the
    /// whole-region action still runs.
    Skipped,
    /// A refusal **before the first word moved**, recorded on the move's own counters; every
    /// region is byte-identical and the whole-region action still runs.
    Declined,
    /// A refusal **after** a word moved (a failed copy, or the verification hit): the destination
    /// stays installed and every source region is left exactly where it is, so the whole-region
    /// action must **not** run for this pass. Its verdict is the same descent that just failed to
    /// see the holder [`keep_both`] exists for, so its corpus is known to be incomplete for these
    /// ranges: dropping on it would free a region whose live word the verification had just found.
    /// The next pass re-classifies every region on fresh evidence.
    KeptBoth,
}

/// One **move attempt**, from the threshold decision to the commit.
///
/// Returns [`MoveOutcome::Moved`] only when the sources were dropped and the destination installed
/// as the current region — the pass then skips the whole-region action, because there is nothing
/// left for it to classify.
pub(crate) fn attempt_move(
    runtime: &mut Runtime,
    input: &PassInput<'_>,
    slots: &mut MoveSlots<'_>,
) -> MoveOutcome {
    if !runtime.region_reclaim.config.store_move {
        runtime.region_reclaim.decline_move(R_SWITCH);
        return MoveOutcome::Declined;
    }
    // A runtime-lifetime store may name a word in a region this move would evacuate and has no
    // writable slot: the family walk might not cover it, so doubt is a refusal at **both** sites
    // (unlike the whole-region pass, whose corpus is complete and needs no latch).
    if input.window.lifetime_pinned.is_some() {
        runtime.region_reclaim.decline_move(R_PIN);
        return MoveOutcome::Declined;
    }
    // **No "a machine is parked" gate belongs here.** A parked machine's root words are already in
    // this move's corpus: [`Runtime::for_each_root_mut`](crate::runtime::for_each_root_mut) reaches
    // the parked-machine family
    // ([`RunnerState::for_each_parked_machine_root_mut`](crate::runner::RunnerState::for_each_parked_machine_root_mut),
    // which visits the live `x` prefix, the Erlang stack and the FR prefix) and therefore *forwards*
    // them, so a word a parked window holds ends up naming the destination like every other root.
    // What a parked window can still contribute is a word the corpus **cannot** rewrite: one at the
    // edge of the engine's used extent that the process has finished with and whose address happens
    // to coincide with a store region (a released heap the store's mapping later covered). That is
    // precisely the shape the **post-copy verification** below exists for, and it answers it with
    // `MoveOutcome::KeptBoth`: every source region stays installed and the destination is kept too,
    // so the stale word still names its region. The refusal is thus the verification hit, decided by
    // an address test over the real corpus, and never a *proxy* for it.
    //
    // "Any engine in the table is parked" is the wrong predicate on two counts. It is not the
    // question (what matters is whether a held word stands in a candidate range, which the
    // verification asks directly and per-word), and on this runtime the answer is unboundedly
    // uninteresting: a process waiting in a `receive` is the *normal* state, so a workload with one
    // parked server has every pass decline, which is exactly the state that once made
    // `store_compactions` structurally unreachable. The gate's only effect was to disable the move
    // permanently — it bought no soundness the verification does not already give.
    let Some(cursor) = runtime.region_reclaim.cursor else {
        // The shared refusal already declined this pass; kept as a typed belt-and-braces branch.
        return MoveOutcome::Skipped;
    };
    let width = size_of::<Eterm>();
    // The candidate geometry: the current region above the pass's cursor, plus every retired
    // region this window created. A region the cursor does not name (a growth moved the current
    // region inside the window) is entirely a window region, so its whole prefix is a candidate.
    let base = heap_region_base(&runtime.eval_heap);
    let used = runtime.eval_heap.used();
    let mark = if base == cursor.store_base {
        cursor.store_mark
    } else {
        0
    };
    if mark > used {
        // Cannot happen (`advance_cursor`/`raise_store_floor` keep the mark below the frontier);
        // refuse rather than fabricate a candidate range.
        return MoveOutcome::Skipped;
    }
    let retired: Vec<usize> = (cursor.store_retired_baseline..runtime.eval_heap_retired.len())
        .filter(|index| runtime.eval_heap_retired[*index].used() > 0)
        .collect();
    let mut total = used - mark;
    for index in &retired {
        total = total.saturating_add(runtime.eval_heap_retired[*index].used());
    }
    if total < MIN_WINDOW_WORDS {
        // Below the same threshold the statement window uses: a descent and a copy would not
        // repay themselves.
        return MoveOutcome::Skipped;
    }
    // The move must release at least one carrier, or it would install a fresh region for nothing.
    if mark != 0 && retired.is_empty() {
        return MoveOutcome::Skipped;
    }
    // The candidate ranges, in the `(base, used_words)` form `FromSet::from_ranges` takes.
    let mut ranges: Vec<(usize, usize)> = Vec::with_capacity(retired.len() + 1);
    if used > mark {
        ranges.push((base + mark * width, used - mark));
    }
    for index in &retired {
        ranges.push((
            heap_region_base(&runtime.eval_heap_retired[*index]),
            runtime.eval_heap_retired[*index].used(),
        ));
    }
    // The same ranges in the **byte** form every probe here takes: [`FromSet`]'s containment test
    // wants `(base, used_words)` because that is the shape the copier classifies against, while
    // [`RangeProbe`] asks the walker whether a reached word's *address* stands inside a half-open
    // `[lo, hi)` byte range. The two forms are derived from one vector so they cannot drift.
    let byte_ranges: Vec<(usize, usize)> = ranges
        .iter()
        .map(|(base, words)| (*base, base.saturating_add(words.saturating_mul(width))))
        .collect();
    // A recorded raise's live reason has a reader and no writable slot: a region it stands in may
    // not move. Presence alone is not a refusal — a reason outside every candidate range is
    // unaffected by the move.
    {
        let mut reason_hits = vec![false; byte_ranges.len()];
        let mut probe = RangeProbe::new(&byte_ranges, &mut reason_hits);
        runtime
            .runner_state
            .for_each_error_live_reason(&mut |term| probe.word(term));
        if reason_hits.iter().any(|hit| *hit) {
            runtime.region_reclaim.decline_move(R_REASON);
            return MoveOutcome::Declined;
        }
    }
    // SAFETY: every range is one of this runtime's own store regions — the current `eval_heap`
    // region's written prefix above the pass's cursor, or a retired store region's whole written
    // prefix. Each is word-aligned, `&mut Runtime` makes the words exclusively readable and
    // writable for the duration of this call, the extents are the regions' own `used` frontiers
    // (never a fabricated one), the carrier is never freed or reallocated while the set is in use
    // (a `beam_alloc::Heap` never moves its block, and no region is dropped until the commit), and
    // the ranges are pairwise disjoint because they are distinct heap allocations.
    let from = unsafe { FromSet::from_ranges(&ranges) };
    // The count walk, over the **exact** corpus the copy will forward: the caller's live slots
    // plus every runtime root family, in that order. Nothing else may be added — `live_words_in`
    // dedups by object, so its answer is precisely the `words_copied` the relocator reports for
    // this from-set and these roots, which is what lets the destination be sized from a real
    // figure and `need <= destination capacity` hold by construction. The immutable
    // `PassInput::value_roots` snapshot is deliberately **not** part of it: it may name a slot the
    // caller has already replaced, and counting a word the copy will not forward would inflate the
    // destination (harmless) while the converse — forwarding a word that was never counted — is
    // what the preflight must refuse.
    let mut corpus: Vec<Eterm> = slots.values();
    if runtime
        .for_each_root_mut(&mut |words| {
            corpus.extend_from_slice(words);
            Ok(())
        })
        .is_err()
    {
        runtime.region_reclaim.decline_move(R_ENUMERATION);
        return MoveOutcome::Declined;
    }
    let need = beam_gc::live_words_in(&from, &corpus);
    #[cfg(test)]
    tripwire::record_pre_move_root(&corpus, slots.slots.len(), &byte_ranges);
    drop(corpus);
    if need == 0 {
        // Nothing in the candidates is live: the whole-region action drops them all, and a move
        // would only copy nothing into a fresh region.
        return MoveOutcome::Skipped;
    }
    let dead = total.saturating_sub(need);
    if dead.saturating_mul(2) < total {
        // Less than half dead: the copy would cost more than it reclaims.
        return MoveOutcome::Skipped;
    }
    // The destination: one fresh right-sized region, with a margin so the copy never runs out of
    // words it was counted for.
    let margin = core::cmp::max(need / 8, 64);
    let capacity = need.saturating_add(margin);
    if capacity > runtime.eval_heap_ceiling() {
        runtime.region_reclaim.decline_move(R_DEST_CEILING);
        return MoveOutcome::Declined;
    }
    let Ok(destination) = beam_alloc::Heap::new(capacity) else {
        runtime.region_reclaim.decline_move(R_DEST_ALLOC);
        return MoveOutcome::Declined;
    };
    let copied = {
        let dest_base = destination.hp();
        // SAFETY: `destination` is a fresh, exclusively owned region of `capacity` words whose
        // block never moves for the `Heap`'s lifetime, and it is a new allocation, so it cannot
        // overlap any range of `from` (the copier writes forwarding markers into the sources).
        let mut dest = unsafe { LinearDest::new(&from, dest_base, capacity) };
        let copy = forward_corpus(runtime, slots, &from, &mut dest, need);
        let copied = dest.used_words();
        (copy, copied)
    };
    let (copy, copied) = copied;
    if copy.is_err() {
        keep_both(runtime, destination, copied, copy.err().unwrap_or(R_COPY));
        return MoveOutcome::KeptBoth;
    }
    if let Some(owner) = verification_owner(runtime, slots, &byte_ranges) {
        runtime.region_reclaim.note_store_move_hit(owner);
        keep_both(runtime, destination, copied, R_VERIFY_HIT);
        return MoveOutcome::KeptBoth;
    }
    commit(runtime, destination, copied, mark, &retired, dead);
    MoveOutcome::Moved
}

/// Forwards the whole corpus and runs the destination scan — the copy half, isolated so the
/// caller keeps ownership of the destination (and can measure what was copied on a failure).
///
/// # Errors
///
/// The reason of the first refusal: the capacity preflight (nothing moved), a copy that ran out of
/// destination mid-way, or the destination scan's own refusal. The caller treats every one of them
/// as "keep both regions".
fn forward_corpus(
    runtime: &mut Runtime,
    slots: &mut MoveSlots<'_>,
    from: &FromSet,
    dest: &mut LinearDest<'_>,
    need: usize,
) -> core::result::Result<(), &'static str> {
    let mut relocator = Relocator::new_checked(from, dest, need).map_err(|_| R_CAPACITY)?;
    runtime
        .for_each_root_mut(&mut |words| relocator.forward_roots(words))
        .map_err(|_| R_COPY)?;
    slots
        .for_each(&mut |slot| relocator.forward_roots(core::slice::from_mut(slot)))
        .map_err(|_| R_COPY)?;
    relocator.complete().map(|_relocation| ()).map_err(|_| R_SCAN)
}

/// Re-runs the **value** descent from the forwarded roots over the OLD ranges: does any live term
/// still stand in a region about to be dropped? And, if so, **which slot of the corpus** names it?
///
/// This is the gate on the move's one irreversible act. It descends from the slots and the runtime
/// families *after* forwarding, so a root that moved names the destination and only a holder the
/// corpus does not enumerate can put a word back into a source range. A descent that cannot
/// complete is a hit — doubt is never an answer.
///
/// `None` means **no hit**: every word the corpus names stands in a region that stays, so the move
/// may commit. `Some(owner)` means hit **or** doubt, and `owner` is the attribution: the first slot
/// that reproduces the hit ([`MoveHitOwner`]), or [`MoveHitOwner::OutsideCorpus`] when no slot of
/// the corpus does. The attribution runs **only here**, on the hit path, so its cost (up to one
/// extra descent per corpus slot) is never paid by a move that verifies clean.
///
/// The unknown header, when that is what this walk met, is recorded on the pass's
/// `reclaim_last_decline_header` field too ([`RegionReclaim::note_decline_header`]) — the move's own
/// reason stays [`R_VERIFY_HIT`], so one run reports both facts.
fn verification_owner(
    runtime: &mut Runtime,
    slots: &MoveSlots<'_>,
    ranges: &[(usize, usize)],
) -> Option<MoveHitOwner> {
    let roots = match verification_roots(runtime, slots) {
        Ok(roots) => roots,
        // The corpus enumeration refused to complete. Nothing was checked, so the verification's
        // answer is doubt — a hit — and no slot can be named.
        Err(()) => return Some(MoveHitOwner::OutsideCorpus),
    };
    let mut hits = vec![false; ranges.len()];
    let verdict = reclaim::walk(&roots, &mut RangeProbe::new(ranges, &mut hits));
    drop(roots);
    if let Some(_header) = verdict.header() {
        runtime.region_reclaim.note_unknown_header(&verdict);
        super::holder::walk_decline(runtime, &verdict, ranges, Some(slots));
    }
    let hit = match verdict {
        Walk::Complete => hits.iter().any(|hit| *hit),
        // A declined walk is a hit (see the module docs): what it could not read may name a source.
        Walk::Declined(_) | Walk::UnknownHeader { .. } => true,
    };
    if !hit {
        return None;
    }
    let owner = attribute_hit(runtime, slots, ranges);
    super::holder::move_hit(runtime, slots, ranges, owner);
    Some(owner)
}

/// The verification's root corpus: the caller's slots as they are now (forwarded), then every
/// runtime family — plus, test-only, the injected pre-move copy ([`tripwire`]).
///
/// # Errors
///
/// `()` when the runtime's root enumeration refuses to complete; the caller treats that as doubt.
fn verification_roots(
    runtime: &mut Runtime,
    slots: &MoveSlots<'_>,
) -> core::result::Result<Vec<Eterm>, ()> {
    let mut roots: Vec<Eterm> = slots.values();
    runtime
        .for_each_root_mut(&mut |words| {
            roots.extend_from_slice(words);
            Ok(())
        })
        .map_err(|_| ())?;
    // Test-only: the injected copy of an evacuated root, appended to the **verification** corpus
    // only — the forwarding corpus was already consumed, so the copy in the source region stands
    // there unrewritten and the descent above must hit it. See [`tripwire`].
    #[cfg(test)]
    if let Some((word, _slot)) = tripwire::recorded_hit() {
        roots.push(word);
    }
    Ok(roots)
}

/// Attributes the verification's hit: **which slot of the move's corpus reproduces it**.
///
/// One descent per candidate, each asking the same question ("does this candidate's graph stand a
/// term inside one of the old ranges?"), in the corpus's own order:
///
/// 1. the **caller's slots**, one at a time — their indices are the caller's own;
/// 2. test-only, the injected pre-move copy, attributed to the slot it was taken from (it is a
///    *snapshot* of that slot, which the copy has since forwarded);
/// 3. the runtime's **families**, family by family through
///    [`Runtime::for_each_root_family_mut`](crate::runtime::Runtime::for_each_root_family_mut):
///    first one descent over the family's whole slice (a cheap reject), then — only for a family
///    that hits — one descent per root of that slice, to name the slot.
///
/// A candidate whose own descent cannot complete is **doubt**, not a reproduction: it never reported
/// a word inside a range, so attributing the hit to it would name a holder that does not name it.
/// With no reproduction at all the answer is [`MoveHitOwner::OutsideCorpus`] — the finding itself.
fn attribute_hit(
    runtime: &mut Runtime,
    slots: &MoveSlots<'_>,
    ranges: &[(usize, usize)],
) -> MoveHitOwner {
    for (index, slot) in slots.slots.iter().enumerate() {
        if reproduces(core::slice::from_ref(*slot), ranges) {
            return MoveHitOwner::CallerSlot(index);
        }
    }
    #[cfg(test)]
    if let Some((word, Some(index))) = tripwire::recorded_hit() {
        if reproduces(core::slice::from_ref(&word), ranges) {
            return MoveHitOwner::CallerSlot(index);
        }
    }
    let mut found: Option<MoveHitOwner> = None;
    // An `Err` from the walk stops it; every family visited before it kept its writes (there are
    // none — this is a read-only descent), and `found` is whatever was attributed so far.
    let _ = runtime.for_each_root_family_mut(&mut |family, words| {
        if found.is_some() || !reproduces(words, ranges) {
            return Ok(());
        }
        for (index, word) in words.iter().enumerate() {
            if reproduces(core::slice::from_ref(word), ranges) {
                found = Some(MoveHitOwner::Family(family, index));
                break;
            }
        }
        Ok(())
    });
    found.unwrap_or(MoveHitOwner::OutsideCorpus)
}

/// Whether a descent from `roots` completes **and** reports a term standing inside one of `ranges`.
///
/// The same walker and the same sink the verification itself uses, asked of one candidate at a time;
/// a descent that cannot complete answers `false` (doubt is never a reproduction).
fn reproduces(roots: &[Eterm], ranges: &[(usize, usize)]) -> bool {
    let mut hits = vec![false; ranges.len()];
    match reclaim::walk(roots, &mut RangeProbe::new(ranges, &mut hits)) {
        Walk::Complete => hits.iter().any(|hit| *hit),
        Walk::Declined(_) | Walk::UnknownHeader { .. } => false,
    }
}
/// **Test-only fault injection** for the post-copy verification: arms a forced hit.
///
/// [`keep_both`]'s path is a *proof obligation* — "if the verification hits, no source region
/// dangles" — that a production run reaches only when the moving corpus is incomplete, which a
/// correct runtime never does. This module makes the obligation executable: armed on the test's
/// own thread, [`attempt_move`] records one **pre-move copy** of a caller slot that stands inside a
/// candidate range, and [`verification_roots`] appends that copy to the *verification* corpus only,
/// after the forward. Nothing enters the forwarding corpus or the count walk, so the copy in the
/// source region is never rewritten and the descent necessarily hits it — the same shape an
/// unenumerated holder produces.
///
/// Two properties make the injection honest rather than a weakening of a guard:
///
/// * it lives entirely behind `#[cfg(test)]` (no release build contains it), and it adds a root
///   **only** to the verification descent — the very check it forces to fail;
/// * the recorded word is a real root of the real corpus, so what the descent then reports is a
///   genuine "a live word stands in an old region", not a fabricated verdict.
///
/// The record also carries **which caller slot the word was taken from**, so the hit's attribution
/// ([`super::attribute_hit`]) names that slot ([`MoveHitOwner::CallerSlot`]) instead of
/// `OutsideCorpus`: the injected word is a snapshot of a slot the copy has since forwarded, so the
/// slot's *current* value can no longer reproduce the hit and only the record can say where it came
/// from. A corpus word from a runtime family is recorded with no slot (`None`); the attribution then
/// falls through to the family walks, which is correct for a family-rooted injection.
///
/// The state is thread-local because the workspace's tests share a process: a global cell would let
/// one case's arming explain another's decline.
///
/// The recorded root is chosen to be a word the walker can *read* after forwarding: a cons cell
/// (its car is `THE_NON_VALUE` and its cdr a destination pointer, both of which the descent's
/// guards handle), never a boxed object whose header word now holds the BOXED marker — that shape
/// is unreadable by a value descent by construction and is the residual [`keep_both`] documents.
#[cfg(test)]
pub(super) mod tripwire {
    use core::cell::Cell;

    use beam_term::Eterm;

    thread_local! {
        /// Whether this thread's moves must force a verification hit.
        static ARMED: Cell<bool> = const { Cell::new(false) };
        /// The pre-move copy of one evacuated root, once an attempt has recorded it, plus the
        /// **caller slot** it was taken from (`None` for a runtime-family word).
        static PRE_MOVE: Cell<Option<(Eterm, Option<usize>)>> = const { Cell::new(None) };
    }

    /// Arms the injector on this thread (and forgets any earlier recording).
    pub(crate) fn arm() {
        PRE_MOVE.with(|cell| cell.set(None));
        ARMED.with(|cell| cell.set(true));
    }

    /// Disarms the injector on this thread.
    pub(crate) fn disarm() {
        ARMED.with(|cell| cell.set(false));
        PRE_MOVE.with(|cell| cell.set(None));
    }

    /// Whether this thread's moves are to force a hit.
    fn armed() -> bool {
        ARMED.with(Cell::get)
    }

    /// Records the first word of `corpus` that stands inside one of `ranges` — a root the copy is
    /// about to evacuate, saved **before** it moves — with the caller slot it came from, when it came
    /// from the caller's own half of the corpus (the first `slot_count` words).
    pub(super) fn record_pre_move_root(
        corpus: &[Eterm],
        slot_count: usize,
        ranges: &[(usize, usize)],
    ) {
        if !armed() {
            return;
        }
        let found = corpus
            .iter()
            .copied()
            .enumerate()
            .find(|(_, word)| inside_a_range(*word, ranges));
        if let Some((index, word)) = found {
            let slot = (index < slot_count).then_some(index);
            PRE_MOVE.with(|cell| cell.set(Some((word, slot))));
        }
    }

    /// The recorded pre-move copy and the caller slot it was taken from, if this thread armed the
    /// injector and an attempt recorded one.
    pub(super) fn recorded_hit() -> Option<(Eterm, Option<usize>)> {
        if !armed() {
            return None;
        }
        PRE_MOVE.with(Cell::get)
    }

    /// Whether `word` is a term naming an address inside one of `ranges` — the same test
    /// [`crate::eval::scratch::RangeProbe`] applies, so the recorded word is exactly one the
    /// verification would report.
    fn inside_a_range(word: Eterm, ranges: &[(usize, usize)]) -> bool {
        if beam_term::literal::is_literal(word)
            || !beam_term::tag::is_term_word(word)
            || !(beam_term::boxed::is_boxed_term(word) || beam_term::prelude::is_list(word))
        {
            return false;
        }
        let address = beam_term::list::ptr_val(word) as usize;
        address != 0
            && ranges
                .iter()
                .any(|range| address >= range.0 && address < range.1)
    }
}

/// The **post-copy refusal**: keep the destination (its copies are what the forwarded roots now
/// name) and every source region, and record why.
///
/// The destination is installed on [`Runtime::eval_heap_retired`], whose `Drop` frees its carrier
/// and whose words [`Runtime::for_each_off_heap_region`] visits — so the copies stay valid *and*
/// stay visible to the off-heap payload scan. Its frontier is advanced to the words the copy really
/// wrote, so the region's live prefix is exactly what it holds.
///
/// # The verification's residual, stated rather than hidden
///
/// Keeping both regions makes every holder's word usable again **except** one shape: an object that
/// the corpus *did* enumerate therefore **moved**, and its old address now holds a forwarding
/// marker. A holder the corpus missed that named such an object reads a marker word instead of the
/// object — an indirection only a marker-protocol reader follows. And the verification's own reach is
/// bounded the same way the descent's is: a **boxed** object whose source word was forwarded holds
/// the `BOXED` marker in its header, which no *value* descent can read, so that shape is a miss the
/// check structurally cannot report (a cons cell, whose two words are readable, is the shape it
/// can). With a complete corpus neither case can arise, which is why every pin and every doubt above
/// is a refusal rather than a warning.
fn keep_both(runtime: &mut Runtime, mut destination: beam_alloc::Heap, copied: usize, reason: &'static str) {
    // The words were written by the relocator; `alloc_words` only advances the frontier over them,
    // which is what makes the region's written prefix (`used_words_mut`, the payload scan, and
    // `heap_region_base`) exactly the copies.
    if copied > 0 && unsafe { destination.alloc_words(copied) }.is_err() {
        // Unreachable: the relocator hands out at most the destination's capacity, which was
        // priced above the count. A destination that cannot claim its own copies is still kept —
        // its roots name it — and the decline reason says the copy is not fully accounted for.
        runtime.region_reclaim.decline_move(R_DEST_ALLOC);
        runtime.eval_heap_retired.push(destination);
        return;
    }
    runtime.eval_heap_retired.push(destination);
    runtime.region_reclaim.decline_move(reason);
}

/// The **commit**: the destination becomes the current region, the old current region is rewound to
/// the window's mark (or dropped, when the mark is word zero) and every retired candidate is
/// dropped.
fn commit(
    runtime: &mut Runtime,
    mut destination: beam_alloc::Heap,
    copied: usize,
    mark: usize,
    retired: &[usize],
    dead: usize,
) {
    // The destination's frontier is the words the copy wrote; `alloc_words` only advances the bump
    // pointer over words the relocator already wrote.
    //
    // SAFETY: `destination` is exclusively owned here and the relocator handed out no more than its
    // capacity, so `copied` words are available and written.
    let ok = copied == 0 || unsafe { destination.alloc_words(copied) }.is_ok();
    debug_assert!(ok, "the destination must be able to claim the words it holds");
    let mut old_current = core::mem::replace(&mut runtime.eval_heap, destination);
    let mut dropped = 0u64;
    // Descending, so removing one candidate does not shift the index of a later one.
    for index in retired.iter().rev() {
        let region = runtime.eval_heap_retired.remove(*index);
        dropped = dropped.saturating_add(1);
        drop(region);
    }
    // The old current region's words below the pass's cursor are **not** candidates: they were
    // written before this window opened, by an earlier `-eval` run whose own root set this pass
    // cannot enumerate. Rewinding to the mark (never to zero) keeps them exactly where they are.
    if old_current.reset_to(mark).is_err() {
        // Unreachable (the mark was checked against the frontier above); keeping the region whole
        // loses nothing but the reset.
        runtime.eval_heap_retired.push(old_current);
    } else if old_current.used() == 0 {
        dropped = dropped.saturating_add(1);
        drop(old_current);
    } else {
        runtime.eval_heap_retired.push(old_current);
    }
    let stats = &mut runtime.region_reclaim.stats;
    stats.store_compactions = stats.store_compactions.saturating_add(1);
    stats.store_words_reclaimed = stats
        .store_words_reclaimed
        .saturating_add(dead as u64);
    stats.store_regions_dropped = stats.store_regions_dropped.saturating_add(dropped);
    // Every address-valued index over the store's terms is now stale: the select registrations'
    // `(fd, handle)` index is rebuilt from the (forwarded) handles.
    runtime.ports.borrow_mut().rekey_select_handles();
    // The pass's cursor and the statement window both name a region that is no longer current.
    advance_cursor(runtime);
    runtime.eval_scratch.stop();
}
