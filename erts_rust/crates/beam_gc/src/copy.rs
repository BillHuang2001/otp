//! The Cheney copy engine: [`Copier`] and the [`collect`] entry point.
//!
//! `unsafe` is confined to this module's word-level copier, which mirrors the legacy
//! C `move_boxed`/`move_cons`/`sweep` (`erts/emulator/beam/erl_gc.c`) — in-place
//! forwarding plus a Cheney scan of the freshly-copied to-space.

use beam_common::Result;
use beam_term::prelude::{
    boxed_val, header_arity, header_is_thing, is_flatmap_header, is_fun_header, is_header,
    is_map_header, is_non_value, list_val, make_boxed, make_list, map_header_val, primary_tag,
    thing_arityval, Eterm, PrimaryTag, HEADER_SUB_BITS, THE_NON_VALUE,
};

use crate::error::heap_exhausted;
use crate::space::{FromSet, HeapSpace};
use crate::stats::GcStats;

/// `FUN_HEADER_ENV_SIZE_OFFS` (`erl_term.h`: `_HEADER_ARITY_OFFS + 2 + 8`).
///
/// Defined locally: `beam_term` exposes `is_fun_header` but not this bit offset of the
/// fun header's free-variable count (the `fun_num_free` field, `erl_fun.h:94`).
const FUN_HEADER_ENV_SIZE_OFFS: u32 = 16;

/// `fun_num_free(hdr)` — the number of free variables in a fun's environment
/// (`erl_fun.h:94`).
#[inline]
fn fun_num_free(hdr: Eterm) -> usize {
    ((hdr.raw() >> FUN_HEADER_ENV_SIZE_OFFS) & 0xFF) as usize
}

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
            unsafe { self.forward_slot(root as *mut Eterm)? };
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
                    // `forward_slot` copies the referenced object (or follows an
                    // already-forwarded marker) and rewrites the word in place.
                    unsafe { self.forward_slot(slot)? };
                    // SAFETY: the reference word is one word; `add(1)` stays within
                    // the to-space (or lands exactly on its top).
                    self.scan = unsafe { self.scan.add(1) };
                }
                PrimaryTag::Header => {
                    if !header_is_thing(gval) {
                        // A transparent header (arityval/record): its elements follow
                        // as separately-scanned words.
                        // SAFETY: one word further along the to-space.
                        self.scan = unsafe { self.scan.add(1) };
                    } else {
                        if gval == HEADER_SUB_BITS {
                            // `ErlSubBits.orig` is word 4 of the object (`erl_bits.h`);
                            // it holds the underlying bitstring as a boxed reference.
                            // SAFETY: `scan` addresses a live `ErlSubBits` thing, so
                            // word 4 is inside it.
                            let orig = unsafe { self.scan.add(4) };
                            // SAFETY: `orig` addresses the live `orig` word of the thing.
                            unsafe { self.forward_slot(orig)? };
                        }
                        // SAFETY: a thing spans `thing_arityval(hdr) + 1` words, all of
                        // which are inside the to-space.
                        self.scan = unsafe { self.scan.add(thing_arityval(gval) + 1) };
                    }
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

    /// The total words (header word + payload) of the boxed object at `ptr`, whose
    /// header word is `hdr` — the `nelts + 1` of the C `move_boxed`.
    ///
    /// `header_arity` already applies the map and fun special cases of
    /// `_unchecked_header_arity`, so only the *extra* words of a flatmap's size word,
    /// a hashmap's occupancy bitmap count and a fun's environment are added here.
    ///
    /// # Preconditions
    ///
    /// `ptr` must point at a live, boxed object of the from-space whose first word is
    /// `hdr`; it is dereferenced only for a flatmap's size word.
    fn move_size(hdr: Eterm, ptr: *const Eterm) -> usize {
        let mut nelts = header_arity(hdr);
        if is_map_header(hdr) {
            if is_flatmap_header(hdr) {
                // SAFETY: a flatmap header is immediately followed by its untagged
                // size word, which `ptr` addresses; the object is live in the
                // from-space and therefore readable.
                nelts += unsafe { *ptr.add(1) }.raw() + 1;
            } else {
                // `hashmap_bitcount(MAP_HEADER_VAL(hdr))`: one occupied slot per set bit.
                nelts += map_header_val(hdr).count_ones() as usize;
            }
        } else if is_fun_header(hdr) {
            nelts += fun_num_free(hdr);
        }
        nelts + 1
    }

    /// Forwards the term word at `slot`, mirroring the C `sweep()` switch body.
    ///
    /// # Safety
    ///
    /// `slot` must address a live, writable term word. If it holds a BOXED or LIST
    /// reference, the referenced object must be a live from-space object (when it is
    /// in the [`FromSet`]) or any other valid, non-from-space address (when it is
    /// left verbatim).
    unsafe fn forward_slot(&mut self, slot: *mut Eterm) -> Result<()> {
        // SAFETY: the caller guarantees `slot` addresses a live term word.
        let term = unsafe { *slot };
        match primary_tag(term) {
            PrimaryTag::Boxed => {
                let ptr = boxed_val(term);
                if !self.from.contains(ptr) {
                    // Literal area, mature space, off-heap or otherwise outside every
                    // evacuated range: leave the reference exactly as it is.
                    return Ok(());
                }
                // SAFETY: `ptr` is inside a live from-space object, so its header word
                // is readable.
                let val = unsafe { *ptr };
                if !is_header(val) {
                    // `IS_MOVED_BOXED`: the header was overwritten with the new BOXED
                    // pointer, so follow the marker rather than copying again.
                    // SAFETY: `slot` is writable by the caller.
                    unsafe { *slot = val };
                    return Ok(());
                }
                let total = Self::move_size(val, ptr);
                if self.to.available_words() < total {
                    // Pre-check so the failure is attributable (and `heap_exhausted`
                    // is exercised); `alloc_words` re-checks and propagates the same
                    // failure below.
                    return Err(heap_exhausted(total));
                }
                // SAFETY: `total` words were just confirmed available, so this returns
                // `total` exclusively-owned, writable words.
                let dst = unsafe { self.to.heap_mut().alloc_words(total)? };
                // SAFETY: `ptr` is a live from-space object of exactly `total` words;
                // `dst` is a distinct, non-overlapping `total`-word to-space region
                // (the spaces are separate allocations).
                unsafe { core::ptr::copy_nonoverlapping(ptr, dst, total) };
                // SAFETY: `ptr` is writable (a from-space object being evacuated) and
                // `slot` is writable by the caller; `dst` is the freshly-written
                // to-space copy the marker now points at.
                unsafe {
                    *ptr = make_boxed(dst as *const Eterm); // forwarding marker
                    *slot = *ptr; // follow the marker: the reference now points at `dst`
                }
                self.stats.objects_copied += 1;
                self.stats.words_copied += total;
            }
            PrimaryTag::List => {
                let ptr = list_val(term);
                if !self.from.contains(ptr) {
                    return Ok(());
                }
                // SAFETY: `ptr` is inside a live from-space cons cell, so its car word
                // is readable.
                let car = unsafe { *ptr };
                if is_non_value(car) {
                    // `IS_MOVED_CONS`: the car holds `THE_NON_VALUE` and the cdr the
                    // new LIST pointer, so follow the marker.
                    // SAFETY: `ptr` is a live cons cell; its cdr is in-bounds. `slot`
                    // is writable by the caller.
                    unsafe { *slot = *ptr.add(1) };
                    return Ok(());
                }
                if self.to.available_words() < 2 {
                    return Err(heap_exhausted(2));
                }
                // SAFETY: a cons cell is 2 words, just confirmed available.
                let dst = unsafe { self.to.heap_mut().alloc_words(2)? };
                // SAFETY: `ptr` is a live 2-word from-space cell; `dst` is a distinct,
                // non-overlapping 2-word to-space region. `*slot = *ptr.add(1)` writes
                // the cdr's new LIST pointer back into the reference.
                unsafe {
                    *dst = car;
                    *dst.add(1) = *ptr.add(1);
                    *ptr = THE_NON_VALUE; // forwarding indicator
                    *ptr.add(1) = make_list(dst as *const Eterm); // forwarding address
                    *slot = *ptr.add(1);
                }
                self.stats.objects_copied += 1;
                self.stats.words_copied += 2;
            }
            // A bare header (already consumed by the scan) or an immediate is not a
            // reference: nothing to forward.
            PrimaryTag::Header | PrimaryTag::Immed1 => {}
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
