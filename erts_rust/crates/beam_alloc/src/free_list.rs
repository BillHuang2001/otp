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
//! and `allocator.rs` call today. The variable-size half
//! ([`FreeList::take_fit`]/[`FreeList::remove`]/[`FreeList::refile`]) is the seam the
//! free-block coalescing pass adopts, so until that lands it carries a
//! `#[allow(dead_code)]` (the API is deliberately in place and fully tested here).
//!
//! This file runs a little past the crate's ~1000-line guideline: the implementation
//! is ~620 lines and the rest is the test module that *is* the correctness evidence
//! for the variable-size machinery (every `unsafe` block also carries its own
//! `// SAFETY:` rationale, as the crate requires).

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
            size >= MIN_FREE_BLOCK_SIZE && size % UNIT_SIZE == 0 && size <= MAX_BLOCK_SIZE,
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
    #[allow(dead_code)] // adopted by the allocator's coalescing pass (see "Adoption")
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
    #[allow(dead_code)] // adopted by the allocator's coalescing pass (see "Adoption")
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
    #[allow(dead_code)] // adopted by the allocator's coalescing pass (see "Adoption")
    pub(crate) unsafe fn refile(
        &mut self,
        old_class: usize,
        new_class: usize,
        payload: *mut u8,
        new_size: usize,
    ) -> bool {
        debug_assert!(
            new_size >= MIN_FREE_BLOCK_SIZE
                && new_size % UNIT_SIZE == 0
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
mod tests {
    use super::*;

    use crate::block::{BlockFlags, BlockHdr};

    /// A `UNIT_SIZE`-aligned byte arena standing in for a carrier's mapping, carved
    /// into blocks exactly the way `Carrier::subdivide_into` does it: a header at the
    /// block start and the payload `BLOCK_HEADER_SIZE` bytes later.
    #[repr(align(16))]
    struct Mem([u64; 256]); // 2048 bytes

    struct Carrier {
        mem: Box<Mem>,
    }

    impl Carrier {
        fn new() -> Carrier {
            Carrier {
                mem: Box::new(Mem([0u64; 256])),
            }
        }

        fn base(&mut self) -> *mut u8 {
            self.mem.0.as_mut_ptr().cast::<u8>()
        }

        /// Writes a block header of `size` bytes at `block_start` (a byte offset from
        /// the arena base) and returns that block's payload address.
        fn block(&mut self, block_start: usize, size: usize) -> *mut u8 {
            let base = self.base();
            let hdr = BlockHdr {
                size,
                payload_offset: 0,
                class: 0,
                carrier: ptr::null_mut(),
                flags: BlockFlags::empty().bits(),
            };
            assert!(
                block_start + size <= 2048,
                "the arena must hold every block"
            );
            // SAFETY: `block_start + BLOCK_HEADER_SIZE` lies inside the 2048-byte arena
            // for every call in these tests, the arena is 16-aligned and nothing else
            // aliases it.
            unsafe { BlockHeader::store(base.add(block_start), hdr) };
            // SAFETY: the payload starts inside the arena, right after the header.
            unsafe { base.add(block_start + BLOCK_HEADER_SIZE) }
        }
    }

    #[test]
    fn new_list_is_empty_and_grows_on_reserve() {
        let mut lists = FreeList::new();
        assert_eq!(lists.class_count(), 0);
        assert_eq!(lists.free_blocks(0), 0);
        lists.reserve(2, 1).expect("class 2 is in range");
        assert_eq!(lists.class_count(), 3);
        for class in 0..3 {
            assert_eq!(lists.free_blocks(class), 0);
            assert_eq!(lists.largest_free(class), 0);
        }
        // SAFETY: no node was pushed, so popping an empty class yields `None`.
        assert!(unsafe { lists.pop(0) }.is_none());
        // An out-of-range class has no list at all.
        // SAFETY: `pop` checks the class index before touching any node.
        assert!(unsafe { lists.pop(usize::MAX) }.is_none());
    }

    #[test]
    fn reserve_rejects_an_unencodable_class() {
        let mut lists = FreeList::new();
        assert!(matches!(
            lists.reserve(MAX_CLASS_COUNT, 1),
            Err(BeamError::BadArg)
        ));
        assert!(lists.reserve(MAX_CLASS_COUNT - 1, 1).is_ok());
    }

    #[test]
    fn push_pop_is_lifo_and_tracks_counts() {
        let mut carrier = Carrier::new();
        let ptrs: Vec<*mut u8> = (0..4)
            .map(|index| carrier.block(index * MIN_FREE_BLOCK_SIZE, MIN_FREE_BLOCK_SIZE))
            .collect();
        let mut lists = FreeList::new();
        lists.reserve(0, 4).expect("class 0");
        for &ptr in &ptrs {
            // SAFETY: each payload is a distinct, 16-aligned free block of 32 bytes
            // whose header records its size.
            unsafe { lists.push(0, ptr) };
        }
        assert_eq!(lists.free_blocks(0), 4);
        assert_eq!(lists.largest_free(0), MIN_FREE_BLOCK_SIZE);
        for &expected in ptrs.iter().rev() {
            // SAFETY: the nodes are on class 0's list and each is popped once.
            let popped = unsafe { lists.pop(0) }.expect("non-empty");
            assert_eq!(popped, expected, "LIFO order");
        }
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.largest_free(0), 0);
        // SAFETY: the list is empty.
        assert!(unsafe { lists.pop(0) }.is_none());
    }

    #[test]
    fn classes_are_independent() {
        let mut carrier = Carrier::new();
        let (a0, a1) = (carrier.block(0, 32), carrier.block(32, 32));
        let (b0, b1) = (carrier.block(64, 32), carrier.block(96, 32));
        let mut lists = FreeList::new();
        lists.reserve(1, 2).expect("class 1");
        // SAFETY: distinct, 16-aligned free blocks with real headers, per class.
        unsafe {
            lists.push(0, a0);
            lists.push(0, a1);
            lists.push(1, b0);
            lists.push(1, b1);
        }
        assert_eq!(lists.free_blocks(0), 2);
        assert_eq!(lists.free_blocks(1), 2);
        assert_eq!(lists.free_blocks(2), 0);
        // SAFETY: class 1's list is non-empty.
        assert_eq!(unsafe { lists.pop(1) }, Some(b1));
        assert_eq!(lists.free_blocks(0), 2, "class 0 is untouched");
        assert_eq!(lists.free_blocks(1), 1);
        // SAFETY: both lists are non-empty.
        assert_eq!(unsafe { lists.pop(0) }, Some(a1));
        assert_eq!(unsafe { lists.pop(1) }, Some(b0));
        assert_eq!(unsafe { lists.pop(0) }, Some(a0));
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.free_blocks(1), 0);
    }

    #[test]
    fn pop_then_push_reuses_the_same_node() {
        let mut carrier = Carrier::new();
        let ptrs = [carrier.block(0, 32), carrier.block(32, 32)];
        let mut lists = FreeList::new();
        lists.reserve(0, 2).expect("class 0");
        // SAFETY: two distinct 16-aligned free blocks with real headers.
        unsafe {
            lists.push(0, ptrs[0]);
            lists.push(0, ptrs[1]);
        }
        // SAFETY: the list is non-empty.
        let first = unsafe { lists.pop(0) }.expect("non-empty");
        // SAFETY: `first` was just removed from the list and is free.
        unsafe { lists.push(0, first) };
        assert_eq!(lists.free_blocks(0), 2);
        // SAFETY: the list is non-empty, and popping twice empties it.
        assert_eq!(unsafe { lists.pop(0) }, Some(first));
        assert_eq!(unsafe { lists.pop(0) }, Some(ptrs[0]));
        assert_eq!(lists.free_blocks(0), 0);
    }

    #[test]
    fn push_reads_the_size_out_of_the_block_header() {
        let mut carrier = Carrier::new();
        let small = carrier.block(0, 32);
        let big = carrier.block(32, 64);
        let mut lists = FreeList::new();
        lists.reserve(0, 2).expect("class 0");
        // SAFETY: both payloads are free blocks whose headers record 32 and 64 bytes.
        unsafe {
            lists.push(0, small);
            lists.push(0, big);
        }
        assert_eq!(lists.largest_free(0), 64, "the header size is what counts");
        assert_eq!(lists.free_blocks(0), 2);
        // SAFETY: both nodes are on class 0's list.
        assert_eq!(unsafe { FreeList::block_size(big) }, 64);
        // A 48-byte request can only be served by the 64-byte block.
        // SAFETY: class 0 is reserved and its nodes are intact free blocks.
        assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(big));
        assert_eq!(lists.free_blocks(0), 1);
    }

    #[test]
    fn insert_records_the_block_size_in_the_header() {
        let mut carrier = Carrier::new();
        // The header starts out claiming 32 bytes ...
        let ptr = carrier.block(0, 32);
        let mut lists = FreeList::new();
        lists.reserve(0, 1).expect("class 0");
        // SAFETY: `ptr` is a free block with a header written by `BlockHeader::store`.
        unsafe { lists.insert(0, ptr, 80) };
        // ... but inserting it with an explicit size makes that size the recorded one.
        // SAFETY: `ptr` is still a free block on class 0's list.
        assert_eq!(unsafe { FreeList::block_size(ptr) }, 80);
        assert_eq!(lists.largest_free(0), 80);
        assert_eq!(lists.free_blocks(0), 1);
        // SAFETY: as above.
        unsafe { FreeList::set_size(ptr, 96) };
        // SAFETY: as above.
        assert_eq!(unsafe { FreeList::block_size(ptr) }, 96);
    }

    #[test]
    fn take_fit_picks_the_best_fit_within_the_search_window() {
        let mut carrier = Carrier::new();
        let p32 = carrier.block(0, 32);
        let p48 = carrier.block(32, 48);
        let p64 = carrier.block(80, 64);
        let p80 = carrier.block(144, 80);
        let mut lists = FreeList::new();
        lists.reserve(0, 4).expect("class 0");
        // SAFETY: four distinct free blocks of 32/48/64/80 bytes with real headers.
        unsafe {
            lists.insert(0, p32, 32);
            lists.insert(0, p48, 48);
            lists.insert(0, p64, 64);
            lists.insert(0, p80, 80);
        }
        // List order (head first) is now 80, 64, 48, 32.
        assert_eq!(lists.free_blocks(0), 4);
        assert_eq!(lists.largest_free(0), 80);

        // An exact match wins immediately.
        // SAFETY: class 0 is reserved and its nodes are intact free blocks.
        assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(p48));
        // 32 is too small, so the *smallest* remaining block that fits (64, not 80) is
        // the good fit.
        // SAFETY: as above.
        assert_eq!(unsafe { lists.take_fit(0, 40) }, Some(p64));
        // SAFETY: as above.
        assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p32));
        assert_eq!(lists.free_blocks(0), 1);
        assert_eq!(lists.largest_free(0), 80);
        // Nothing left can serve a request bigger than the largest block.
        // SAFETY: as above.
        assert!(unsafe { lists.take_fit(0, 96) }.is_none());
        assert_eq!(lists.free_blocks(0), 1, "a rejected search takes nothing");
        // ... and the last block is still reachable by a smaller request.
        // SAFETY: as above.
        assert_eq!(unsafe { lists.take_fit(0, 33) }, Some(p80));
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.largest_free(0), 0);
    }

    #[test]
    fn take_fit_falls_back_to_the_first_fit_beyond_the_search_window() {
        let mut carrier = Carrier::new();
        let p48 = carrier.block(0, 48);
        let p32a = carrier.block(48, 32);
        let p32b = carrier.block(80, 32);
        let p32c = carrier.block(112, 32);
        let mut lists = FreeList::new();
        lists.reserve(0, 4).expect("class 0");
        // Filing the 48-byte block *first* puts it beyond the three-node search window:
        // list order (head first) is 32, 32, 32, 48.
        // SAFETY: four distinct free blocks of 48/32/32/32 bytes with real headers.
        unsafe {
            lists.insert(0, p48, 48);
            lists.insert(0, p32a, 32);
            lists.insert(0, p32b, 32);
            lists.insert(0, p32c, 32);
        }
        // The window holds only 32-byte blocks, which cannot serve 40 bytes; the exact
        // largest-block record proves the 48-byte block exists, so the search continues
        // and finds it (first fit beyond the window) rather than reporting "no fit".
        // SAFETY: class 0 is reserved and its nodes are intact free blocks.
        assert_eq!(unsafe { lists.take_fit(0, 40) }, Some(p48));
        assert_eq!(lists.free_blocks(0), 3);
        assert_eq!(lists.largest_free(0), 32);
        // SAFETY: as above.
        assert!(
            unsafe { lists.take_fit(0, 40) }.is_none(),
            "only 32s remain"
        );
        assert_eq!(lists.free_blocks(0), 3);
        // SAFETY: as above.
        assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p32c));
    }

    #[test]
    fn remove_unlinks_a_specific_block() {
        let mut carrier = Carrier::new();
        let p32 = carrier.block(0, 32);
        let p48 = carrier.block(32, 48);
        let p64 = carrier.block(80, 64);
        let p80 = carrier.block(144, 80);
        let mut lists = FreeList::new();
        lists.reserve(0, 4).expect("class 0");
        // SAFETY: four distinct free blocks with real headers (list order 80/64/48/32).
        unsafe {
            lists.insert(0, p32, 32);
            lists.insert(0, p48, 48);
            lists.insert(0, p64, 64);
            lists.insert(0, p80, 80);
        }
        // Remove the block in the middle of the list.
        // SAFETY: every block of class 0's list is intact.
        assert!(unsafe { lists.remove(0, p48) });
        assert_eq!(lists.free_blocks(0), 3);
        assert_eq!(lists.largest_free(0), 80);
        // Removing it again is a no-op (an unlinked node has null links).
        // SAFETY: `p48` is on no free list at all now.
        assert!(!unsafe { lists.remove(0, p48) });
        assert_eq!(lists.free_blocks(0), 3);
        // The survivors are exactly the other three, and the removed one is not among
        // them (it would have been preferred by the exact-size search).
        // SAFETY: class 0's remaining nodes are intact free blocks.
        assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(p64));
        // SAFETY: as above.
        assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p32));
        // SAFETY: as above.
        assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(p80));
        assert_eq!(lists.free_blocks(0), 0);
    }

    #[test]
    fn remove_rejects_a_block_that_is_not_on_the_class() {
        let mut carrier = Carrier::new();
        let a = carrier.block(0, 32);
        let b = carrier.block(32, 32);
        let mut lists = FreeList::new();
        lists.reserve(1, 2).expect("classes 0 and 1");
        // SAFETY: two distinct free blocks with real headers.
        unsafe {
            lists.insert(0, a, 32);
            lists.insert(1, b, 32);
        }
        // `a` is the head of class 0, so asking class 1 to remove it must be refused
        // (its null `prev` proves it is not a node of class 1's list).
        // SAFETY: `a` is not a node of class 1's list.
        assert!(!unsafe { lists.remove(1, a) });
        assert_eq!(lists.free_blocks(0), 1, "class 0 is untouched");
        assert_eq!(lists.free_blocks(1), 1, "class 1 is untouched");
        // SAFETY: a null node and an untracked class are both refused without touching
        // memory.
        assert!(!unsafe { lists.remove(0, ptr::null_mut()) });
        // SAFETY: class 99 was never reserved.
        assert!(!unsafe { lists.remove(99, a) });
        // The real removals still work.
        // SAFETY: both nodes are on their lists.
        assert!(unsafe { lists.remove(0, a) });
        assert!(unsafe { lists.remove(1, b) });
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.free_blocks(1), 0);
    }

    #[test]
    fn coalescing_two_adjacent_blocks_lets_a_bigger_request_fit() {
        let mut carrier = Carrier::new();
        // Two physically adjacent 32-byte blocks: `a` spans [0, 32) and `b` [32, 64).
        let a = carrier.block(0, 32);
        let b = carrier.block(32, 32);
        let mut lists = FreeList::new();
        lists.reserve(0, 2).expect("class 0");
        // SAFETY: two adjacent free blocks with real headers.
        unsafe {
            lists.insert(0, a, 32);
            lists.insert(0, b, 32);
        }
        assert_eq!(lists.free_blocks(0), 2);
        assert_eq!(lists.largest_free(0), 32);
        // Neither 32-byte block can satisfy a 48-byte request ...
        // SAFETY: class 0's nodes are intact free blocks.
        assert!(unsafe { lists.take_fit(0, 48) }.is_none());
        assert_eq!(lists.free_blocks(0), 2, "a rejected search takes nothing");

        // ... so coalesce them: `b` is absorbed by `a`, growing it to the 64 bytes the
        // pair spans, and the bigger block is re-filed with its new size.
        // SAFETY: `b` is a node of class 0's list.
        assert!(unsafe { lists.remove(0, b) });
        // SAFETY: `a` is a node of class 0's list and now spans 64 bytes.
        assert!(unsafe { lists.refile(0, 0, a, 64) });
        // SAFETY: `a` is still a free block.
        assert_eq!(unsafe { FreeList::block_size(a) }, 64);
        assert_eq!(lists.free_blocks(0), 1, "the two blocks became one");
        assert_eq!(lists.largest_free(0), 64);

        // The merged block can serve the request the two separate blocks could not.
        // SAFETY: class 0's remaining node is an intact 64-byte free block.
        assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(a));
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.largest_free(0), 0);
    }

    #[test]
    fn refile_moves_a_block_between_classes() {
        let mut carrier = Carrier::new();
        let a = carrier.block(0, 64);
        let mut lists = FreeList::new();
        lists.reserve(1, 1).expect("classes 0 and 1");
        // SAFETY: `a` is a free block with a real header.
        unsafe { lists.insert(0, a, 32) };
        assert_eq!(lists.largest_free(0), 32);
        // A coalesced block that outgrew its old class moves to the bigger one.
        // SAFETY: `a` is a node of class 0's list and now spans 64 bytes.
        assert!(unsafe { lists.refile(0, 1, a, 64) });
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.largest_free(0), 0);
        assert_eq!(lists.free_blocks(1), 1);
        assert_eq!(lists.largest_free(1), 64);
        // SAFETY: class 1's node is an intact 64-byte free block.
        assert_eq!(unsafe { lists.take_fit(1, 64) }, Some(a));
        assert_eq!(lists.free_blocks(1), 0);

        // A re-file into an untracked class is refused without losing the block.
        // SAFETY: `a` is a free block on no list at all now.
        unsafe { lists.insert(0, a, 32) };
        // SAFETY: as above (`a` is a node of class 0's list).
        assert!(!unsafe { lists.refile(0, 99, a, 64) });
        assert_eq!(lists.free_blocks(0), 1, "the block stayed where it was");
        // SAFETY: `a` is still filed in class 0.
        assert_eq!(unsafe { FreeList::block_size(a) }, 32);
        // A re-file of a block that is not on the source class is refused too.
        // SAFETY: `a` is now on no list at all.
        assert!(unsafe { lists.remove(0, a) });
        // SAFETY: as above.
        assert!(!unsafe { lists.refile(0, 1, a, 64) });
        assert_eq!(lists.free_blocks(0), 0);
        assert_eq!(lists.free_blocks(1), 0);
    }

    #[test]
    fn largest_free_is_exact_across_inserts_and_removals() {
        let mut carrier = Carrier::new();
        let p32 = carrier.block(0, 32);
        let p64 = carrier.block(32, 64);
        let p80 = carrier.block(96, 80);
        let mut lists = FreeList::new();
        lists.reserve(0, 3).expect("class 0");
        // SAFETY: three distinct free blocks with real headers.
        unsafe {
            lists.insert(0, p32, 32);
            lists.insert(0, p64, 64);
            lists.insert(0, p80, 80);
        }
        assert_eq!(lists.largest_free(0), 80);
        // Removing the largest block forces a rescan: the record must follow.
        // SAFETY: every node of class 0's list is intact.
        assert!(unsafe { lists.remove(0, p80) });
        assert_eq!(lists.largest_free(0), 64);
        // SAFETY: as above.
        assert!(unsafe { lists.remove(0, p64) });
        assert_eq!(lists.largest_free(0), 32);
        // SAFETY: as above.
        assert!(unsafe { lists.remove(0, p32) });
        assert_eq!(lists.largest_free(0), 0);
        // An untracked class reports nothing rather than guessing.
        assert_eq!(lists.largest_free(7), 0);
        assert_eq!(lists.free_blocks(7), 0);
    }

    #[test]
    fn empty_and_untracked_classes_report_zero_and_serve_nothing() {
        let mut lists = FreeList::new();
        lists.reserve(1, 1).expect("class 1");
        assert_eq!(lists.free_blocks(1), 0);
        assert_eq!(lists.largest_free(1), 0);
        assert_eq!(lists.free_blocks(7), 0, "class 7 was never reserved");
        assert_eq!(lists.largest_free(7), 0);
        // SAFETY: an empty class and an untracked class both yield `None` without
        // touching memory.
        assert!(unsafe { lists.take_fit(1, MIN_FREE_BLOCK_SIZE) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { lists.take_fit(7, MIN_FREE_BLOCK_SIZE) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { lists.take_fit(1, usize::MAX) }.is_none());
        // SAFETY: as above.
        assert!(unsafe { lists.pop(1) }.is_none());
    }

    #[test]
    fn filing_and_unfiling_never_grows_the_bookkeeping() {
        // `FreeList`'s only heap allocation is the per-class slot vector, and `reserve`
        // is the only method that touches it. Filing (`insert`/`link_front`), unfiling
        // (`unlink`/`remove`/`take_fit`/`pop`) and re-filing (`refile`) write only into
        // the free blocks' own payloads and headers, so once the class is reserved the
        // slot vector can neither grow nor be reallocated.
        let mut carrier = Carrier::new();
        let small = carrier.block(0, 32);
        let big = carrier.block(32, 64);
        let mut lists = FreeList::new();
        lists.reserve(0, 2).expect("class 0");
        let classes = lists.class_count();
        let capacity = lists.slot_capacity();

        for _ in 0..256 {
            // SAFETY: both payloads are free blocks with real headers.
            unsafe {
                lists.insert(0, small, 32);
                lists.insert(0, big, 64);
            }
            assert_eq!(lists.free_blocks(0), 2);
            assert_eq!(lists.largest_free(0), 64);
            // SAFETY: both nodes are on class 0's list.
            assert_eq!(unsafe { lists.take_fit(0, 48) }, Some(big));
            // SAFETY: as above.
            assert_eq!(unsafe { lists.take_fit(0, 32) }, Some(small));
            assert_eq!(lists.free_blocks(0), 0);
            assert_eq!(lists.largest_free(0), 0);
        }

        assert_eq!(lists.class_count(), classes);
        assert_eq!(
            lists.slot_capacity(),
            capacity,
            "the per-class bookkeeping must not have been reallocated"
        );
    }
}
