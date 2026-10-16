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

//! GenImmix-style **mark-region mature space** ([`ImmixSpace`]) — the measurement-gated
//! Stage-4 maturation of the mature generation.
//!
//! Design source of truth: `ERTS_RUST_REWRITE_DEEPDIVE.md` §D, whose *Fit matrix* names
//! GenImmix/StickyImmix the **best modernization candidate for the mature generation**
//! ("Immix alone — worse for small heaps (per-line metadata)") and whose §H risk register
//! flags "Stage 4 GenImmix mature generation" as **measurement-gated**, high effort and
//! uncertain payoff; `ERTS_RUST_REWRITE_EVALUATION.md` ~240-250 reaches the same
//! conclusion ("keep the generational copying design … or move toward mark-region /
//! Immix — better for large heaps and lower copy cost"). This module therefore *adds* an
//! option next to the copying [`HeapSpace`](crate::space::HeapSpace); it does not replace
//! the copying design, and being a **stop-the-world mark-region space over immutable
//! terms it needs no generational write barrier** — the barrier-free young design
//! (`beam_gc` §5) is untouched.
//!
//! # Geometry
//!
//! One contiguous region is partitioned into fixed-size **blocks**, each subdivided into
//! fixed-size **lines**:
//!
//! * [`LINE_WORDS`] = 16 (128 bytes on the 64-bit target) — the classic Immix line size.
//! * [`LINES_PER_BLOCK`] = 16.
//! * [`BLOCK_WORDS`] = 256 (2 KiB) — a whole number of lines.
//!
//! The *classic* Immix block is 32 KiB. That is tuned for multi-gigabyte heaps; an OTP
//! **process** heap starts at a few hundred words (`HeapPolicy::initial_young_words` is
//! 233), so a 32 KiB block would cost every process 32 KiB for a ~2 KiB heap. 2 KiB
//! blocks keep the smallest useful Immix space at ~2 KiB while still being a whole number
//! of 128-byte lines; the per-block metadata is 3 × `u64` = 24 bytes, i.e. 1.2 % of a
//! block. Objects larger than one block are supported by claiming a run of whole blocks
//! (§ "Large objects"). Tuning `BLOCK_WORDS` upward is a measurement decision for the
//! benchmark gate, not a correctness one.
//!
//! # Allocation is line-granular (a documented M1 simplification)
//!
//! Classic Immix allocates word-granularly inside a "hole" (a run of free lines), so an
//! object may start mid-line. Recovering those object starts at mark/evacuation time
//! needs a word-granular start map. This implementation instead makes every
//! [`alloc_words`](ImmixSpace::alloc_words) call consume a whole number of lines and
//! return a **line-aligned** pointer, so object starts are always line-aligned and the
//! mark and evacuation walks are *exact* without any extra map. The price is internal
//! fragmentation of up to `LINE_WORDS - 1` words per object (a 2-word cons cell occupies
//! a whole 16-word line); word-granular packing plus a start bitmap is the follow-up that
//! would remove it. The same rule makes "which line starts an object" decidable from the
//! per-line mark bitmap alone.
//!
//! # Per-block state
//!
//! Three line bitmaps (`LINES_PER_BLOCK` = 16 lines ⇒ one `u64` each) plus a hole cursor:
//!
//! | bitmap | meaning |
//! |---|---|
//! | `allocated` | the line has been handed out and still belongs to an object |
//! | `marked` | the line is covered by an object that was live at the last mark |
//! | `kept` | the line must survive the **running** collection (a copy destination, or a pinned object) |
//!
//! `allocated` alone is the allocator's free/busy map; `marked` is the mark result;
//! `kept` is the evacuation result. After a collection `allocated = kept`, so the space is
//! immediately usable again and every hole the collection created is reusable.
//!
//! # Block classification (per-object reclamation)
//!
//! From the marks each block is classified ([`BlockState`]):
//!
//! * [`BlockState::Free`] — no live line: the whole block is reclaimed.
//! * [`BlockState::Partial`] — some live lines: **only the live objects are evacuated**
//!   and the rest of the block is reclaimed.
//! * [`BlockState::AllMarked`] — every line is live: the block is **pinned and never
//!   evacuated** (the classic Immix full-block rule).
//!
//! This is what keeps a partially-live block from being evacuated wholesale.
//!
//! # Large objects
//!
//! A request larger than [`BLOCK_WORDS`] claims a run of whole blocks starting at a block
//! boundary (the classic Immix large-object path, simplified to whole blocks). Because a
//! live object's marks cover *all* of its lines and it always starts on a block boundary,
//! the **first** block it occupies is entirely covered by its marks and is therefore
//! `AllMarked`; a large object is thus always pinned — even when a later block it reaches
//! is only partially covered — and its interior is never mistaken for a sequence of
//! objects.
//!
//! # Module layout
//!
//! The algorithm is split so no file approaches the crate's ~1000-line budget: this file
//! holds the geometry, the block metadata, allocation and the accounting views;
//! [`collect`](self::collect) holds the mark / classify / evacuate / finalize phases and
//! the [`EvacDest`](crate::copy::EvacDest) implementation; `tests` holds the unit tests.
//!
//! # Sharing with the copying engine
//!
//! Object sizing ([`move_size`](crate::copy::move_size)) and the per-object reference
//! enumerator ([`for_each_ref`](crate::copy::for_each_ref)) are `pub(crate)` items of
//! `copy.rs`, both driven by the canonical per-object walk of [`beam_term::layout`]; the
//! whole in-place forwarding marker protocol
//! ([`forward_slot`](crate::copy::forward_slot) over the
//! [`EvacDest`](crate::copy::EvacDest) seam) shares the same `copy.rs` primitives. This
//! module implements [`EvacDest`](crate::copy::EvacDest) — the *one* decision that
//! differs between the two spaces is *which* objects move — and shares everything else.

use core::cell::Cell;
use core::mem::size_of;

use beam_alloc::{AllocRef, Heap};
use beam_common::Result;
use beam_term::prelude::{
    is_boxed, is_list, make_list, make_tuple, ptr_val, try_make_arityval, Eterm,
};

use crate::error::heap_exhausted;

mod collect;
#[cfg(test)]
mod tests;

/// Words per Immix line: 128 bytes on the 64-bit target — the classic Immix line size
/// (`ERTS_RUST_REWRITE_DEEPDIVE.md` §D).
pub const LINE_WORDS: usize = 16;

/// Lines per block.
pub const LINES_PER_BLOCK: usize = 16;

/// Words per block: a whole number of [`LINE_WORDS`]-word lines.
pub const BLOCK_WORDS: usize = LINE_WORDS * LINES_PER_BLOCK;

/// "No block is currently being filled" (the allocator cursor is a hint, not state).
const NO_BLOCK: usize = usize::MAX;

/// All [`LINES_PER_BLOCK`] bits set (the bitmap is one `u64` per block).
const ALL_LINES: u64 = u64::MAX >> (64 - LINES_PER_BLOCK);

/// The line bitmap for `[from, from + len)` — `len == 0` yields an empty mask, and
/// `from + len <= LINES_PER_BLOCK` is the caller's obligation.
#[inline]
fn line_mask(from: usize, len: usize) -> u64 {
    if len == 0 {
        return 0;
    }
    debug_assert!(from + len <= LINES_PER_BLOCK);
    (u64::MAX >> (64 - len)) << from
}

/// A single bit, `1 << line`.
#[inline]
fn line_bit(line: usize) -> u64 {
    debug_assert!(line < LINES_PER_BLOCK);
    1u64 << line
}

/// The number of lines a request of `words` term words occupies (rounded up; see the
/// module docs on line-granular allocation).
#[inline]
pub const fn lines_for(words: usize) -> usize {
    words.div_ceil(LINE_WORDS)
}

/// The lines a collection assigned to a block; the classifier `ImmixSpace::classify`
/// derives it from the mark bitmap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    /// No line is covered by a live object: the entire block is reclaimable.
    Free,
    /// Some — but not all — lines are covered by live objects: those objects are
    /// evacuated and the remaining lines reclaimed.
    Partial,
    /// Every line is covered by a live object: the block is **pinned** and is never
    /// evacuated.
    AllMarked,
}

/// The state the block's `allocated` bitmap implies: the classification used after a
/// collection has finished (every surviving line is then a live line in its final
/// position).
fn state_of(bits: u64) -> BlockState {
    if bits == 0 {
        BlockState::Free
    } else if bits == ALL_LINES {
        BlockState::AllMarked
    } else {
        BlockState::Partial
    }
}

/// The per-block metadata: three line bitmaps and the allocation cursor.
///
/// Every mutable field is a [`Cell`] so the mark phase can run behind `&self` while a
/// collection still holds `&mut self` for its evacuation half. Copying out the bitmap
/// before a call keeps the borrow checker happy without any interior borrow guards.
struct Block {
    state: Cell<BlockState>,
    /// Where the allocator starts searching this block for a hole. A hint only: every
    /// search falls back to a full scan from line 0, so a hole is never missed.
    cursor: Cell<usize>,
    allocated: Cell<u64>,
    marked: Cell<u64>,
    kept: Cell<u64>,
}

impl Block {
    fn new() -> Block {
        Block {
            state: Cell::new(BlockState::Free),
            cursor: Cell::new(0),
            allocated: Cell::new(0),
            marked: Cell::new(0),
            kept: Cell::new(0),
        }
    }
}

/// ORs `[start_g, start_g + len)` into the per-block bitmap `pick` selects.
///
/// Global line index `g` denotes line `g % LINES_PER_BLOCK` of block `g / LINES_PER_BLOCK`,
/// so one call can span blocks — which is exactly what a large object's mark needs.
fn set_lines(start_g: usize, len: usize, blocks: &[Block], pick: fn(&Block) -> &Cell<u64>) {
    debug_assert!(start_g + len <= blocks.len() * LINES_PER_BLOCK);
    let mut g = start_g;
    let end = start_g + len;
    while g < end {
        let b = g / LINES_PER_BLOCK;
        let l = g % LINES_PER_BLOCK;
        let take = (LINES_PER_BLOCK - l).min(end - g);
        let cell = pick(&blocks[b]);
        cell.set(cell.get() | line_mask(l, take));
        g += take;
    }
}

/// A point-in-time accounting view of an [`ImmixSpace`] — the numbers a collection uses
/// to decide whether mark-region reclamation is paying off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImmixCounters {
    /// Blocks in the space.
    pub blocks_total: usize,
    /// Blocks with no live line ([`BlockState::Free`]).
    pub blocks_free: usize,
    /// Blocks with some live lines ([`BlockState::Partial`]).
    pub blocks_partial: usize,
    /// Blocks that are fully live and therefore pinned ([`BlockState::AllMarked`]).
    pub blocks_all_marked: usize,
    /// Lines in the space (`block_count * LINES_PER_BLOCK`).
    pub lines_total: usize,
    /// Lines not owned by any object — the reclamation headroom.
    pub lines_free: usize,
    /// Lines covered by a live object at the last mark.
    pub lines_marked: usize,
    /// Total capacity in term words.
    pub capacity_words: usize,
    /// Words owned by objects (whole lines; see the module docs).
    pub used_words: usize,
}

impl ImmixCounters {
    /// Lines owned by an object.
    pub fn lines_allocated(&self) -> usize {
        self.lines_total - self.lines_free
    }

    /// Words still available to the allocator.
    pub fn available_words(&self) -> usize {
        self.capacity_words - self.used_words
    }

    /// The fraction of the space the allocator could hand out right now, in `[0, 1]` —
    /// the "reclaimable" metric a measurement gate watches. `0.0` for an empty space.
    pub fn reclaimable(&self) -> f64 {
        if self.lines_total == 0 {
            0.0
        } else {
            self.lines_free as f64 / self.lines_total as f64
        }
    }
}

/// The work one mark-region collection performed.
///
/// "Evacuated" counts the live objects that were copied into free lines; "pinned" counts
/// the live objects left where they were — either because their block was
/// [`BlockState::AllMarked`], or because the space had no hole large enough. Together they
/// are every object the mark phase found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImmixStats {
    /// Live objects copied into free lines.
    pub objects_evacuated: usize,
    /// Lines the evacuated objects occupied in their from-blocks.
    pub lines_evacuated: usize,
    /// Live objects left in place.
    pub objects_pinned: usize,
    /// Lines the pinned objects occupy.
    pub lines_pinned: usize,
    /// Blocks wholly reclaimed by this collection.
    pub blocks_reclaimed: usize,
    /// Words the collection made available again.
    pub words_reclaimed: usize,
    /// Words still held by pinned objects.
    pub words_pinned: usize,
}

/// A mark-region mature space (GenImmix-style): blocks of lines, line-granular
/// allocation, explicit-worklist marking and per-object reclamation.
///
/// See the module documentation for the geometry, the line bitmaps and the block
/// classification. The space is a *mature generation* option: the nursery stays the
/// copying [`HeapSpace`](crate::space::HeapSpace).
pub struct ImmixSpace {
    heap: Heap,
    blocks: Vec<Block>,
    /// The block the allocator is currently filling ([`NO_BLOCK`] when none).
    current: usize,
    collections: u64,
}

impl ImmixSpace {
    /// Creates an empty mark-region space of **at least** `capacity_words` term words,
    /// rounded up to a whole number of blocks (at least one), over the default
    /// allocator.
    pub fn new(capacity_words: usize) -> Result<ImmixSpace> {
        ImmixSpace::build(Heap::new(block_count(capacity_words) * BLOCK_WORDS)?)
    }

    /// As [`new`](ImmixSpace::new), backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, capacity_words: usize) -> Result<ImmixSpace> {
        ImmixSpace::build(Heap::with_allocator(
            alloc,
            block_count(capacity_words) * BLOCK_WORDS,
        )?)
    }

    /// Wraps an already block-aligned heap.
    fn build(heap: Heap) -> Result<ImmixSpace> {
        let blocks = heap.capacity() / BLOCK_WORDS;
        debug_assert_eq!(
            blocks * BLOCK_WORDS,
            heap.capacity(),
            "the space capacity is a whole number of blocks"
        );
        Ok(ImmixSpace {
            heap,
            blocks: (0..blocks).map(|_| Block::new()).collect(),
            current: NO_BLOCK,
            collections: 0,
        })
    }

    // -----------------------------------------------------------------------
    // Geometry
    // -----------------------------------------------------------------------

    /// The first word of the region.
    pub fn base(&self) -> *mut Eterm {
        // `hend` is `base + capacity`, so subtracting the capacity recovers `base`
        // exactly. `wrapping_sub` computes an address arithmetically, without
        // dereferencing anything.
        self.heap.hend().wrapping_sub(self.heap.capacity())
    }

    /// Whether `ptr` points anywhere inside the region (allocated or not).
    pub fn contains(&self, ptr: *const Eterm) -> bool {
        let addr = ptr as usize;
        let base = self.base() as usize;
        addr >= base && addr < base + self.capacity_words() * size_of::<Eterm>()
    }

    /// Whether the boxed/list term `term` refers into this region.
    pub fn contains_term(&self, term: Eterm) -> bool {
        (is_boxed(term) || is_list(term)) && self.contains(ptr_val(term) as *const Eterm)
    }

    /// Total capacity in term words (a whole number of blocks).
    pub fn capacity_words(&self) -> usize {
        self.heap.capacity()
    }

    /// Number of blocks.
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// Number of lines.
    pub fn line_count(&self) -> usize {
        self.blocks.len() * LINES_PER_BLOCK
    }

    /// Lines owned by an object.
    pub fn used_lines(&self) -> usize {
        self.line_count() - self.free_lines()
    }

    /// Lines the allocator can still hand out.
    pub fn free_lines(&self) -> usize {
        self.blocks
            .iter()
            .map(|b| LINES_PER_BLOCK - b.allocated.get().count_ones() as usize)
            .sum()
    }

    /// Words owned by an object (whole lines; see the module docs).
    pub fn used_words(&self) -> usize {
        self.used_lines() * LINE_WORDS
    }

    /// Words the allocator can still hand out.
    pub fn available_words(&self) -> usize {
        self.free_lines() * LINE_WORDS
    }

    /// How many collections this space has run.
    pub fn collections(&self) -> u64 {
        self.collections
    }

    /// The allocator backing this space.
    pub fn allocator(&self) -> &AllocRef {
        self.heap.allocator()
    }

    /// The accounting view ([`ImmixCounters`]).
    pub fn counters(&self) -> ImmixCounters {
        let mut c = ImmixCounters {
            blocks_total: self.blocks.len(),
            lines_total: self.line_count(),
            capacity_words: self.capacity_words(),
            ..ImmixCounters::default()
        };
        for b in &self.blocks {
            match b.state.get() {
                BlockState::Free => c.blocks_free += 1,
                BlockState::Partial => c.blocks_partial += 1,
                BlockState::AllMarked => c.blocks_all_marked += 1,
            }
            c.lines_free += LINES_PER_BLOCK - b.allocated.get().count_ones() as usize;
            c.lines_marked += b.marked.get().count_ones() as usize;
        }
        c.used_words = c.lines_allocated() * LINE_WORDS;
        c
    }

    /// Releases every object in the space (every line becomes free again).
    ///
    /// Invalidation, not reclamation: any term previously handed out from this space is
    /// dangling afterwards.
    pub fn reset(&mut self) {
        for b in &self.blocks {
            b.state.set(BlockState::Free);
            b.cursor.set(0);
            b.allocated.set(0);
            b.marked.set(0);
            b.kept.set(0);
        }
        self.current = NO_BLOCK;
    }

    // -----------------------------------------------------------------------
    // Allocation
    // -----------------------------------------------------------------------

    /// Hands out at least `words` **uninitialised** writable term words at a line-aligned
    /// address, preferring the block the allocator is already filling and otherwise
    /// skipping to another block or to a hole a previous collection left. A request of
    /// zero words claims nothing (and returns the base).
    ///
    /// # Safety
    ///
    /// The returned words are uninitialised and are owned exclusively by the caller until
    /// the next collection; they must be written before they are read, and they must not
    /// be used after [`reset`](ImmixSpace::reset) or a [`collect`](ImmixSpace::collect)
    /// that reclaims them.
    pub unsafe fn alloc_words(&mut self, words: usize) -> Result<*mut Eterm> {
        let need = lines_for(words);
        match self.find_run(need) {
            Some(start_g) => {
                self.claim(start_g, need);
                Ok(self.base().wrapping_add(start_g * LINE_WORDS))
            }
            None => Err(heap_exhausted(words)),
        }
    }

    /// Builds a cons cell `[Car, Cdr]` in this space.
    pub fn cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm> {
        // SAFETY: `alloc_words` returned two exclusively-owned, writable words.
        let p = unsafe { self.alloc_words(2)? };
        // SAFETY: `p` is the base of the two words just allocated.
        unsafe {
            *p = car;
            *p.add(1) = cdr;
        }
        Ok(make_list(p as *const Eterm))
    }

    /// Builds a tuple of `elems` in this space.
    pub fn tuple(&mut self, elems: &[Eterm]) -> Result<Eterm> {
        let n = elems.len();
        let header: Eterm = try_make_arityval(n)?;
        // SAFETY: `alloc_words` returned `n + 1` exclusively-owned, writable words.
        let p = unsafe { self.alloc_words(n + 1)? };
        // SAFETY: the header goes in word 0 and the `n` elements in the words after it.
        unsafe {
            *p = header;
            core::ptr::copy_nonoverlapping(elems.as_ptr(), p.add(1), n);
        }
        Ok(make_tuple(p))
    }

    /// Whether a copy destination of `words` term words can still be reserved — the exact
    /// question [`find_run`](ImmixSpace::find_run) answers, so it never over-reports.
    pub fn can_reserve(&self, words: usize) -> bool {
        self.find_run(lines_for(words)).is_some()
    }

    /// The global line the first hole of `need` consecutive unallocated lines starts at.
    ///
    /// Searches the current block from its cursor first (the Immix "keep filling the block
    /// you are in" rule), then every block from line 0 — so a hole an earlier allocation
    /// skipped, or one a collection just created, is always found. A run longer than a
    /// block must be a run of whole free blocks.
    fn find_run(&self, need: usize) -> Option<usize> {
        if need == 0 {
            return Some(0);
        }
        if need <= LINES_PER_BLOCK {
            if self.current != NO_BLOCK && self.current < self.blocks.len() {
                let b = self.current;
                if let Some(l) = self.first_hole(b, self.blocks[b].cursor.get(), need) {
                    return Some(b * LINES_PER_BLOCK + l);
                }
            }
            for b in 0..self.blocks.len() {
                if let Some(l) = self.first_hole(b, 0, need) {
                    return Some(b * LINES_PER_BLOCK + l);
                }
            }
            None
        } else {
            let k = need.div_ceil(LINES_PER_BLOCK);
            if k > self.blocks.len() {
                return None;
            }
            for b in 0..=(self.blocks.len() - k) {
                if self.blocks[b..b + k]
                    .iter()
                    .all(|blk| blk.allocated.get() == 0)
                {
                    return Some(b * LINES_PER_BLOCK);
                }
            }
            None
        }
    }

    /// The first line at or after `from` with `need` consecutive unallocated lines.
    fn first_hole(&self, b: usize, from: usize, need: usize) -> Option<usize> {
        if need == 0 || need > LINES_PER_BLOCK {
            return None;
        }
        let allocated = self.blocks[b].allocated.get();
        let last = LINES_PER_BLOCK - need;
        let mut l = from.min(last);
        while l <= last {
            if allocated & line_mask(l, need) == 0 {
                return Some(l);
            }
            l += 1;
        }
        None
    }

    /// Marks `[start_g, start_g + need)` allocated and moves the allocator cursor past it.
    fn claim(&mut self, start_g: usize, need: usize) {
        set_lines(start_g, need, &self.blocks, |b| &b.allocated);
        let mut g = start_g;
        let end = start_g + need;
        while g < end {
            let b = g / LINES_PER_BLOCK;
            let l = g % LINES_PER_BLOCK;
            let take = (LINES_PER_BLOCK - l).min(end - g);
            self.blocks[b].cursor.set(l + take);
            if self.blocks[b].state.get() == BlockState::Free {
                self.blocks[b].state.set(BlockState::Partial);
            }
            g += take;
        }
        self.current = start_g / LINES_PER_BLOCK;
    }

    /// Reserves `words` destination words for an evacuation: as
    /// [`alloc_words`](ImmixSpace::alloc_words), but additionally records the lines in the
    /// `kept` bitmap so the collection's finalize step retains them.
    fn reserve_for_copy(&mut self, words: usize) -> Result<*mut Eterm> {
        let need = lines_for(words);
        match self.find_run(need) {
            Some(start_g) => {
                self.claim(start_g, need);
                set_lines(start_g, need, &self.blocks, |b| &b.kept);
                Ok(self.base().wrapping_add(start_g * LINE_WORDS))
            }
            None => Err(heap_exhausted(words)),
        }
    }

    // -----------------------------------------------------------------------
    // Releasing a reservation (the inverse of `claim`)
    // -----------------------------------------------------------------------

    /// Un-claims the run of whole lines a previous [`alloc_words`](ImmixSpace::alloc_words)
    /// (or [`cons`](ImmixSpace::cons)/[`tuple`](ImmixSpace::tuple)) handed out at `ptr` for an
    /// object of `words` term words — the exact inverse of the private `claim`.
    ///
    /// It exists so a **promotion's own** reservations can be undone when a later object of
    /// the same promotion finds no contiguous run: after [`release_lines`](Self::release_lines)
    /// the space's `allocated` bitmap and block states are exactly what they were before the
    /// reservation, so the attempt leaves no trace and can be retried against a differently
    /// built region.
    ///
    /// A `ptr` that does not name a line-aligned address of this region was never handed out
    /// here, so there is nothing to release and the call does nothing (the mismatch is a
    /// `debug_assert!`). This is a **safe** method: it reads and writes only this space's own
    /// metadata and never dereferences `ptr`'s memory.
    #[inline]
    pub fn release_words(&mut self, ptr: *mut Eterm, words: usize) {
        let need = lines_for(words);
        if need == 0 {
            return;
        }
        let addr = ptr as usize;
        let base = self.base() as usize;
        let bytes_per_line = LINE_WORDS * size_of::<Eterm>();
        debug_assert!(
            self.contains(ptr as *const Eterm),
            "a released run lies inside this space"
        );
        debug_assert_eq!(
            addr.wrapping_sub(base) % bytes_per_line,
            0,
            "a released run starts a line (alloc_words returns line-aligned words)"
        );
        if addr < base || (addr - base) % bytes_per_line != 0 {
            return;
        }
        self.release_lines((addr - base) / bytes_per_line, need);
    }

    /// Un-claims `need` lines from the global line `start_g` — the inverse of the private
    /// `claim` for a run this space itself reserved.
    ///
    /// The `allocated` bits of `[start_g, start_g + need)` are cleared, and a block whose
    /// `allocated` bitmap becomes empty is put back to [`BlockState::Free`] (the state `claim`
    /// found it in); every other touched block keeps its `Partial` classification. The
    /// `marked`/`kept` bitmaps are deliberately untouched: this is only valid to undo a run
    /// **no collection is running on** — a promotion's own reservation — which is also why
    /// [`alloc_words`](ImmixSpace::alloc_words) never writes them. The allocation cursor and
    /// `current` block are only hints (every search falls back to a full scan, which still
    /// finds the released hole), so they are left as they are.
    ///
    /// A run that is not wholly inside the region is clamped to it (a `debug_assert!` reports
    /// the mistake in a debug build).
    pub fn release_lines(&mut self, start_g: usize, need: usize) {
        if need == 0 {
            return;
        }
        debug_assert!(
            start_g + need <= self.line_count(),
            "a released run stays inside the region"
        );
        let end = start_g.saturating_add(need).min(self.line_count());
        let mut g = start_g;
        while g < end {
            let b = g / LINES_PER_BLOCK;
            let l = g % LINES_PER_BLOCK;
            let take = (LINES_PER_BLOCK - l).min(end - g);
            let block = &self.blocks[b];
            let remaining = block.allocated.get() & !line_mask(l, take);
            block.allocated.set(remaining);
            if remaining == 0 {
                // The block holds no line any more: back to the state `claim` found it in.
                block.state.set(BlockState::Free);
            }
            g += take;
        }
    }
    // -----------------------------------------------------------------------
    // Line queries (`&self`, backed by the per-block `Cell`s)
    // -----------------------------------------------------------------------

    /// The `(block, line)` a pointer falls in, or `None` when it is outside the region.
    ///
    /// A pointer that is not word-aligned truncates — which can only mis-locate a pointer
    /// that is not a term pointer at all, and every caller re-derives the term from the
    /// located word before using it.
    fn locate(&self, ptr: *const Eterm) -> Option<(usize, usize)> {
        let addr = ptr as usize;
        let base = self.base() as usize;
        let bytes = self.capacity_words() * size_of::<Eterm>();
        if addr < base || addr >= base + bytes {
            return None;
        }
        let word = (addr - base) / size_of::<Eterm>();
        let b = word / BLOCK_WORDS;
        let l = (word % BLOCK_WORDS) / LINE_WORDS;
        Some((b, l))
    }

    /// Whether the line holds memory that belongs to an object.
    fn line_allocated(&self, b: usize, l: usize) -> bool {
        self.blocks[b].allocated.get() & line_bit(l) != 0
    }

    /// Whether the line is covered by a live object.
    fn line_marked(&self, b: usize, l: usize) -> bool {
        self.blocks[b].marked.get() & line_bit(l) != 0
    }

    /// Whether the line is retained in place by the running collection: either a
    /// **pinned** live object's own line, or a fresh **destination** line an evacuation
    /// reserved (`reserve_for_copy`). Both survive [`finalize`](ImmixSpace::finalize).
    fn line_kept(&self, b: usize, l: usize) -> bool {
        self.blocks[b].kept.get() & line_bit(l) != 0
    }

    /// The block's `AllMarked` / `Partial` / `Free` classification.
    fn block_state(&self, b: usize) -> BlockState {
        self.blocks[b].state.get()
    }

    /// ORs `len` lines into the mark bitmap from global line `start_g`.
    fn add_marked(&self, start_g: usize, len: usize) {
        debug_assert!(
            start_g + len <= self.line_count(),
            "a mark span stays inside the region"
        );
        set_lines(
            start_g,
            len.min(self.line_count() - start_g),
            &self.blocks,
            |b| &b.marked,
        );
    }

    /// ORs `len` lines into the `kept` bitmap from global line `start_g`.
    fn add_kept(&self, start_g: usize, len: usize) {
        debug_assert!(
            start_g + len <= self.line_count(),
            "an evacuation span stays inside the region"
        );
        set_lines(
            start_g,
            len.min(self.line_count() - start_g),
            &self.blocks,
            |b| &b.kept,
        );
    }
}

/// The number of blocks `capacity_words` needs: at least one, rounded up.
fn block_count(capacity_words: usize) -> usize {
    capacity_words.div_ceil(BLOCK_WORDS).max(1)
}
