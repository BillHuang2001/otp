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

//! **Region-generic relocating compaction** — [`Relocator`], [`compact`] and the turnkey
//! [`LinearDest`] — so a host holding a *foreign* word region can evacuate it.
//!
//! Everything a copy needs already exists and is heap-agnostic: the [`FromSet`] is a set of
//! explicit byte ranges (§[`FromSet::from_ranges`]), object sizing and per-object child
//! enumeration are [`beam_term::layout`]'s, and the forwarding-marker protocol is
//! [`forward_slot`](crate::copy::forward_slot) over the [`EvacDest`] seam. This module is the
//! **driver** that composes them for a caller whose region is not a process heap: the
//! collector's own [`Copier`](crate::Copier) owns a [`HeapSpace`](crate::HeapSpace) and is
//! driven by the collection cadence, while a [`Relocator`] owns nothing — it borrows the
//! from-set and the destination, so a `beam_alloc::ReservedHeap`, a pre-reserved
//! `Vec<Eterm>`, an mmap'd arena or the engine's term arena can be compacted by it.
//!
//! [`LinearDest`] is the destination such a host usually wants: one flat, word-aligned run it
//! already reserved, with the classification the [`Copier`](crate::Copier) uses (so a host does
//! not have to reimplement the marker protocol) and a bump pointer.
//!
//! # The two sound patterns, and why the order matters
//!
//! ```text
//! // (a) ONE root slice — atomic:
//! let report = compact(&from, &mut roots, &mut dest)?;      // refuses before mutating
//!
//! // (b) MANY root slices — preflight, then forward:
//! let need = slices.iter().map(|s| live_words_in(&from, s)).sum();
//! let mut reloc = Relocator::new_checked(&from, &mut dest, need)?;
//! for slice in slices { reloc.forward_roots(slice)?; }
//! let report = reloc.complete()?;
//! ```
//!
//! A relocation is **destructive and not recoverable once it starts**: [`forward_roots`] and
//! [`complete`] write forwarding markers into the source objects and rewrite the roots they are
//! given, so a `heap_exhausted` raised *from* either of them leaves the source **partially
//! forwarded** and the destination holding the copies made so far — the caller must not read
//! the source afterwards. Every failure that can be decided up front is therefore decided up
//! front: [`Relocator::new`]/[`Relocator::new_checked`] mutate nothing, and they are how a
//! caller makes a multi-slice compaction atomic (pattern (b)). [`compact`] does the same for the
//! single-slice case by sizing the destination from an exact live walk
//! ([`live_words_in`]) before the first word moves.
//!
//! # Root-slice idempotence
//!
//! [`Relocator::forward_roots`] may be called **many times**, once per root slice of a host
//! whose roots are scattered (x registers, a stack, a stack of saved arguments, an
//! introspection table). A slot it already rewrote names a destination object, which the
//! destination classifies [`Target::Foreign`] (the destination region is disjoint from every
//! evacuated range), so re-visiting a slice — or a slice overlapping one already forwarded — is
//! a no-op: the result equals one call over the union of the slices. An immediate, a bare header
//! and a reference outside every from-range are left byte-identical. This is precisely the
//! [`Copier`](crate::Copier)'s root rule; the marker protocol is what makes it idempotent.
//!
//! # What a relocation is *not*
//!
//! It is not a collection of a process heap: [`Relocation::stats`]'s
//! `minor_collections`/`major_collections` stay `0`, no heap is sized or resized, and the
//! from-set's ranges are **not** freed — releasing the source region is the host's, which is
//! why the safety contract of [`FromSet::from_ranges`] asks for a region the host keeps valid
//! for the whole relocation and frees itself afterwards.
//!
//! `unsafe` is confined to this module's word-level driver (the Cheney destination scan reads
//! words it knows were copied there) and to [`LinearDest`]'s reservation; each block carries a
//! `// SAFETY:` rationale.

use core::mem::size_of;

use beam_common::{BeamError, Result};
use beam_term::prelude::{
    for_each_child_slot, is_header, is_non_value, primary_tag, Eterm, PrimaryTag,
};

use crate::copy::{forward_accounted, move_size, EvacDest, Target};
use crate::error::heap_exhausted;
use crate::live::live;
use crate::space::FromSet;
use crate::stats::GcStats;

// ---------------------------------------------------------------------------
// The turnkey destination
// ---------------------------------------------------------------------------

/// A destination over a flat, word-aligned region the **host** owns: copies land contiguously
/// from `base`, in reservation order, and `used_words` is the region's new live length.
///
/// The classification is exactly the copying collector's ([`Copier`](crate::Copier)): a
/// reference outside every from-range is [`Target::Foreign`]; a BOXED word inside one is
/// [`Target::Live`] while its object still heads a real header word and [`Target::Forwarded`]
/// once that word has become the marker; a LIST word inside one is [`Target::Forwarded`] once
/// its car holds `THE_NON_VALUE` and [`Target::Live`] otherwise. A host therefore never writes
/// the marker protocol itself — it reserves the region, hands it over and calls
/// [`Relocator`]/[`compact`].
///
/// The destination is **append-only**: a relocation never reuses, compacts or rewinds the words
/// it already handed out, so `base` stays valid and the copies keep their addresses for as long
/// as the host keeps the region.
pub struct LinearDest<'a> {
    /// The evacuated ranges — the containment test that tells a from-space object (copy it)
    /// from a destination object (leave it).
    from: &'a FromSet,
    /// The first word of the host's region.
    base: *mut Eterm,
    /// The region's size in term words (the ceiling `reserve` refuses past).
    capacity_words: usize,
    /// The bump pointer offset: how many words are handed out (the region's live length).
    used: usize,
}

impl<'a> LinearDest<'a> {
    /// A destination over the `capacity_words` words at `base`.
    ///
    /// # Safety
    ///
    /// * `base` must address `capacity_words` word-aligned, writable, **exclusively owned**
    ///   words (a larger `capacity_words` than the allocation really has is an out-of-bounds
    ///   write as soon as a copy needs the words).
    /// * Those words must stay valid and must **not move** until the relocator that borrows this
    ///   destination finishes — the returned copies are addressed by the roots the relocator
    ///   rewrites.
    /// * The region must not overlap any range of `from`: a copier writes a forwarding marker
    ///   into the source object, so an aliasing destination would overwrite the copy it had just
    ///   made (the overlap is asserted in debug builds and is the caller's contract in release
    ///   builds).
    /// * `capacity_words == 0` is allowed and copies nothing.
    pub unsafe fn new(from: &'a FromSet, base: *mut Eterm, capacity_words: usize) -> LinearDest<'a> {
        let dest = LinearDest {
            from,
            base,
            capacity_words,
            used: 0,
        };
        // Debug-only: the disjointness the safety contract promises. `align_of` is checked too
        // because `reserve` hands out words by pointer arithmetic from `base`.
        debug_assert!(
            base as usize % core::mem::align_of::<Eterm>() == 0,
            "a destination base must be word-aligned, got {base:p}"
        );
        if cfg!(debug_assertions) {
            let lo = base as usize;
            let hi = lo.saturating_add(capacity_words.saturating_mul(size_of::<Eterm>()));
            for &(from_lo, from_hi) in from.ranges() {
                debug_assert!(
                    from_hi <= lo || from_lo >= hi,
                    "a destination must not overlap the evacuated range \
                     [{from_lo:#x}, {from_hi:#x})"
                );
            }
        }
        dest
    }

    /// The first word of the host's region.
    pub fn base(&self) -> *mut Eterm {
        self.base
    }

    /// The region's size in term words.
    pub fn capacity_words(&self) -> usize {
        self.capacity_words
    }

    /// The words handed out so far — for a compaction from an empty region, exactly the new live
    /// length of the host's words.
    pub fn used_words(&self) -> usize {
        self.used
    }

    /// The words still free (`capacity_words - used_words`).
    pub fn available_words(&self) -> usize {
        self.capacity_words.saturating_sub(self.used)
    }
}

impl EvacDest for LinearDest<'_> {
    unsafe fn classify_boxed(&self, ptr: *const Eterm) -> Target {
        if !self.from.contains(ptr) {
            // A literal, another region, an off-heap pointer — or one of this destination's own
            // copies, which is what makes a repeated root slice idempotent.
            return Target::Foreign;
        }
        // SAFETY: `ptr` is inside a from-range, whose words the caller declared readable
        // (`FromSet::from_ranges`), so its header word is readable.
        if is_header(unsafe { *ptr }) {
            Target::Live
        } else {
            // `IS_MOVED_BOXED`: the header was overwritten with the new BOXED pointer.
            Target::Forwarded
        }
    }

    unsafe fn classify_cons(&self, ptr: *const Eterm) -> Target {
        if !self.from.contains(ptr) {
            return Target::Foreign;
        }
        // SAFETY: `ptr` is inside a from-range, so its car word is readable.
        if is_non_value(unsafe { *ptr }) {
            Target::Forwarded
        } else {
            Target::Live
        }
    }

    fn can_reserve(&self, words: usize) -> bool {
        self.available_words() >= words
    }

    unsafe fn reserve(&mut self, words: usize) -> Result<*mut Eterm> {
        if words > self.available_words() {
            // The pre-check `forward_slot` performs, repeated here so a caller that bypasses it
            // gets the same typed refusal rather than a write past the region.
            return Err(heap_exhausted(words));
        }
        // The reserved words are `[base + used, base + used + words)`. They are inside the
        // caller's `capacity_words`-word region (`words <= available_words`), so the pointer
        // arithmetic stays in bounds; they are handed out once (the offset only advances), so
        // two reservations never alias.
        let at = self.base.wrapping_add(self.used);
        self.used += words;
        Ok(at)
    }

    fn copied_top(&self) -> Option<*mut Eterm> {
        // The copied run is `[base + used_at_construction, base + used)`: exactly the reserved
        // words, in reservation order.
        Some(self.base.wrapping_add(self.used))
    }
}

// ---------------------------------------------------------------------------
// The driver
// ---------------------------------------------------------------------------

/// Drives a relocation of one [`FromSet`] into one [`EvacDest`]: [`forward_roots`] per root
/// slice, then [`complete`] to walk the copies and forward their children.
///
/// The mechanism is the Cheney copy of §[`Copier`](crate::Copier) with its *to-space* replaced
/// by any destination that publishes a contiguous copied run
/// ([`EvacDest::copied_top`]): the scan starts where that run started and stops where it now
/// ends, and the copies it walks are exactly the ones the roots reached. Only the from-set and
/// the destination are borrowed, so no heap is owned and no collection cadence is implied.
///
/// # Failure semantics
///
/// * [`new`](Relocator::new) fails with [`BeamError::BadArg`] — before mutating anything — when
///   the destination does not publish a contiguous copied run
///   ([`copied_top`](EvacDest::copied_top) is `None`); such a destination has its own driver.
/// * [`new_checked`](Relocator::new_checked) additionally refuses with the typed
///   `heap_exhausted` (`BeamError::Alloc`/`AllocKind::Heap`) when the destination cannot reserve
///   `required_words`, again **before anything is mutated**.
/// * A refusal raised from [`forward_roots`](Relocator::forward_roots) or
///   [`complete`](Relocator::complete) (a `reserve` that does not fit) leaves the source
///   **partially forwarded** — some header words hold forwarding markers, some roots are
///   rewritten — and the destination holding the copies made so far. That state is **not
///   recoverable**: the caller must not read the source afterwards. Use
///   [`new_checked`](Relocator::new_checked) (or [`compact`]) to make the failure decision up
///   front.
///
/// # Root-slice idempotence
///
/// [`forward_roots`](Relocator::forward_roots) is idempotent and may be called once per root
/// slice: a slot it already rewrote names a destination object, which the destination
/// classifies [`Target::Foreign`], so a second pass over the same slice copies nothing. See the
/// module docs for the multi-slice recipe.
pub struct Relocator<'a> {
    /// The evacuated ranges.
    from: &'a FromSet,
    /// The destination the copies go to (and whose copied run the scan walks).
    dest: &'a mut dyn EvacDest,
    /// The Cheney scan position: the next copied word to inspect.
    scan: *mut Eterm,
    /// The destination's copied-run end at construction — the scan's origin, and the baseline
    /// [`dest_words_used`](Relocator::dest_words_used) measures this compaction's copies from.
    origin: *mut Eterm,
    /// What moved so far.
    stats: GcStats,
}

impl<'a> Relocator<'a> {
    /// Starts a relocation of `from` into `dest`, recording the destination's copied-run end as
    /// the scan origin.
    ///
    /// Returns [`BeamError::BadArg`] when `dest.copied_top()` is `None` — a destination whose
    /// copies do not form one contiguous run cannot be walked by a Cheney scan, so it needs its
    /// own driver (the mark-region space's block-structured promotion). Nothing is mutated in
    /// either outcome.
    pub fn new(from: &'a FromSet, dest: &'a mut dyn EvacDest) -> Result<Relocator<'a>> {
        let origin = dest.copied_top().ok_or(BeamError::BadArg)?;
        Ok(Relocator {
            from,
            dest,
            scan: origin,
            origin,
            stats: GcStats::default(),
        })
    }

    /// As [`new`](Relocator::new), but also refuses — **before anything is mutated** — when the
    /// destination cannot reserve `required_words` (a [`heap_exhausted`], i.e.
    /// `BeamError::Alloc`/`AllocKind::Heap`).
    ///
    /// This is the preflight that makes a **multi-root-slice** compaction atomic: size the
    /// destination first with [`live_words_in`](crate::live_words_in) summed over every root
    /// slice, then forward the slices one at a time. Pass the exact copy volume — an
    /// over-estimate only causes a conservative refusal, an under-estimate does not (it lets
    /// [`forward_roots`](Relocator::forward_roots) fail mid-way).
    pub fn new_checked(
        from: &'a FromSet,
        dest: &'a mut dyn EvacDest,
        required_words: usize,
    ) -> Result<Relocator<'a>> {
        // The destination shape is checked first: a `BadArg` is a programming error, an
        // exhaustion is a sizing one, and reporting the wrong one would send the caller looking
        // in the wrong place.
        let origin = dest.copied_top().ok_or(BeamError::BadArg)?;
        if !dest.can_reserve(required_words) {
            return Err(heap_exhausted(required_words));
        }
        Ok(Relocator {
            from,
            dest,
            scan: origin,
            origin,
            stats: GcStats::default(),
        })
    }

    /// Forwards each live root word in `roots` in place — idempotently, so it can be called once
    /// per root slice (module docs).
    ///
    /// # Failure semantics
    ///
    /// A destination that cannot hold an object is the typed `heap_exhausted`, raised **after**
    /// earlier roots of this slice (and every earlier slice) were already forwarded: the source
    /// is left partially forwarded and must not be read again.
    pub fn forward_roots(&mut self, roots: &mut [Eterm]) -> Result<()> {
        for root in roots.iter_mut() {
            // SAFETY: `root` is a live, exclusively-borrowed term word; the destination's
            // classification only dereferences pointers inside `from`.
            unsafe { forward_accounted(&mut *self.dest, root as *mut Eterm, &mut self.stats)? };
        }
        Ok(())
    }

    /// The words copied so far (the source words now living in the destination).
    pub fn words_copied(&self) -> usize {
        self.stats.words_copied
    }

    /// The objects copied so far (boxed objects and cons cells).
    pub fn objects_copied(&self) -> usize {
        self.stats.objects_copied
    }

    /// The destination words this relocation reserved so far — the copied run's growth, which
    /// for a linear destination equals [`words_copied`](Relocator::words_copied) (and is the
    /// host's new used length when it started from an empty region).
    pub fn dest_words_used(&self) -> usize {
        match self.dest.copied_top() {
            // `copied_top` advanced monotonically since construction (the destination contract),
            // so the difference is the reserved word count.
            Some(top) => (top as usize).saturating_sub(self.origin as usize) / size_of::<Eterm>(),
            None => 0,
        }
    }

    /// Runs the Cheney destination scan to completion and reports what moved.
    ///
    /// The loop is [`Copier::complete`](crate::Copier)'s exactly, over the destination's
    /// contiguous copied run instead of a `HeapSpace` to-space: for each copied word, a
    /// reference word is forwarded (one word), a header word is sized by `move_size` and its
    /// canonical child slots (`beam_term::layout::for_each_child_slot`) are forwarded before the
    /// scan jumps past the object, and an immediate is stepped over. New copies land at the
    /// destination's end, i.e. *ahead* of the scan, so the loop terminates exactly when the scan
    /// reaches the last copy.
    ///
    /// # Failure semantics
    ///
    /// As [`forward_roots`](Relocator::forward_roots): a destination that cannot hold a copied
    /// child is the typed `heap_exhausted`, raised with the source partially forwarded.
    pub fn complete(mut self) -> Result<Relocation> {
        loop {
            // The destination contract says the value only advances, so re-reading it per
            // iteration is what makes the bound "the copies made so far" rather than a snapshot.
            let top = self.dest.copied_top().ok_or(BeamError::BadArg)?;
            if self.scan >= top {
                break;
            }
            // SAFETY: `scan < top`, i.e. it addresses a word a copy already wrote into the
            // destination's contiguous run.
            let gval = unsafe { *self.scan };
            match primary_tag(gval) {
                PrimaryTag::Boxed | PrimaryTag::List => {
                    // SAFETY: `scan` addresses a live reference word of the destination run;
                    // forwarding it copies the referenced object (or follows an already-moved
                    // marker) and rewrites the word in place.
                    unsafe {
                        forward_accounted(&mut *self.dest, self.scan, &mut self.stats)?;
                    }
                    // SAFETY: the reference word is one word, so `add(1)` stays within the run
                    // (or lands exactly on its end).
                    self.scan = unsafe { self.scan.add(1) };
                }
                PrimaryTag::Header => {
                    let scan = self.scan;
                    let total = move_size(gval, scan);
                    // `for_each_child_slot`'s callback is infallible; the accumulator carries
                    // the first forwarding failure out of the walk.
                    let mut err: Result<()> = Ok(());
                    // The destination and the counters are split out of `self` as two disjoint
                    // borrows so the closure can drive the shared accounting routine.
                    let dest: &mut dyn EvacDest = &mut *self.dest;
                    let stats: &mut GcStats = &mut self.stats;
                    // SAFETY: `scan` addresses a copied object header of exactly `total` words
                    // (the object was copied verbatim), so every child slot the canonical walk
                    // reports lies inside it and holds a live term reference.
                    unsafe {
                        for_each_child_slot(gval, scan, |offset| {
                            if err.is_ok() {
                                err = forward_accounted(dest, scan.add(offset), stats);
                            }
                        });
                    }
                    err?;
                    // SAFETY: `total` is exactly the object's word count, all of it inside the
                    // destination run, so the advanced position stays within the run (or lands
                    // exactly on its end).
                    self.scan = unsafe { scan.add(total) };
                }
                PrimaryTag::Immed1 => {
                    // SAFETY: an immediate occupies one word.
                    self.scan = unsafe { self.scan.add(1) };
                }
            }
        }

        let dest_words_used = {
            let top = self.dest.copied_top().ok_or(BeamError::BadArg)?;
            (top as usize).saturating_sub(self.origin as usize) / size_of::<Eterm>()
        };
        let mut stats = self.stats;
        // The from-space words no copy kept. A reference outside every range was never part of
        // `from.words()`, so it does not disturb this figure.
        stats.words_reclaimed = self.from.words().saturating_sub(stats.words_copied);
        Ok(Relocation {
            stats,
            dest_words_used,
        })
    }
}

/// What a completed relocation moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Relocation {
    /// The copy counters: `words_copied`/`objects_copied` describe this compaction,
    /// `words_reclaimed` is `from.words() - words_copied` (the source words no live term named),
    /// and the two collection counters stay `0` — a relocation is **not** a collection of a
    /// process heap, so it advances no `minor_gcs`/`fullsweep` cadence.
    pub stats: GcStats,
    /// The destination words this relocation reserved (the copied run's growth). For a linear
    /// destination this equals [`Relocation::stats`]'s `words_copied` and is the host's new used
    /// length; document your destination's reading against
    /// [`EvacDest::copied_top`] if it is not linear.
    pub dest_words_used: usize,
}

/// One-shot **atomic** compaction of a single root slice: exact-live-preflighted, then copied.
///
/// The destination is sized from a read-only live walk of the same from-set and roots
/// ([`live_words_in`](crate::live_words_in)) *before* the first word moves, so a destination too
/// small is the typed `heap_exhausted` with `from`'s ranges and `roots` byte-identical — no
/// forwarding marker written, no root rewritten, the destination untouched. Once the preflight
/// passes, the roots are forwarded ([`Relocator::forward_roots`]) and the destination scan runs
/// to completion ([`Relocator::complete`]).
///
/// The atomicity stops at the preflight: a destination whose `copied_top` is `None` is
/// [`BeamError::BadArg`] (also before mutating), and any *unexpected* mid-copy refusal (a
/// destination whose `can_reserve` lie was caught only at `reserve`) would leave the source
/// partially forwarded. For several root slices, use the preflighted multi-slice recipe in the
/// module docs instead: one exact total, then one `forward_roots` per slice.
pub fn compact(from: &FromSet, roots: &mut [Eterm], dest: &mut dyn EvacDest) -> Result<Relocation> {
    let required = live(from, roots).words;
    let mut relocator = Relocator::new_checked(from, dest, required)?;
    relocator.forward_roots(roots)?;
    relocator.complete()
}

#[cfg(test)]
mod tests;
