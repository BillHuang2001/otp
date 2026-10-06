//! The Cheney copy engine: [`Copier`] and the [`collect`] entry point.
//!
//! `unsafe` is confined to this module's word-level copier, which mirrors the legacy
//! C `move_boxed`/`move_cons`/`sweep` (`erts/emulator/beam/erl_gc.c`) — in-place
//! forwarding plus a Cheney scan of the freshly-copied to-space.

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
                        let step = if is_fun_header(gval) {
                            // A fun block is `[header][desc_words(kind)][env[0..num_free]]`
                            // with the C `entry` expanded into inlined descriptor words
                            // (`beam_term::fun`). `thing_arityval(gval) + 1` is only 2 (the
                            // C `ERL_FUN_SIZE`), which would land the scan on the *second*
                            // descriptor word — a raw `index` for a `Local` fun, raw `uniq`
                            // bytes for an `Imported` one — and follow that raw value as if
                            // it were a term reference, corrupting the heap. Skip the whole
                            // descriptor opaquely (`fun_env_offset_header == 1 + desc_words`)
                            // so the word-by-word loop scans exactly the `num_free` env
                            // words that follow.
                            fun_env_offset_header(gval)?
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
                            thing_arityval(gval) + 1
                        };
                        // SAFETY: `step` words are exactly the header + descriptor + env of
                        // a fun (or the header + payload of any other thing), all written
                        // into the to-space, so the advanced position stays within it.
                        self.scan = unsafe { self.scan.add(step) };
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
    /// A fun is sized explicitly from its inlined-descriptor layout
    /// (`beam_term::fun`): `1 + desc_words(kind) + num_free`. `header_arity` is **not**
    /// used for a fun because it deliberately carries the C `ERL_FUN_SIZE - 1 == 1`
    /// value (the old single `entry`-pointer assumption), which would under-size the
    /// copy by `desc_words(kind) - 1` words (1 for a `Local`/`External` fun, 9 for an
    /// `Imported` one). For every other boxed object `header_arity` already applies the
    /// map special case of `_unchecked_header_arity`, so only the *extra* words of a
    /// flatmap's size word and a hashmap's occupancy bitmap count are added here.
    ///
    /// # Preconditions
    ///
    /// `ptr` must point at a live, boxed object of the from-space whose first word is
    /// `hdr`; it is dereferenced only for a flatmap's size word.
    fn move_size(hdr: Eterm, ptr: *const Eterm) -> usize {
        if is_fun_header(hdr) {
            // `fun_kind_of_header` is the infallible bit extraction and is valid
            // because `is_fun_header(hdr)` just held; this avoids a `Result` on a path
            // that cannot fail (never `unwrap` runtime data).
            return 1 + fun_desc_words(fun_kind_of_header(hdr)) + fun_num_free(hdr);
        }
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
