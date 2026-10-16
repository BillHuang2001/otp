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

//! The **atomic promotion** driver of [`GenImmixCollector`](super::GenImmixCollector):
//! evacuating the nursery (and, on a relocating fullsweep, the old mature region) into a
//! [`ImmixSpace`], with a rollback that makes a would-not-fit promotion unobservable.
//!
//! # Why this is not `copy.rs::forward_slot`
//!
//! The shared marker protocol ([`forward_slot`](crate::copy::forward_slot) over the
//! [`EvacDest`](crate::copy::EvacDest) seam) rewrites a reference the moment it makes the
//! copy it points at, and `ImmixSpace::alloc_words` can refuse *mid-promotion*: a line-granular
//! request needs a run of contiguous free lines **inside one 16-line block**
//! (`immix::find_run`), while every caller's pre-check compares against the region-wide
//! `free_lines()`. Two 7-line objects fill 14 of a block's 16 lines, so a region with plenty of
//! free lines can have no run left — and a `forward_slot`-driven promotion then fails *after*
//! some objects were copied and their references rewritten: a half-forwarded heap, surfaced as
//! a spurious `Alloc(AllocFailure { kind: Heap })`.
//!
//! This module splits the promotion into two phases instead, preserving the traversal order
//! of the `forward_slot` driver exactly (roots in order, then a LIFO worklist of the reference
//! slots inside the copies; each object copied when it is first reached):
//!
//! * **Phase R — reserve, copy, mark.** Every object is reserved, copied and marked as
//!   before, but **no reference word is rewritten**: the roots and the reference slots inside
//!   the copies are merely *recorded*. A reservation that finds no run ends the phase and the
//!   whole attempt is [rolled back](rollback) — each reservation un-claimed through
//!   [`ImmixSpace::release_words`](crate::immix::ImmixSpace::release_words) and every source
//!   word the phase overwrote with a marker restored from its copy (which still holds the
//!   untouched original). The from-space and the destination are then byte-identical to their
//!   state before the attempt.
//! * **Phase C — rewrite.** Only once the *whole* reserve phase has succeeded is every
//!   recorded reference word written, which is exactly what `forward_slot`'s `Target::Forwarded`
//!   branch does for one slot.
//!
//! The outcome is therefore a decision ([`PromoteOutcome`]), never a partly-applied
//! collection: a caller that is refused can retry the same roots against a freshly built
//! region, and a caller that succeeds has a fully relocated graph.
//!
//! # Sharing with the copying engine
//!
//! Object sizing ([`move_size`](crate::copy::move_size)) and reference enumeration
//! ([`for_each_ref`](crate::copy::for_each_ref)) are the same shared `copy.rs` primitives the
//! copying collector and the mark-region reclaim use, so no shape logic lives here: the
//! *one* decision this module ownes is *which* objects move — exactly the references that
//! fall inside the [`PromoteFrom`] set.

use beam_common::Result;
use beam_term::prelude::{
    boxed_val, is_header, is_non_value, list_val, make_boxed, make_list, primary_tag, Eterm,
    PrimaryTag, THE_NON_VALUE,
};

use crate::copy::{for_each_ref, move_size};
use crate::immix::ImmixSpace;
use crate::nursery::Nursery;
use crate::space::FromSet;
use crate::stats::GcStats;
/// The from-space of a promotion: the copying nursery chain, plus — when a fullsweep is also
/// **relocating** the mature generation into a differently-built region — the old mark-region
/// mature space. A reference into neither is left verbatim.
pub(super) struct PromoteFrom<'a> {
    nursery: FromSet,
    mature: Option<&'a ImmixSpace>,
}

impl<'a> PromoteFrom<'a> {
    /// Collects the promotion's from-space: every space of `nursery`'s chain, plus `mature`
    /// when this collection abandons that region too.
    pub(super) fn new(nursery: &Nursery, mature: Option<&'a ImmixSpace>) -> PromoteFrom<'a> {
        let mut nursery_from = FromSet::new();
        nursery.add_to_from_set(&mut nursery_from);
        PromoteFrom {
            nursery: nursery_from,
            mature,
        }
    }

    /// Whether `ptr` names an object a promotion must copy, i.e. one that lives in a space
    /// this collection is abandoning.
    fn contains(&self, ptr: *const Eterm) -> bool {
        self.nursery.contains(ptr) || self.mature.is_some_and(|m| m.contains(ptr))
    }
}

/// The outcome of a promotion attempt.
pub(super) enum PromoteOutcome {
    /// Every reachable object was copied and every reference word rewritten: the statistics of
    /// the evacuation.
    Done(GcStats),
    /// The destination has no contiguous run of free lines for an object the promotion has to
    /// place — the only failure a line-granular reservation has. **Nothing happened**: no
    /// reference was rewritten and the destination carries no trace of the attempt, so the
    /// caller may retry the same roots against a differently built region.
    WouldNotFit,
}

/// The plan a promotion's reserve phase builds before it rewrites a single reference word.
#[derive(Default)]
struct PromotePlan {
    /// Reference slots **inside copies this phase made** whose objects have not been walked
    /// yet — the LIFO worklist of the traversal.
    work: Vec<*mut Eterm>,
    /// Every reference word that named a from-space object, with the value it must end up
    /// holding. Applied only once the whole reserve phase has succeeded (phase C).
    rewrites: Vec<(*mut Eterm, Eterm)>,
    /// Every copy the reserve phase made, as `(source, copy, words)`. The rollback undoes
    /// exactly these.
    copies: Vec<(*mut Eterm, *mut Eterm, usize)>,
    /// The copies this attempt made, for the promotion's [`GcStats`].
    stats: GcStats,
}

/// Whether a single reserve step copied an object, did nothing, or found no room.
enum Step {
    /// The step completed (it copied an object, followed a marker, or left a foreign
    /// reference alone).
    Done,
    /// The destination has no run for the object this step must place: the attempt is refused.
    WouldNotFit,
}

/// Evacuates every live object reachable from `roots` out of `from` into `to`, updating
/// `roots` in place, and returns the copy statistics — **atomically**.
///
/// The traversal is a Cheney worklist: copying an object queues the addresses of the reference
/// words inside its copy, and each queued word is processed once, so a shared object is copied
/// once and every later reference to it follows the marker. A reservation that finds no run
/// ends the attempt with [`PromoteOutcome::WouldNotFit`] and leaves both spaces untouched
/// (see the module documentation); anything else is a genuine error and propagates as `Err`
/// with the from-space equally untouched.
pub(super) fn promote(
    from: &PromoteFrom<'_>,
    to: &mut ImmixSpace,
    roots: &mut [Eterm],
) -> Result<PromoteOutcome> {
    let mut plan = PromotePlan::default();

    // Phase R — reserve, copy and mark. The roots in order, then the LIFO worklist, exactly
    // as the `forward_slot`-driven traversal ordered them. Nothing but the *sources* (a
    // marker) and the destination's reservations is written.
    let mut refused = false;
    for root in roots.iter_mut() {
        if matches!(
            reserve_step(from, to, root as *mut Eterm, &mut plan)?,
            Step::WouldNotFit
        ) {
            refused = true;
            break;
        }
    }
    while !refused {
        let Some(slot) = plan.work.pop() else {
            break;
        };
        if matches!(reserve_step(from, to, slot, &mut plan)?, Step::WouldNotFit) {
            refused = true;
        }
    }
    if refused {
        rollback(to, &plan.copies);
        return Ok(PromoteOutcome::WouldNotFit);
    }

    // Phase C — rewrite every recorded reference word. By now every reachable from-space
    // object has been copied, so each recorded value is a final destination address, and a
    // slot that named a foreign object was never recorded. This reproduces exactly what
    // `forward_slot`'s `Target::Forwarded` branch writes for one slot.
    for (slot, value) in plan.rewrites.iter() {
        // SAFETY: every recorded slot is a live, writable reference word — a caller root, or
        // a reference word inside a copy this promotion made — and phase R wrote nothing to
        // it (a marker goes into the *source*, never into a slot).
        unsafe { **slot = *value };
    }
    Ok(PromoteOutcome::Done(plan.stats))
}

/// One step of phase R: classify the term in `slot` and, when it names a not-yet-copied
/// from-space object, reserve its copy, queue the copy's own reference slots and write the
/// forwarding marker into the source.
fn reserve_step(
    from: &PromoteFrom<'_>,
    to: &mut ImmixSpace,
    slot: *mut Eterm,
    plan: &mut PromotePlan,
) -> Result<Step> {
    // SAFETY: the caller guarantees `slot` addresses a readable, live term word: a root word
    // of the promotion's root slice, or a reference word inside a copy this promotion made.
    let term = unsafe { *slot };
    match primary_tag(term) {
        PrimaryTag::Boxed => {
            let ptr = boxed_val(term);
            if !from.contains(ptr) {
                // A mature object, a literal, off-heap memory or any other out-of-from-space
                // address: never moved by this promotion, left byte-identical.
                return Ok(Step::Done);
            }
            // SAFETY: `ptr` is inside a live from-space object, so its header word is readable.
            if !is_header(unsafe { *ptr }) {
                // `IS_MOVED_BOXED`: the header was overwritten with the new BOXED pointer, so
                // the reference only has to follow the marker.
                // SAFETY: as above — the marker is this promotion's own destination reference.
                let marker = unsafe { *ptr };
                plan.rewrites.push((slot, marker));
                return Ok(Step::Done);
            }
            // SAFETY: `ptr` heads a live boxed from-space object, so its first word is its true
            // header and the canonical walk reads only the words that shape implies are
            // present.
            let total = move_size(unsafe { *ptr }, ptr);
            // SAFETY: `alloc_words` hands out `total` exclusively-owned, line-aligned words —
            // uninitialised, so they are written below before anything reads them.
            let dst = match unsafe { to.alloc_words(total) } {
                Ok(dst) => dst,
                // `alloc_words`'s only failure is "no run of `lines_for(total)` free lines
                // inside one block": this object cannot be placed, so the whole attempt is
                // refused and rolled back (never a half-forwarded heap).
                Err(_) => return Ok(Step::WouldNotFit),
            };
            // Queue the copy's own reference slots *before* the source's header is overwritten
            // with the marker. The copy is verbatim, so a slot at offset `o` of the source is
            // that same word of the copy.
            enqueue_children(ptr, dst, total, plan)?;
            // SAFETY: `ptr` addresses a live object of exactly `total` readable words; `dst` is
            // a distinct, non-overlapping `total`-word reservation; `ptr` is writable (a live
            // object being evacuated).
            unsafe {
                core::ptr::copy_nonoverlapping(ptr, dst, total);
                *ptr = make_boxed(dst as *const Eterm); // forwarding marker
                let marker = *ptr;
                plan.rewrites.push((slot, marker));
            }
            plan.copies.push((ptr, dst, total));
            plan.stats.objects_copied += 1;
            plan.stats.words_copied += total;
            Ok(Step::Done)
        }
        PrimaryTag::List => {
            let ptr = list_val(term);
            if !from.contains(ptr) {
                // Another space's cons cell (or a literal): left byte-identical.
                return Ok(Step::Done);
            }
            // SAFETY: `ptr` is inside a live from-space cons cell, so its car word is readable.
            if is_non_value(unsafe { *ptr }) {
                // `IS_MOVED_CONS`: the car holds `THE_NON_VALUE` and the cdr the new LIST
                // pointer, so the reference follows the cdr.
                // SAFETY: as above — the marker is this promotion's own destination reference.
                let marker = unsafe { *ptr.add(1) };
                plan.rewrites.push((slot, marker));
                return Ok(Step::Done);
            }
            // SAFETY: `alloc_words` hands out two exclusively-owned, line-aligned words —
            // uninitialised, so they are written below before anything reads them.
            let dst = match unsafe { to.alloc_words(2) } {
                Ok(dst) => dst,
                // No run for the cell: the attempt is refused and rolled back.
                Err(_) => return Ok(Step::WouldNotFit),
            };
            // Queue both reference words of the copy *before* the marker overwrites both words
            // of the source cell (a cons cell carries no header, so its two payload words are
            // its only state).
            enqueue_children(ptr, dst, 2, plan)?;
            // SAFETY: `ptr` addresses a live 2-word cons cell; `dst` is a distinct,
            // non-overlapping 2-word reservation; both source words are writable.
            unsafe {
                *dst = *ptr;
                *dst.add(1) = *ptr.add(1);
                *ptr = THE_NON_VALUE; // forwarding indicator
                *ptr.add(1) = make_list(dst as *const Eterm); // forwarding address
                let marker = *ptr.add(1);
                plan.rewrites.push((slot, marker));
            }
            plan.copies.push((ptr, dst, 2));
            plan.stats.objects_copied += 1;
            plan.stats.words_copied += 2;
            Ok(Step::Done)
        }
        // A bare header (already consumed by the scan) or an immediate is not a reference:
        // nothing to copy and nothing to rewrite.
        PrimaryTag::Header | PrimaryTag::Immed1 => Ok(Step::Done),
    }
}

/// Queues the address of every reference word of the copy at `dst` — a verbatim copy of the
/// `total`-word object at `src` — for the rest of phase R.
///
/// The slots are read from the **source**, before the marker overwrites it, and mapped onto
/// the copy's own offsets; the shared [`for_each_ref`] decides which words are references
/// (exactly the set the `forward_slot` driver queued for a copy), so a fun's inlined
/// descriptor is skipped opaquely, a map's size/occupancy words are handled, and an
/// `ErlSubBits`'s `orig` word is visited.
fn enqueue_children(
    src: *const Eterm,
    dst: *mut Eterm,
    total: usize,
    plan: &mut PromotePlan,
) -> Result<()> {
    // SAFETY: the caller guarantees `src` addresses a live object of exactly `total` readable
    // words (`2` for a cons cell) and `dst` a distinct reservation of the same size, so every
    // offset the walk reports lies inside both.
    unsafe {
        for_each_ref(src as *mut Eterm, total, &mut |child| {
            let offset = child.offset_from(src as *mut Eterm) as usize;
            plan.work.push(dst.add(offset));
            Ok(())
        })
    }
}

/// Undoes phase R of a **refused** promotion: every reservation is un-claimed and every source
/// word the phase overwrote with a forwarding marker is restored from its copy.
///
/// A copy still holds its object's untouched original words — phase C, the only writer of
/// reference words, has not run — so this restores the from-space exactly; the source's
/// reserved lines are then released through
/// [`ImmixSpace::release_words`](crate::immix::ImmixSpace::release_words), which restores the
/// destination's `allocated` bitmap and block states. Neither space carries a trace of the
/// attempt afterwards.
fn rollback(to: &mut ImmixSpace, copies: &[(*mut Eterm, *mut Eterm, usize)]) {
    for &(src, dst, total) in copies.iter().rev() {
        // SAFETY: `dst` is the verbatim copy this phase made of the live `total`-word object at
        // `src`, and nothing has written to `dst` since (phase C has not run), so copying it
        // back restores every word the marker overwrote — the header word of a boxed object,
        // both payload words of a cons cell.
        unsafe { core::ptr::copy_nonoverlapping(dst, src, total) };
        to.release_words(dst, total);
    }
}
