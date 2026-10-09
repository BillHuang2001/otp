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

//! The Cheney copy engine — [`Copier`], the [`collect`] entry point — and the **shared
//! word-level forwarding primitives** that the Immix mark-region mature space
//! ([`crate::immix`]) reuses.
//!
//! `unsafe` is confined to this module's word-level code, which mirrors the legacy C
//! `move_boxed`/`move_cons`/`sweep` (`erts/emulator/beam/erl_gc.c`): in-place forwarding
//! plus a Cheney scan of the freshly-copied to-space. The per-object layout itself — how
//! many words one object occupies and which of its payload words are references — is the
//! canonical walk of [`beam_term::layout`] (the workspace's single owner of the per-object
//! layout): [`move_size`] is [`beam_term::layout::object_words`] plus the copy-specific
//! `ErlSubBits` override, and [`for_each_ref`] / [`Copier::complete`] drive
//! [`beam_term::layout::for_each_child_slot`]. This module adds only those two
//! copy-specific pieces and the reference-forwarding switch ([`forward_slot`], over the
//! [`EvacDest`] seam), all `pub(crate)` so the two evacuating spaces share one
//! implementation instead of each carrying its own copy of the marker scheme.

use beam_common::Result;
use beam_term::prelude::{
    boxed_val, for_each_child_slot, is_header, is_non_value, list_val, make_boxed, make_list,
    object_words, primary_tag, Eterm, PrimaryTag, ERL_SUB_BITS_SIZE, HEADER_SUB_BITS,
    THE_NON_VALUE,
};

use crate::error::heap_exhausted;
use crate::space::{FromSet, HeapSpace};
use crate::stats::GcStats;

// ---------------------------------------------------------------------------
// Shared word-level forwarding primitives (used by this module and `immix.rs`)
// ---------------------------------------------------------------------------

/// The total words (header word + payload) the boxed object at `ptr` occupies in the
/// **copy** graph, whose header word is `hdr` — the `nelts + 1` of the C `move_boxed`.
///
/// Every shape follows the canonical per-object walk [`beam_term::layout::object_words`]
/// **except** an `ErlSubBits`. `object_words` reports the object's contribution to the
/// *sizing* graph — for a refc binary the whole `[BinRef(3)][ErlSubBits(5)]` pair (`8`),
/// and for a match context a fresh heap-bits window — but a *copier* must not lay down
/// that many words: it copies the `ErlSubBits` block's own [`ERL_SUB_BITS_SIZE`] (`5`)
/// words and forwards its `orig` word, which copies the 3-word `BinRef` as its own child
/// (the C `move_boxed`/`sweep` `SUB_BITS` protocol, `erl_gc.c:2332-2347`;
/// `copy.c:1677-1700`). The pair is therefore copied as two separate allocations, exactly
/// as [`Copier::complete`] and [`for_each_ref`] expect.
///
/// # Preconditions
///
/// `ptr` must point at a live, boxed object whose first word is `hdr`; it is dereferenced
/// by `object_words` only for the words `hdr`'s shape implies are present (a flatmap's
/// size word, an `ErlSubBits`'s `start`/`end`/`orig`).
pub(crate) fn move_size(hdr: Eterm, ptr: *const Eterm) -> usize {
    if hdr == HEADER_SUB_BITS {
        // The copy size of an `ErlSubBits` is its own block, never `object_words`'s graph
        // size (which folds in the refc pair's separate `BinRef`).
        ERL_SUB_BITS_SIZE
    } else {
        // SAFETY: the caller guarantees `ptr` heads a live object of `hdr`'s shape, so the
        // canonical walk reads only the words that shape implies are present.
        unsafe { object_words(hdr, ptr) }
    }
}

/// Calls `visit` with the address of every *reference word* of the object at `ptr`, which
/// occupies `total` words (`total` = the [`move_size`] of its header, or `2` for a cons
/// cell).
///
/// For a **boxed object** this is the per-object form of the region walk
/// [`Copier::complete`] performs over its contiguous to-space region: both drive the
/// canonical child-slot enumeration [`beam_term::layout::for_each_child_slot`] (the
/// workspace's single owner of the per-object layout), so a fun's inlined descriptor is
/// skipped opaquely, a transparent arityval/record is stepped over, a map's occupancy/size
/// words are handled, and an `ErlSubBits`'s `orig` word is visited. They are not merged
/// into one routine because `complete` walks a *region* that grows as it copies (the
/// Cheney invariant `while (n_hp != n_htop)`), not an object of known size; sharing the
/// loop would thread a moving region bound through the copier's hot path for no gain.
///
/// A **cons cell** (`[CAR, CDR]`) has no header word, so it is not part of the
/// header-based canonical walk (`beam_term::layout`: "every caller handles it directly");
/// its two payload words are both references and are yielded here directly.
///
/// It yields **only child slots that hold a `Boxed`/`List` word** — exactly the set
/// [`forward_slot`] can move — plus an `ErlSubBits`'s `orig` unconditionally, so a caller
/// may enqueue a yielded slot without re-testing it. The mark-region space
/// ([`crate::immix`]) uses this to enumerate objects it marks and objects it copies.
///
/// # Safety
///
/// `ptr` must address a live object of exactly `total` words (`2` for a cons cell), all of
/// which are readable, and `visit` must not invalidate them.
pub(crate) unsafe fn for_each_ref<F>(ptr: *mut Eterm, total: usize, visit: &mut F) -> Result<()>
where
    F: FnMut(*mut Eterm) -> Result<()>,
{
    // SAFETY: the caller guarantees `ptr` heads a live object of `total` words, so its
    // first word is readable.
    let first = unsafe { *ptr };
    // [`for_each_child_slot`]'s callback is infallible while `visit` is fallible; the
    // accumulator carries the first failure out of the walk without forking it.
    let mut err: Result<()> = Ok(());

    if is_header(first) {
        // A boxed object: word 0 is its header, which decides the payload layout. An
        // `ErlSubBits`'s `orig` slot is a reference the canonical copy walk exposes even
        // when it does not itself hold a `Boxed`/`List` word (it names the object's
        // underlying bitstring); every other child slot is yielded only when it holds a
        // movable word.
        let is_sub_bits = first == HEADER_SUB_BITS;
        // SAFETY: `ptr` heads a live object of `first`'s shape (the caller's guarantee), so
        // the canonical walk reads only words that shape implies are present.
        unsafe {
            for_each_child_slot(first, ptr, |offset| {
                if err.is_err() || offset >= total {
                    return;
                }
                // SAFETY: `offset < total`, so this word is inside the live object.
                let slot = ptr.add(offset);
                if is_sub_bits || matches!(primary_tag(*slot), PrimaryTag::Boxed | PrimaryTag::List)
                {
                    err = visit(slot);
                }
            });
        }
    } else {
        // A cons cell (`[CAR, CDR]`): both payload words are references. Word 0 of a boxed
        // object *is* its header (`is_header` is the frozen "not a value" pattern), so its
        // primary tag tells the two apart — the same test `scan_young` uses.
        // SAFETY: the caller guarantees a live cons cell of `total` words, so both words are
        // readable.
        unsafe {
            for offset in 0..total.min(2) {
                if err.is_err() {
                    break;
                }
                // SAFETY: `offset < total`, so this word is inside the live cell.
                let slot = ptr.add(offset);
                if matches!(primary_tag(*slot), PrimaryTag::Boxed | PrimaryTag::List) {
                    err = visit(slot);
                }
            }
        }
    }

    err
}

/// How the object a BOXED/LIST reference points at relates to an evacuation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Target {
    /// Outside the evacuated set: leave the reference exactly as it is (a literal,
    /// another space, an off-heap pointer, a pinned object, or an object already in its
    /// final position).
    Foreign,
    /// Inside the set and already moved: the object's first word is a forwarding marker
    /// to follow.
    Forwarded,
    /// Inside the set and not moved yet: the object must be copied now.
    Live,
}

/// The destination of an evacuation — *which* objects move is the only decision that
/// differs between the copying collector ([`Copier`]) and the Immix mark-region space
/// ([`crate::immix::ImmixSpace`]); the word-level protocol (object sizing, the in-place
/// marker scheme, `copy_nonoverlapping`) is shared through [`forward_slot`].
pub(crate) trait EvacDest {
    /// Classifies the object named by the BOXED reference `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` is the pointer word of a BOXED reference and may address anything; an
    /// implementation must only dereference it after establishing that it addresses
    /// memory the destination owns.
    unsafe fn classify_boxed(&self, ptr: *const Eterm) -> Target;

    /// Classifies the cons cell named by the LIST reference `ptr`.
    ///
    /// # Safety
    ///
    /// As [`classify_boxed`](EvacDest::classify_boxed).
    unsafe fn classify_cons(&self, ptr: *const Eterm) -> Target;

    /// Whether `words` writable destination words can still be reserved.
    fn can_reserve(&self, words: usize) -> bool;

    /// Reserves `words` writable, exclusively-owned destination words.
    ///
    /// # Safety
    ///
    /// `words` must satisfy [`can_reserve`](EvacDest::can_reserve). The returned words
    /// may be uninitialised and must be written before they are read.
    unsafe fn reserve(&mut self, words: usize) -> Result<*mut Eterm>;
}

/// Forwards the term word at `slot` in place, returning the number of words copied
/// (`0` when nothing was moved).
///
/// Mirrors the C `sweep()` switch body: a BOXED or LIST word naming an evacuated object
/// is copied into the destination and the reference is rewritten to the copy, while the
/// source object's header word (or, for a cons cell, its car) is left holding the
/// forwarding marker exactly as in C. An already-moved reference follows the marker; an
/// immediate, a bare header, and anything the destination classifies as
/// [`Target::Foreign`] are left untouched.
///
/// # Safety
///
/// `slot` must address a live, writable term word. If it holds a BOXED or LIST
/// reference, the referenced object must either be a live object of the destination's
/// evacuated set or some other valid address that the destination classifies as
/// [`Target::Foreign`].
pub(crate) unsafe fn forward_slot<D: EvacDest + ?Sized>(
    dest: &mut D,
    slot: *mut Eterm,
) -> Result<usize> {
    // SAFETY: the caller guarantees `slot` addresses a live term word.
    let term = unsafe { *slot };
    match primary_tag(term) {
        PrimaryTag::Boxed => {
            let ptr = boxed_val(term);
            // SAFETY: `ptr` is the pointer word of a BOXED reference; classification must
            // dereference it only when it addresses memory the destination owns.
            match unsafe { dest.classify_boxed(ptr) } {
                Target::Foreign => Ok(0),
                Target::Forwarded => {
                    // `IS_MOVED_BOXED`: the header was overwritten with the new BOXED
                    // pointer, so follow the marker rather than copying again.
                    // SAFETY: classification established that `ptr` is a moved object of
                    // the destination, hence readable; `slot` is writable by the caller.
                    unsafe { *slot = *ptr };
                    Ok(0)
                }
                Target::Live => {
                    // SAFETY: classification read the live object's header word.
                    let hdr = unsafe { *ptr };
                    let total = move_size(hdr, ptr);
                    if !dest.can_reserve(total) {
                        // Pre-check so the failure is attributable (and `heap_exhausted`
                        // is exercised); `reserve` re-checks and propagates the same
                        // failure below.
                        return Err(heap_exhausted(total));
                    }
                    // SAFETY: `total` words were just confirmed available, so this
                    // returns `total` exclusively-owned, writable destination words.
                    let dst = unsafe { dest.reserve(total)? };
                    // SAFETY: `ptr` is a live object of exactly `total` words; `dst` is a
                    // distinct, non-overlapping `total`-word region (source and
                    // destination are separate areas); `ptr` is writable (an object being
                    // evacuated) and `slot` is writable by the caller.
                    unsafe {
                        core::ptr::copy_nonoverlapping(ptr, dst, total);
                        *ptr = make_boxed(dst as *const Eterm); // forwarding marker
                        *slot = *ptr; // follow the marker: the reference now points at `dst`
                    }
                    Ok(total)
                }
            }
        }
        PrimaryTag::List => {
            let ptr = list_val(term);
            // SAFETY: `ptr` is the pointer word of a LIST reference; classification must
            // dereference it only when it addresses memory the destination owns.
            match unsafe { dest.classify_cons(ptr) } {
                Target::Foreign => Ok(0),
                Target::Forwarded => {
                    // `IS_MOVED_CONS`: the car holds `THE_NON_VALUE` and the cdr the new
                    // LIST pointer, so follow the marker.
                    // SAFETY: classification established that `ptr` is a moved cons cell
                    // of the destination, hence readable and 2 words long.
                    unsafe { *slot = *ptr.add(1) };
                    Ok(0)
                }
                Target::Live => {
                    if !dest.can_reserve(2) {
                        return Err(heap_exhausted(2));
                    }
                    // SAFETY: a cons cell is 2 words, just confirmed available.
                    let dst = unsafe { dest.reserve(2)? };
                    // SAFETY: `ptr` is a live 2-word cell; `dst` is a distinct,
                    // non-overlapping 2-word region; `ptr` is writable (a cell being
                    // evacuated) and `slot` is writable by the caller.
                    unsafe {
                        *dst = *ptr;
                        *dst.add(1) = *ptr.add(1);
                        *ptr = THE_NON_VALUE; // forwarding indicator
                        *ptr.add(1) = make_list(dst as *const Eterm); // forwarding address
                        *slot = *ptr.add(1);
                    }
                    Ok(2)
                }
            }
        }
        // A bare header (already consumed by the scan) or an immediate is not a
        // reference: nothing to forward.
        PrimaryTag::Header | PrimaryTag::Immed1 => Ok(0),
    }
}

// ---------------------------------------------------------------------------
// The copying collector
// ---------------------------------------------------------------------------

/// The Cheney copier: evacuates live terms from a [`FromSet`] into a [`HeapSpace`].
///
/// A boxed object's header word is overwritten with the new BOXED-tagged pointer and
/// a cons cell's car/cdr with the forwarding marker, exactly as in C; the `scan`
/// pointer walks the to-space to completion.
pub struct Copier<'a> {
    from: &'a FromSet,
    to: &'a mut HeapSpace,
    scan: *mut Eterm,
    stats: GcStats,
}

/// The [`EvacDest`] decision for a copying collection: evacuate exactly the references
/// into the [`FromSet`], and put the copies at the bump heap's top.
impl EvacDest for Copier<'_> {
    unsafe fn classify_boxed(&self, ptr: *const Eterm) -> Target {
        if !self.from.contains(ptr) {
            // Literal area, mature space, off-heap or otherwise outside every evacuated
            // range: leave the reference exactly as it is.
            return Target::Foreign;
        }
        // SAFETY: `ptr` is inside a live from-space object, so its header word is
        // readable.
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
        // SAFETY: `ptr` is inside a live from-space cons cell, so its car word is
        // readable.
        if is_non_value(unsafe { *ptr }) {
            Target::Forwarded
        } else {
            Target::Live
        }
    }

    fn can_reserve(&self, words: usize) -> bool {
        self.to.available_words() >= words
    }

    unsafe fn reserve(&mut self, words: usize) -> Result<*mut Eterm> {
        // SAFETY: the caller guarantees `words` fit (`can_reserve` just held), so this
        // hands out `words` exclusively-owned words at the to-space top.
        unsafe { self.to.heap_mut().alloc_words(words) }
    }
}

impl<'a> Copier<'a> {
    /// Creates a copier evacuating `from` into `to`, recording the to-space scan origin.
    pub fn new(from: &'a FromSet, to: &'a mut HeapSpace) -> Copier<'a> {
        // The scan starts where the to-space currently ends, so a copy into a
        // non-empty to-space begins scanning after the words already there.
        let scan = to.top();
        Copier {
            from,
            to,
            scan,
            stats: GcStats::default(),
        }
    }

    /// Forwards each live root word in `roots` in place (updating it to its new address).
    pub fn forward_roots(&mut self, roots: &mut [Eterm]) -> Result<()> {
        for root in roots.iter_mut() {
            // SAFETY: `root` is a live, exclusively-borrowed term word; `forward_slot`
            // only reads it, and (for an immediate or out-of-from-space reference)
            // leaves it untouched.
            unsafe { self.forward_one(root as *mut Eterm)? };
        }
        Ok(())
    }

    /// Runs the Cheney scan of the to-space to completion and returns the statistics.
    pub fn complete(mut self) -> Result<GcStats> {
        // The bound is the *live* to-space top, which grows as more objects are
        // copied in behind the scan pointer — exactly the C `while (n_hp != n_htop)`.
        while self.scan < self.to.top() {
            // SAFETY: `scan` is below `to.top()`, i.e. it addresses a word that has
            // already been written into the to-space.
            let gval = unsafe { *self.scan };
            match primary_tag(gval) {
                PrimaryTag::Boxed | PrimaryTag::List => {
                    let slot = self.scan;
                    // SAFETY: `slot` addresses a live reference word of the to-space;
                    // `forward_one` copies the referenced object (or follows an
                    // already-forwarded marker) and rewrites the word in place.
                    unsafe { self.forward_one(slot)? };
                    // SAFETY: the reference word is one word; `add(1)` stays within
                    // the to-space (or lands exactly on its top).
                    self.scan = unsafe { self.scan.add(1) };
                }
                PrimaryTag::Header => {
                    // One canonical step over a whole object: size it, forward each of its
                    // copy child slots (the word offsets `beam_term::layout` reports for its
                    // shape), then jump past it. A transparent arityval/record, a fun's
                    // inlined descriptor, a map's size/occupancy words and an `ErlSubBits`'s
                    // `orig` are all handled by that shared walk, so no shape logic lives
                    // here — the reference forwarding is the only thing this loop does.
                    let scan = self.scan;
                    let total = move_size(gval, scan);
                    // `for_each_child_slot`'s callback is infallible; the accumulator
                    // carries the first forwarding failure out of the walk.
                    let mut err: Result<()> = Ok(());
                    // SAFETY: `scan` addresses a live object header in the to-space of
                    // exactly `total` words (the object was copied verbatim), so every child
                    // slot the canonical walk reports lies inside it and holds a live term
                    // reference.
                    unsafe {
                        for_each_child_slot(gval, scan, |offset| {
                            if err.is_ok() {
                                err = self.forward_one(scan.add(offset));
                            }
                        });
                    }
                    err?;
                    // SAFETY: `total` is exactly the object's word count, all written into
                    // the to-space, so the advanced position stays within it (or lands
                    // exactly on its top).
                    self.scan = unsafe { scan.add(total) };
                }
                PrimaryTag::Immed1 => {
                    // SAFETY: an immediate occupies one word.
                    self.scan = unsafe { self.scan.add(1) };
                }
            }
        }

        let mut stats = self.stats;
        // Words reclaimed = the from-space words that were *not* copied. Literal and
        // other out-of-from-space references were never part of `from.words()`, so
        // they do not affect this figure.
        stats.words_reclaimed = self.from.words().saturating_sub(self.stats.words_copied);
        Ok(stats)
    }

    /// Forwards the roots and completes the scan in one call.
    pub fn run(mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        self.forward_roots(roots)?;
        self.complete()
    }

    /// Forwards the term word at `slot` through the shared [`forward_slot`] primitive and
    /// charges the moved object (when there was one) to this copier's statistics.
    ///
    /// # Safety
    ///
    /// As [`forward_slot`].
    unsafe fn forward_one(&mut self, slot: *mut Eterm) -> Result<()> {
        // SAFETY: forwarded to the shared primitive; the caller guarantees `slot`, and
        // `Copier`'s `EvacDest` implementation only ever dereferences pointers inside
        // `self.from`.
        let copied = unsafe { forward_slot(self, slot)? };
        if copied > 0 {
            self.stats.objects_copied += 1;
            self.stats.words_copied += copied;
        }
        Ok(())
    }
}

/// The primitive young-generation collection entry: evacuate the live terms reachable
/// from `roots` out of `from` into `to`, updating `roots` in place.
pub fn collect(from: &HeapSpace, to: &mut HeapSpace, roots: &mut [Eterm]) -> Result<GcStats> {
    let mut set = FromSet::new();
    set.add(from);
    Copier::new(&set, to).run(roots)
}
