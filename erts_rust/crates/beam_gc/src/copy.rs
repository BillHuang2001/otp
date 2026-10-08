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
//! plus a Cheney scan of the freshly-copied to-space. Object sizing ([`move_size`]), the
//! header/thing stepping rule ([`scan_step`]) and the reference-forwarding switch
//! ([`forward_slot`], over the [`EvacDest`] seam) are `pub(crate)` so that the two
//! evacuating spaces share one implementation instead of each carrying its own copy of
//! the marker scheme.

use beam_common::Result;
use beam_term::prelude::{
    boxed_val, fun_desc_words, fun_env_offset_header, fun_kind_of_header, fun_num_free,
    header_arity, header_is_thing, is_flatmap_header, is_fun_header, is_header, is_map_header,
    is_non_value, list_val, make_boxed, make_list, map_header_val, primary_tag, thing_arityval,
    Eterm, PrimaryTag, HEADER_SUB_BITS, THE_NON_VALUE,
};

use crate::error::heap_exhausted;
use crate::space::{FromSet, HeapSpace};
use crate::stats::GcStats;

// ---------------------------------------------------------------------------
// Shared word-level forwarding primitives (used by this module and `immix.rs`)
// ---------------------------------------------------------------------------

/// How the word-by-word scan advances over a *non-reference* word it has just read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScanStep {
    /// Advance one word: an immediate, or a transparent header (an arityval or a
    /// record) whose elements follow as separately scanned words.
    Word,
    /// A thing header: advance `step` words. When `orig_index` is `Some`, that word of
    /// the thing holds an interior reference that must be forwarded before stepping
    /// over the thing (the C `HEADER_SUB_BITS` case, whose `orig` is word 4).
    Thing {
        /// The number of words the thing occupies (header + payload).
        step: usize,
        /// The word index of an interior reference inside the thing, if it has one.
        orig_index: Option<usize>,
    },
}

/// Classifies the header word `gval` for the word-by-word scan of the C `sweep()`.
///
/// Precondition (checked by every caller): `gval` carries the [`PrimaryTag::Header`]
/// primary tag, i.e. it is *not* a reference word.
///
/// A **fun** is sized from its inlined-descriptor layout (`beam_term::fun`) rather than
/// `thing_arityval`: a fun block here is `[header][desc_words(kind)][env[0..num_free]]`
/// with the C `entry` pointer expanded into descriptor words, so `thing_arityval(gval) + 1`
/// (only 2, the C `ERL_FUN_SIZE`) would land the scan on the *second* descriptor word — a
/// raw `index` for a `Local` fun, raw `uniq` bytes for an `Imported` one — and follow that
/// raw value as if it were a term reference, corrupting the heap. Skipping the whole
/// descriptor opaquely (`fun_env_offset_header == 1 + desc_words`) leaves the word-by-word
/// loop to scan exactly the `num_free` env words that follow.
pub(crate) fn scan_step(gval: Eterm) -> Result<ScanStep> {
    if !header_is_thing(gval) {
        // A transparent header (arityval/record): its elements follow as separately
        // scanned words.
        return Ok(ScanStep::Word);
    }
    if is_fun_header(gval) {
        return Ok(ScanStep::Thing {
            step: fun_env_offset_header(gval)?,
            orig_index: None,
        });
    }
    if gval == HEADER_SUB_BITS {
        // `ErlSubBits.orig` is word 4 of the object (`erl_bits.h`); it holds the
        // underlying bitstring as a boxed reference.
        return Ok(ScanStep::Thing {
            step: thing_arityval(gval) + 1,
            orig_index: Some(4),
        });
    }
    Ok(ScanStep::Thing {
        step: thing_arityval(gval) + 1,
        orig_index: None,
    })
}

/// The total words (header word + payload) of the boxed object at `ptr`, whose header
/// word is `hdr` — the `nelts + 1` of the C `move_boxed`.
///
/// A fun is sized explicitly from its inlined-descriptor layout (`beam_term::fun`):
/// `1 + desc_words(kind) + num_free`. `header_arity` is **not** used for a fun because it
/// deliberately carries the C `ERL_FUN_SIZE - 1 == 1` value (the old single
/// `entry`-pointer assumption), which would under-size the copy by `desc_words(kind) - 1`
/// words (1 for a `Local`/`External` fun, 9 for an `Imported` one). For every other boxed
/// object `header_arity` already applies the map special case of
/// `_unchecked_header_arity`, so only the *extra* words of a flatmap's size word and a
/// hashmap's occupancy bitmap count are added here.
///
/// # Preconditions
///
/// `ptr` must point at a live, boxed object whose first word is `hdr`; it is dereferenced
/// only for a flatmap's size word (which `hdr` guarantees to be present).
pub(crate) fn move_size(hdr: Eterm, ptr: *const Eterm) -> usize {
    if is_fun_header(hdr) {
        // `fun_kind_of_header` is the infallible bit extraction and is valid because
        // `is_fun_header(hdr)` just held; this avoids a `Result` on a path that cannot
        // fail (never `unwrap` runtime data).
        return 1 + fun_desc_words(fun_kind_of_header(hdr)) + fun_num_free(hdr);
    }
    let mut nelts = header_arity(hdr);
    if is_map_header(hdr) {
        if is_flatmap_header(hdr) {
            // SAFETY: a flatmap header is immediately followed by its untagged size
            // word, which `ptr` addresses; the object is live and therefore readable.
            nelts += unsafe { *ptr.add(1) }.raw() + 1;
        } else {
            // `hashmap_bitcount(MAP_HEADER_VAL(hdr))`: one occupied slot per set bit.
            nelts += map_header_val(hdr).count_ones() as usize;
        }
    }
    nelts + 1
}

/// Calls `visit` with the address of every *reference word* of the boxed object at
/// `ptr`, which occupies `total` words (`total` = the [`move_size`] of its header).
///
/// This is the **per-object** form of the word walk [`Copier::complete`] performs over its
/// contiguous to-space region: both step through words with [`scan_step`], so a fun's
/// inlined descriptor is skipped opaquely, a transparent arityval is stepped word by word,
/// and an `ErlSubBits`'s `orig` word is visited. They are not merged into one routine
/// because `complete` walks a *region* that grows as it copies (the Cheney invariant
/// `while (n_hp != n_htop)`), not an object of known size; sharing the loop would thread a
/// moving region bound through the copier's hot path for no gain. The mark-region space
/// ([`crate::immix`]) uses this to enumerate objects it marks and objects it copies.
///
/// # Safety
///
/// `ptr` must address a live boxed object of exactly `total` words, all of which are
/// readable, and `visit` must not invalidate them.
pub(crate) unsafe fn for_each_ref<F>(
    ptr: *mut Eterm,
    total: usize,
    visit: &mut F,
) -> Result<()>
where
    F: FnMut(*mut Eterm) -> Result<()>,
{
    // Word 0 is the object's header, and it is the header that decides the payload
    // layout: a transparent arityval is stepped over so its elements are visited one by
    // one, while a fun's inlined descriptor and a map's untagged size/occupancy words are
    // opaque and jumped over as a whole. Starting at the header (exactly what the
    // `complete()` region walk does when it meets an object's header word) is what makes
    // this the *per-object* form of that walk; starting at word 1 would read a fun's raw
    // descriptor word (`uniq`/`index`) as if it were a term reference and, worse, would
    // never visit the fun's real environment words.
    let mut i = 0usize;
    while i < total {
        // SAFETY: `i < total`, so this word is inside the live `total`-word object.
        let word = unsafe { *ptr.add(i) };
        match primary_tag(word) {
            PrimaryTag::Boxed | PrimaryTag::List => {
                // SAFETY: word `i` of a live object is a live reference word.
                visit(unsafe { ptr.add(i) })?;
                i += 1;
            }
            PrimaryTag::Header => match scan_step(word)? {
                ScanStep::Word => i += 1,
                ScanStep::Thing { step, orig_index } => {
                    if let Some(index) = orig_index {
                        let at = i + index;
                        if at < total {
                            // SAFETY: `at < total`, so the interior reference is inside the
                            // object.
                            visit(unsafe { ptr.add(at) })?;
                        }
                    }
                    if step == 0 || i + step > total {
                        // A malformed object cannot happen for the objects this crate
                        // writes; stopping is the safe reading of one rather than
                        // walking out of the object.
                        debug_assert!(false, "object at {ptr:p} over-runs its {total} words");
                        break;
                    }
                    i += step;
                }
            },
            PrimaryTag::Immed1 => i += 1,
        }
    }
    Ok(())
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
                PrimaryTag::Header => match scan_step(gval)? {
                    ScanStep::Word => {
                        // SAFETY: one word further along the to-space.
                        self.scan = unsafe { self.scan.add(1) };
                    }
                    ScanStep::Thing { step, orig_index } => {
                        if let Some(index) = orig_index {
                            // SAFETY: `scan` addresses a live thing of more than
                            // `index` words, so the interior reference is inside it.
                            let orig = unsafe { self.scan.add(index) };
                            // SAFETY: `orig` addresses a live interior reference word.
                            unsafe { self.forward_one(orig)? };
                        }
                        // SAFETY: `step` words are exactly the header + descriptor + env
                        // of a fun (or the header + payload of any other thing), all
                        // written into the to-space, so the advanced position stays
                        // within it.
                        self.scan = unsafe { self.scan.add(step) };
                    }
                },
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
