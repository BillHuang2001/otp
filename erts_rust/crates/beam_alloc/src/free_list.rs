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

//! Segregated free lists: the per-size-class intrusive lists of free blocks.
//!
//! Implementation module (public for testing/reuse, **not** part of the cross-crate
//! seam). Each allocator size class owns one *bucket* of free blocks: a doubly-linked
//! intrusive list threaded through the free blocks' own payloads, exactly as the
//! legacy allocator does it in `../../erts/emulator/beam/erl_alloc_util.{c,h}`
//! (`link_free_block` / `unlink_free_block` over the `Block_t` link fields) and as the
//! segregated good-fit/best-fit allocators search it (`erl_goodfit_alloc.c`).
//!
//! # Variable-size blocks
//!
//! In the legacy framework a bucket is **not** a set of equal-sized blocks: blocks of
//! differing sizes share one free list and the allocator picks a *good fit* — the
//! smallest block that is at least as large as the request, found within a bounded
//! search depth. This module models exactly that:
//!
//! * [`FreeList::insert`] files a block together with its byte size,
//! * [`FreeList::take_fit`] finds and unlinks the block that best fits a request,
//! * [`FreeList::remove`] unlinks one specific block,
//! * [`FreeList::refile`] re-files a block whose size changed — what coalescing does
//!   when two physically adjacent free blocks are merged into one bigger block,
//! * [`FreeList::free_blocks`] and [`FreeList::largest_free`] report the bucket's
//!   contents.
//!
//! [`FreeList::push`]/[`FreeList::pop`] are the size-oblivious (equal-block) subset
//! the segregated/slab allocator path uses; they are implemented over the same
//! machinery.
//!
//! # In-block link layout
//!
//! A list node *is* the block's payload address (what the allocator hands to the
//! caller and what [`FreeList::push`]/[`FreeList::pop`] exchange): while a block is
//! free its payload holds the two intrusive link words and nothing else, so filing
//! and unfiling a block is **allocation-free**. The only allocation in this module is
//! the per-class slot vector, grown by [`FreeList::reserve`]:
//!
//! ```text
//! payload + 0  .. + 8    next  (*mut u8)   the next free block's payload, or null
//! payload + 8  .. + 16   prev  (*mut u8)   the previous free block's payload, or null
//! ```
//!
//! [`LINK_WORDS`] words are therefore the entire in-block overhead, which is why the
//! smallest block a free list can hold is [`MIN_FREE_BLOCK_SIZE`] (the block header
//! plus the two link words). A block's **size** is deliberately *not* stored in the
//! payload — there is no room for it — but in the block header directly below the
//! payload, at `payload - BLOCK_HEADER_SIZE`: this mirrors the legacy
//! `MBC_FBLK_SZ(blk)`, which reads the size out of `blk->bhdr`. [`FreeList::insert`]
//! records the size there (the legacy coalescing code updates `bhdr` before calling
//! `link_free_block`), [`FreeList::push`] reads it back and [`FreeList::block_size`]
//! exposes it.
//!
//! The geometry assumed throughout is the one `carrier.rs` writes and `allocator.rs`
//! reads: a block's header starts [`BLOCK_HEADER_SIZE`] bytes below its payload
//! (`payload_offset == 0` in [`crate::block::BlockHdr`] terms). A block with alignment
//! padding is a dedicated (large) block with no size class and never goes on a class
//! list.
//!
//! # Which block is taken
//!
//! [`FreeList::take_fit`] models the legacy good-fit search
//! (`erl_goodfit_alloc.c::search_bucket`, whose bounded search depth is
//! `max_blk_search`, by default 3): it walks at most [`MAX_FIT_SEARCH`] nodes from the
//! head of the class and takes the **smallest** block that is at least the requested
//! size, returning immediately on an exact-size match (the legacy `blk_sz == min_sz`
//! shortcut). That is a *good* fit rather than a best fit: when the smallest suitable
//! block lies beyond the search window, the best block inside the window is taken
//! instead. The window bounds the *cost*, never the *result*: the per-class largest
//! block record proves whether a fitting block exists at all, so a window holding no
//! candidate falls back to the first fitting block beyond it, and `take_fit` returns
//! `None` exactly when the class holds no block of the requested size.
//!
//! `unsafe` is confined to this module's marked functions; every `unsafe fn` carries a
//! `# Safety` section and every `unsafe` block a `// SAFETY:` rationale
//! (`beam_common::safety`).
//!
//! Sizes are always **total block sizes** (header + payload), the same unit as
//! [`crate::block::BlockHdr::size`] and the legacy `MBC_FBLK_SZ`.
//!
//! # Adoption
//!
//! [`FreeList::push`]/[`FreeList::pop`]/[`FreeList::reserve`] are what `carrier.rs`
//! and `allocator.rs` call today. The variable-size half —
//! [`FreeList::take_fit`] (the good-fit search) and [`FreeList::remove`] (unlink one
//! known block) — is what the variable-size multi-block-carrier sub-allocator
//! (`crate::mbc`) drives to split a request out of a big free block and to coalesce
//! physically adjacent free blocks. [`FreeList::refile`] is not used yet, so it alone
//! still carries an `#[allow(dead_code)]`.
//!
//! The correctness evidence for the variable-size machinery lives in the sibling `tests`
//! module (every `unsafe` block also carries its own `// SAFETY:` rationale, as the crate
//! requires).

use core::ptr;

use beam_common::{BeamError, Result};

use crate::block::{BlockHeader, BLOCK_HEADER_SIZE, MAX_BLOCK_SIZE, MAX_CLASS_COUNT, UNIT_SIZE};

/// Number of machine words a free block's payload reserves for its intrusive links
/// (`next` at word 0, `prev` at word 1).
pub const LINK_WORDS: usize = 2;

/// Size in bytes of one link word (`align_of::<Eterm>() == 8` on 64-bit).
const LINK_WORD_SIZE: usize = core::mem::size_of::<*mut u8>();

/// The smallest block a free list can hold: the block header plus both link words.
///
/// A smaller block cannot carry a `next`/`prev` pair, which is why the allocator
/// never carves one ([`crate::size_class::SizeClasses`] starts at `UNIT_SIZE` bytes of
/// *payload*, i.e. `UNIT_SIZE + BLOCK_HEADER_SIZE` per block).
pub const MIN_FREE_BLOCK_SIZE: usize = BLOCK_HEADER_SIZE + LINK_WORDS * LINK_WORD_SIZE;

// The link pair must fit the smallest block: header + two words, i.e. two units.
const _: () = assert!(MIN_FREE_BLOCK_SIZE == 2 * UNIT_SIZE);

/// The bounded search depth of [`FreeList::take_fit`]'s good-fit pass.
///
/// Mirrors the legacy good-fit allocator's default `max_blk_search` (`mbsd`) of 3:
/// insert and remove stay O(1) and the search is O(`MAX_FIT_SEARCH`) while still
/// finding a small-enough block in practice.
pub const MAX_FIT_SEARCH: usize = 3;

/// One class's bucket of free blocks of *any* size, as an intrusive doubly-linked list.
#[derive(Clone, Copy)]
struct ClassList {
    /// The payload address of the first free block (null when the bucket is empty).
    head: *mut u8,
    /// The number of free blocks in the bucket.
    count: usize,
    /// The largest block size in the bucket (0 when empty); maintained exactly.
    max_size: usize,
}

impl Default for ClassList {
    fn default() -> ClassList {
        ClassList {
            head: ptr::null_mut(),
            count: 0,
            max_size: 0,
        }
    }
}

/// The per-size-class set of free-block lists (the allocator's `buckets`).
#[derive(Default)]
pub struct FreeList {
    /// One bucket per size class, indexed by the class index.
    classes: Vec<ClassList>,
}

// SAFETY: the heads point into carrier memory owned by the allocator; the structure
// is only ever mutated through `&mut self` (in practice while holding the
// allocator's mutex), so moving it between threads is sound. `Sync` is deliberately
// not implemented.
unsafe impl Send for FreeList {}

impl FreeList {
    /// A new, empty free-list set.
    pub fn new() -> FreeList {
        FreeList::default()
    }

    /// The number of size classes tracked.
    pub fn class_count(&self) -> usize {
        self.classes.len()
    }

    /// The number of free blocks currently on `class`'s list (0 for an out-of-range
    /// class).
    pub fn free_blocks(&self, class: usize) -> usize {
        self.classes.get(class).map_or(0, |list| list.count)
    }

    /// The largest free block currently on `class`'s list, in bytes (0 when the class
    /// is empty or out of range).
    ///
    /// Kept **exactly**: filing a block is O(1), but unlinking the bucket's largest
    /// block rescans that bucket (O(blocks in the class)), so callers can rely on
    /// `largest_free(class) < size` meaning "this class cannot serve the request".
    pub fn largest_free(&self, class: usize) -> usize {
        self.classes.get(class).map_or(0, |list| list.max_size)
    }

    /// The number of class slots the per-class bookkeeping can hold before it has to
    /// grow.
    ///
    /// Diagnostic only (it makes the "filing and unfiling a block never allocates"
    /// property testable): besides [`FreeList::reserve`], no method in this module
    /// allocates, so this value can never change once a class has been reserved.
    #[cfg(test)]
    pub(crate) fn slot_capacity(&self) -> usize {
        self.classes.capacity()
    }

    /// Ensures `class` has room to track `count` more free blocks.
    ///
    /// An intrusive list needs no backing capacity, so this only enlarges the
    /// per-class bookkeeping to cover `class`; `count` is advisory (kept for the
    /// legacy `reserve`/growth surface) and is not stored. A `class` beyond the
    /// encodable range is rejected with [`BeamError::BadArg`].
    pub(crate) fn reserve(&mut self, class: usize, count: usize) -> Result<()> {
        if class >= MAX_CLASS_COUNT {
            return Err(BeamError::BadArg);
        }
        let _ = count;
        if class >= self.classes.len() {
            self.classes.resize(class + 1, ClassList::default());
        }
        Ok(())
    }

    /// The size recorded in the block header of the free block at `payload`.
    ///
    /// # Safety
    ///
    /// `payload` must be a payload of a block owned by this allocator whose header (at
    /// `payload - BLOCK_HEADER_SIZE`) was written by [`BlockHeader::store`].
    pub(crate) unsafe fn block_size(payload: *mut u8) -> usize {
        // SAFETY: the caller guarantees a header written by `BlockHeader::store` lives
        // `BLOCK_HEADER_SIZE` bytes below the payload.
        unsafe { BlockHeader::load(payload.sub(BLOCK_HEADER_SIZE)) }.size
    }

    /// Records `size` as the block size of the free block at `payload`, leaving the
    /// header's payload offset, class, carrier and flags untouched.
    ///
    /// This is the "the block's size changed" half of coalescing: a block that is
    /// currently *filed* must additionally be re-filed (see [`FreeList::refile`]) so
    /// that its class's bookkeeping stays exact.
    ///
    /// # Safety
    ///
    /// As for [`FreeList::block_size`]; `size` must be a `UNIT_SIZE`-granular block
    /// size no larger than [`MAX_BLOCK_SIZE`].
    pub(crate) unsafe fn set_size(payload: *mut u8, size: usize) {
        debug_assert_eq!(size % UNIT_SIZE, 0, "block size must be UNIT_SIZE-granular");
        debug_assert!(
            size <= MAX_BLOCK_SIZE,
            "block size exceeds the encodable range"
        );
        // SAFETY: pointer arithmetic on a payload the caller guarantees belongs to one
        // of this allocator's blocks, so the header is in bounds.
        let header = unsafe { payload.sub(BLOCK_HEADER_SIZE) };
        // SAFETY: the caller guarantees the header was written by `BlockHeader::store`,
        // so it is readable and rewritable in place.
        let mut hdr = unsafe { BlockHeader::load(header) };
        hdr.size = size;
        // SAFETY: as above — the header words are this block's own, and nothing else
        // aliases them (the block is free, so its payload holds only links).
        unsafe { BlockHeader::store(header, hdr) };
    }

    /// Inserts the free block `ptr` of `size` bytes at the head of `class`'s list,
    /// recording `size` in the block's header.
    ///
    /// Filing is O(1) and allocation-free. An out-of-range `class` (one that was never
    /// reserved) is a caller bug: it trips a debug assertion and is refused without
    /// modifying the list.
    ///
    /// # Safety
    ///
    /// `class` must be a reserved class index; `ptr` must be a `UNIT_SIZE`-aligned free
    /// block of at least [`MIN_FREE_BLOCK_SIZE`] `UNIT_SIZE`-granular bytes whose header
    /// (at `ptr - BLOCK_HEADER_SIZE`) was written by [`BlockHeader::store`], and must
    /// not be reachable from anywhere else.
    pub(crate) unsafe fn insert(&mut self, class: usize, ptr: *mut u8, size: usize) {
        debug_assert_eq!(
            ptr as usize % UNIT_SIZE,
            0,
            "free-list nodes must be UNIT_SIZE-aligned payloads"
        );
        debug_assert!(
            class < self.classes.len(),
            "the class slot must be reserved before inserting"
        );
        debug_assert!(
            size >= MIN_FREE_BLOCK_SIZE && size.is_multiple_of(UNIT_SIZE) && size <= MAX_BLOCK_SIZE,
            "a free block must be UNIT_SIZE-granular and hold both link words"
        );
        if class >= self.classes.len() {
            // Unreachable when the caller reserved the slot; dropping the block here
            // would leak it, so the debug assertion above is the real guard.
            return;
        }
        // SAFETY: per the contract `ptr` is a free block with a header written by
        // `BlockHeader::store`; recording the size there keeps the block's size
        // recoverable from the block itself, exactly as the legacy `bhdr` does.
        unsafe { FreeList::set_size(ptr, size) };
        // SAFETY: as above — `ptr` has room for both link words and is not reachable
        // from anywhere else, so linking it cannot clobber live data.
        unsafe { self.link_front(class, ptr, size) };
    }

    /// Pushes `ptr` onto `class`'s free list, using the block size recorded in the
    /// block header at `ptr - BLOCK_HEADER_SIZE` (the legacy `MBC_FBLK_SZ` model).
    ///
    /// # Safety
    ///
    /// `ptr` must point to a `UNIT_SIZE`-aligned free block of at least
    /// [`MIN_FREE_BLOCK_SIZE`] bytes whose header was written by [`BlockHeader::store`]
    /// with a `UNIT_SIZE`-granular `size`, and must not be otherwise reachable.
    pub(crate) unsafe fn push(&mut self, class: usize, ptr: *mut u8) {
        debug_assert_eq!(
            ptr as usize % UNIT_SIZE,
            0,
            "free-list nodes must be UNIT_SIZE-aligned payloads"
        );
        // SAFETY: per the contract `ptr` is a free block whose header was written by
        // `BlockHeader::store`, so its size is readable.
        let size = unsafe { FreeList::block_size(ptr) };
        // SAFETY: as above — `ptr` is a free block of `size` bytes with room for both
        // link words and is not reachable from anywhere else.
        unsafe { self.insert(class, ptr, size) };
    }

    /// Pops a free block off `class`'s list (the head, i.e. LIFO), or `None` when it is
    /// empty.
    ///
    /// Size-oblivious: the segregated (equal-block) allocator path uses this, while
    /// [`FreeList::take_fit`] is the size-aware one.
    ///
    /// # Safety
    ///
    /// `class` must be a valid class index; the returned block must not be otherwise
    /// reachable, and its intrusive link must be removed by this call.
    pub(crate) unsafe fn pop(&mut self, class: usize) -> Option<*mut u8> {
        let head = self.classes.get(class)?.head;
        if head.is_null() {
            return None;
        }
        // SAFETY: `head` is the first node of `class`'s list, so it is a free block with
        // an intact header and intact links; unlinking it leaves the rest of the list
        // consistent.
        unsafe { self.unlink(class, head) };
        Some(head)
    }

    /// Finds and unlinks a free block of **at least** `size` bytes on `class` and
    /// returns its payload (see the module docs for the good-fit policy).
    ///
    /// Returns `None` exactly when `class` holds no block of at least `size` bytes.
    ///
    /// # Safety
    ///
    /// `class` must be a valid class index and every block filed on it must be an
    /// intact free block (as [`FreeList::insert`]/[`FreeList::push`] require).
    pub(crate) unsafe fn take_fit(&mut self, class: usize, size: usize) -> Option<*mut u8> {
        let list = *self.classes.get(class)?;
        if list.head.is_null() || list.max_size < size {
            // Cheap reject: the exact largest-block record proves no fit exists.
            return None;
        }

        // Phase 1 — the bounded good-fit window (legacy `search_bucket`).
        let mut node = list.head;
        let mut scanned = 0usize;
        let mut best: *mut u8 = ptr::null_mut();
        let mut best_size = usize::MAX;
        while !node.is_null() && scanned < MAX_FIT_SEARCH {
            // SAFETY: `node` is a free block on this list, so its header size is
            // readable (and its link, below).
            let node_size = unsafe { FreeList::block_size(node) };
            if node_size >= size {
                if node_size == size {
                    // The ideal fit — the legacy `blk_sz == min_sz` shortcut.
                    // SAFETY: `node` is a node of `class`'s list.
                    unsafe { self.unlink(class, node) };
                    return Some(node);
                }
                if node_size < best_size {
                    best = node;
                    best_size = node_size;
                }
            }
            // SAFETY: `node` is a free block on this list, so its `next` link is
            // readable.
            node = unsafe { link_next(node) };
            scanned += 1;
        }
        if !best.is_null() {
            // SAFETY: `best` is a node of `class`'s list.
            unsafe { self.unlink(class, best) };
            return Some(best);
        }

        // Phase 2 — the window held no fitting block while `max_size` proves the class
        // has one, so the fit lies beyond the window: take the first one, continuing
        // from where phase 1 stopped. The search stays bounded whenever a fit is close
        // (the common case), and is never *negative* about a fit that exists.
        while !node.is_null() {
            // SAFETY: `node` is a free block on this list.
            if unsafe { FreeList::block_size(node) } >= size {
                // SAFETY: as above.
                unsafe { self.unlink(class, node) };
                return Some(node);
            }
            // SAFETY: `node` is a free block on this list.
            node = unsafe { link_next(node) };
        }
        debug_assert!(
            self.classes[class].max_size < size,
            "an exact `max_size` guarantees a fitting block on the list"
        );
        None
    }

    /// Removes the specific free block `payload` from `class`'s list.
    ///
    /// Returns `true` when the block was on the list and has been unlinked in O(1), and
    /// `false` — leaving the list untouched — when `class` is not a tracked list or
    /// `payload` is not a node of it. A removed block is left with null links, so
    /// removing it again is a harmless `false` instead of a corruption.
    ///
    /// # Safety
    ///
    /// `payload` must be either a free block currently on `class`'s list or a pointer
    /// that is not a node of any free list at all.
    pub(crate) unsafe fn remove(&mut self, class: usize, payload: *mut u8) -> bool {
        if payload.is_null() || class >= self.classes.len() {
            return false;
        }
        // Membership check for the one case that is detectable in O(1): a node with a
        // null `prev` can only be the list head.
        // SAFETY: `payload` is either a node of a free list (whose link words are ours
        // to read) or, per the contract, not a node at all — in which case the caller
        // has already accepted that its links are stale.
        let prev = unsafe { link_prev(payload) };
        if prev.is_null() && self.classes[class].head != payload {
            return false;
        }
        // SAFETY: `payload` is a node of `class`'s list.
        unsafe { self.unlink(class, payload) };
        true
    }

    /// Re-files a free block whose size changed (e.g. two adjacent free blocks were
    /// coalesced into one): unlinks it from `old_class`, records `new_size` in its
    /// header and links it into `new_class`.
    ///
    /// Returns `false`, having changed nothing, when `new_class` is not a tracked list
    /// or `payload` is not on `old_class`'s list. Both checks happen before the block is
    /// unlinked, so a refused call never loses a block.
    ///
    /// # Safety
    ///
    /// `payload` must be either a free block currently on `old_class`'s list or a
    /// pointer that is not a node of any free list at all; `new_size` must be a
    /// `UNIT_SIZE`-granular block size of at least [`MIN_FREE_BLOCK_SIZE`] bytes with
    /// `payload`'s block actually spanning it.
    #[allow(dead_code)] // not used yet (coalescing unlinks/links with remove/insert — see "Adoption")
    pub(crate) unsafe fn refile(
        &mut self,
        old_class: usize,
        new_class: usize,
        payload: *mut u8,
        new_size: usize,
    ) -> bool {
        debug_assert!(
            new_size >= MIN_FREE_BLOCK_SIZE
                && new_size.is_multiple_of(UNIT_SIZE)
                && new_size <= MAX_BLOCK_SIZE,
            "a re-filed block must hold both link words"
        );
        if new_class >= self.classes.len() {
            return false;
        }
        // SAFETY: the caller guarantees `payload` is a node of `old_class`'s list (or is
        // on no list at all, which `remove` reports as `false`).
        if !unsafe { self.remove(old_class, payload) } {
            return false;
        }
        // SAFETY: `payload` is a free block with a header written by
        // `BlockHeader::store`, and it is currently on no list.
        unsafe { FreeList::set_size(payload, new_size) };
        // SAFETY: `new_class` is a reserved slot (checked above) and `payload` is a free
        // block of `new_size` bytes that is not reachable from anywhere else.
        unsafe { self.link_front(new_class, payload, new_size) };
        true
    }

    /// Links `node` at the head of `class`'s list with `size` as its block size,
    /// updating the bucket's count and largest-block record. O(1), allocation-free.
    ///
    /// # Safety
    ///
    /// `class` must be a reserved class index and `node` a free block of `size` bytes
    /// (at least [`MIN_FREE_BLOCK_SIZE`]) that is not reachable from anywhere else.
    unsafe fn link_front(&mut self, class: usize, node: *mut u8, size: usize) {
        let head = self.classes[class].head;
        // SAFETY: `node` holds both link words (>= `MIN_FREE_BLOCK_SIZE` bytes) and is
        // not reachable from anywhere else, so writing them cannot clobber live data.
        unsafe {
            store_link_prev(node, ptr::null_mut());
            store_link_next(node, head);
        }
        if !head.is_null() {
            // SAFETY: `head` is the former first node of this list, so its `prev` link is
            // writable.
            unsafe { store_link_prev(head, node) };
        }
        let list = &mut self.classes[class];
        list.head = node;
        list.count += 1;
        if size > list.max_size {
            list.max_size = size;
        }
    }

    /// Splices `node` out of `class`'s doubly-linked list, leaving its own links null,
    /// and updates the bucket's count and largest-block record. O(1) plus, at worst, a
    /// rescan when the largest block left the bucket.
    ///
    /// # Safety
    ///
    /// `class` must be a reserved class index and `node` a block currently on that
    /// list, with its header written by [`BlockHeader::store`].
    unsafe fn unlink(&mut self, class: usize, node: *mut u8) {
        // SAFETY: `node` is a free block of the list, so its header and both link words
        // are readable.
        let size = unsafe { FreeList::block_size(node) };
        // SAFETY: as above.
        let (next, prev) = unsafe { (link_next(node), link_prev(node)) };
        if prev.is_null() {
            debug_assert_eq!(
                self.classes[class].head, node,
                "a null `prev` marks the head of the list"
            );
            self.classes[class].head = next;
        } else {
            // SAFETY: `prev` is a node of the same list, so its `next` link is writable.
            unsafe { store_link_next(prev, next) };
        }
        if !next.is_null() {
            // SAFETY: `next` is a node of the same list, so its `prev` link is writable.
            unsafe { store_link_prev(next, prev) };
        }
        // SAFETY: `node` is no longer on any list, so clearing its links is a write to
        // memory this module owns; it makes a repeated `remove` detectable.
        unsafe {
            store_link_next(node, ptr::null_mut());
            store_link_prev(node, ptr::null_mut());
        }

        let count = self.classes[class].count.saturating_sub(1);
        self.classes[class].count = count;
        if count == 0 {
            self.classes[class].max_size = 0;
        } else if size >= self.classes[class].max_size {
            // The block that just left may have been the largest one: rescan so the
            // record stays exact.
            // SAFETY: every remaining node of `class`'s list is an intact free block.
            let max = unsafe { self.scan_max(class) };
            self.classes[class].max_size = max;
        }
    }

    /// The largest block size on `class`'s list by walking it (0 when empty).
    ///
    /// # Safety
    ///
    /// `class` must be a reserved class index and every node of its list an intact free
    /// block with a header written by [`BlockHeader::store`].
    unsafe fn scan_max(&self, class: usize) -> usize {
        let mut max = 0;
        let mut node = self.classes[class].head;
        while !node.is_null() {
            // SAFETY: `node` is a free block on this list.
            let size = unsafe { FreeList::block_size(node) };
            if size > max {
                max = size;
            }
            // SAFETY: as above.
            node = unsafe { link_next(node) };
        }
        max
    }
}

/// Reads a free block's `next` link (its payload's first word).
///
/// # Safety
///
/// `node` must be a payload of at least [`LINK_WORDS`] words that this module owns (a
/// free block on a list, or one being filed).
unsafe fn link_next(node: *mut u8) -> *mut u8 {
    // SAFETY: the caller guarantees `node`'s payload holds the intrusive links, so its
    // first word is a readable, aligned `*mut u8`.
    unsafe { node.cast::<*mut u8>().read() }
}

/// Writes a free block's `next` link (its payload's first word).
///
/// # Safety
///
/// As for [`link_next`]; the write must not clobber live data (the block must be free).
unsafe fn store_link_next(node: *mut u8, next: *mut u8) {
    // SAFETY: as for `link_next` — the first payload word is the `next` link.
    unsafe { node.cast::<*mut u8>().write(next) };
}

/// Reads a free block's `prev` link (its payload's second word).
///
/// # Safety
///
/// `node` must be a payload of at least [`LINK_WORDS`] words that this module owns (a
/// free block on a list, or one being filed).
unsafe fn link_prev(node: *mut u8) -> *mut u8 {
    // SAFETY: the caller guarantees `node`'s payload holds the intrusive links, so its
    // second word is a readable, aligned `*mut u8`.
    unsafe { node.cast::<*mut u8>().add(1).read() }
}

/// Writes a free block's `prev` link (its payload's second word).
///
/// # Safety
///
/// As for [`link_prev`]; the write must not clobber live data (the block must be free).
unsafe fn store_link_prev(node: *mut u8, prev: *mut u8) {
    // SAFETY: as for `link_prev` — the second payload word is the `prev` link.
    unsafe { node.cast::<*mut u8>().add(1).write(prev) };
}

#[cfg(test)]
mod tests;
