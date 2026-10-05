//! The allocator seam: the [`Allocator`] trait, its shared handle [`AllocRef`], the
//! [`AllocatorConfig`] builder and the concrete [`CarrierAllocator`].
//!
//! This is the **frozen cross-crate contract** of the crate. Dependents
//! (`beam_codec`, `beam_ets`, `beam_gc`, `beam_message`, `beam_process`) allocate
//! through an [`AllocRef`] and never touch carriers, blocks or free lists directly,
//! so the carrier framework can evolve behind this interface.
//!
//! # The carrier model (Milestone 1)
//!
//! All mutation is serialised through one [`parking_lot::Mutex`]. Carriers come from
//! the OS (`mmap`) and are page-aligned; there are two allocation paths:
//!
//! * **Small / slab.** A request at or below [`SizeClasses::large_threshold`] whose
//!   alignment is at most [`MIN_ALIGN`] is served from its size class. Each
//!   multi-block carrier is dedicated to one class, subdivided into equal blocks
//!   (each preceded by a [`crate::block::BlockHdr`]) and threaded on that class's
//!   intrusive free list. When a class's list runs dry a new carrier is mapped;
//!   drained carriers are retained (a documented M1 retention policy).
//! * **Large / dedicated.** Anything larger (or more aligned) gets a dedicated
//!   single-block carrier that is `munmap`ed again when the block is freed.
//!
//! Because carriers are page-aligned and alignments above the page size are
//! rejected, a block's alignment padding is a function of the requested alignment
//! alone, so `dealloc`/`realloc` can locate a block's header from the caller's
//! layout. Header errors are *never* trusted blindly: a header that does not look
//! like one this allocator wrote is reported and left alone rather than freed.
//!
//! This is the crate's largest file by design: it *is* the frozen seam (the trait,
//! the config builder and the one concrete implementation) and it carries a
//! comprehensive in-module test module, because the locator and the header
//! plausibility check are only exercisable with an allocator in hand.
//!
//! `unsafe` is confined to this module's marked functions.

use core::alloc::Layout;
use core::ffi::c_void;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Arc;

use beam_common::Result;
use parking_lot::Mutex;

use crate::block::{
    BlockFlags, BlockHdr, BlockHeader, ALL_FLAGS, BLOCK_HEADER_SIZE, CLASS_NONE, MAX_BLOCK_SIZE,
    MAX_CLASS_COUNT, UNIT_SIZE,
};
use crate::carrier::{os_page_size, round_up_unit, Carrier};
use crate::error;
use crate::free_list::FreeList;
use crate::size_class::SizeClasses;
use crate::stats::{AllocStats, Counters};
use crate::strategy::Strategy;

/// Byte alignment of a [`beam_term::prelude::Eterm`] word
/// (`align_of::<Eterm>()`).
pub const WORD_ALIGN: usize = 8;

/// The minimum alignment guaranteed for every allocation (`UNIT_SIZE`).
pub const MIN_ALIGN: usize = UNIT_SIZE;

/// A cheaply-clonable shared handle to an [`Allocator`].
///
/// Dependents hold this (not a borrow) so an allocator can outlive the call that
/// created it and be shared across threads without a lifetime parameter on every
/// structure.
pub type AllocRef = Arc<dyn Allocator>;

/// The allocator interface every allocation site in the workspace uses.
///
/// Implementors must be `Send + Sync + 'static` so the handle can be shared. The
/// trait is object-safe (no generic methods), so it is used as `dyn Allocator`
/// behind [`AllocRef`]. Failure is return-based: an allocator returns
/// `BeamError::Alloc(..)` on OOM and never panics.
pub trait Allocator: Send + Sync + 'static {
    /// Allocates `layout.size()` bytes aligned to `layout.align()`.
    fn alloc(&self, layout: Layout) -> Result<NonNull<u8>>;

    /// Grows or shrinks an allocation to `new_size` bytes, preserving its contents up
    /// to the smaller of the old and new sizes.
    fn realloc(&self, ptr: NonNull<u8>, old_layout: Layout, new_size: usize)
        -> Result<NonNull<u8>>;

    /// Frees an allocation previously returned by [`alloc`](Allocator::alloc) or
    /// [`realloc`](Allocator::realloc) on *this* allocator.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by `alloc`/`realloc` on this allocator, must not
    /// be used afterwards, and `layout` must be the layout it was allocated with.
    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout);

    /// A point-in-time snapshot of this allocator's counters.
    fn stats(&self) -> AllocStats;

    /// The human-readable allocator name (for logs / `system_info`).
    fn name(&self) -> &str;

    /// Allocates `size` bytes with the default alignment ([`MIN_ALIGN`]).
    fn alloc_bytes(&self, size: usize) -> Result<NonNull<u8>> {
        let layout =
            Layout::from_size_align(size, MIN_ALIGN).map_err(|_| error::bad_align(MIN_ALIGN))?;
        self.alloc(layout)
    }

    /// Allocates `layout` and zeroes the whole allocation.
    fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<u8>> {
        let ptr = self.alloc(layout)?;
        // SAFETY: `alloc` returned a block of at least `layout.size()` bytes that is
        // exclusively ours until it is handed back to `dealloc`, so writing zeros is
        // in-bounds and non-aliasing.
        unsafe { std::ptr::write_bytes(ptr.as_ptr(), 0, layout.size()) };
        Ok(ptr)
    }
}

/// Configuration for a [`CarrierAllocator`].
///
/// Build one with [`AllocatorConfig::builder`] or take the defaults via
/// [`AllocatorConfig::default`].
#[derive(Clone, Debug)]
pub struct AllocatorConfig {
    name: String,
    strategy: Strategy,
    carrier_size: usize,
    classes: SizeClasses,
    max_footprint: Option<usize>,
    zeroed_carriers: bool,
}

impl AllocatorConfig {
    /// The default carrier size (1 MiB).
    pub const DEFAULT_CARRIER_SIZE: usize = 1024 * 1024;

    /// Starts building a configuration from the defaults.
    pub fn builder() -> AllocatorConfigBuilder {
        AllocatorConfigBuilder {
            config: AllocatorConfig::default(),
        }
    }

    /// The allocator name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The selected allocation strategy.
    pub fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// The carrier size in bytes.
    pub fn carrier_size(&self) -> usize {
        self.carrier_size
    }

    /// The size-class table.
    pub fn classes(&self) -> &SizeClasses {
        &self.classes
    }

    /// The optional total-footprint cap in bytes (used by tests).
    pub fn max_footprint(&self) -> Option<usize> {
        self.max_footprint
    }

    /// Whether freshly mapped carriers are handed out zeroed.
    pub fn zeroed_carriers(&self) -> bool {
        self.zeroed_carriers
    }
}

impl Default for AllocatorConfig {
    fn default() -> AllocatorConfig {
        AllocatorConfig {
            name: String::from("beam_alloc"),
            strategy: Strategy::FirstFit,
            carrier_size: AllocatorConfig::DEFAULT_CARRIER_SIZE,
            classes: SizeClasses::default_small_block(),
            max_footprint: None,
            zeroed_carriers: true,
        }
    }
}

/// Builder for [`AllocatorConfig`]; obtain it from [`AllocatorConfig::builder`].
#[derive(Clone, Debug)]
pub struct AllocatorConfigBuilder {
    config: AllocatorConfig,
}

impl AllocatorConfigBuilder {
    /// Sets the allocator name.
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.config.name = name.into();
        self
    }

    /// Sets the allocation strategy.
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.config.strategy = strategy;
        self
    }

    /// Sets the carrier size in bytes.
    pub fn carrier_size(mut self, bytes: usize) -> Self {
        self.config.carrier_size = bytes;
        self
    }

    /// Sets the size-class table.
    pub fn classes(mut self, classes: SizeClasses) -> Self {
        self.config.classes = classes;
        self
    }

    /// Sets (or clears) the total-footprint cap in bytes.
    pub fn max_footprint(mut self, bytes: Option<usize>) -> Self {
        self.config.max_footprint = bytes;
        self
    }

    /// Sets whether freshly mapped carriers are handed out zeroed.
    pub fn zeroed_carriers(mut self, zeroed: bool) -> Self {
        self.config.zeroed_carriers = zeroed;
        self
    }

    /// Finishes the configuration.
    pub fn build(self) -> AllocatorConfig {
        self.config
    }
}

/// The alignment padding between the fixed block header and the payload for a
/// request aligned to `align`.
///
/// Only meaningful for `align <= os_page_size()` (which the allocator enforces):
/// a carrier base is page-aligned, so any smaller power-of-two alignment divides
/// it and the padding depends on `align` alone — `align - BLOCK_HEADER_SIZE` for a
/// larger-than-header alignment, and none otherwise. That is what makes a block
/// locatable from the caller's layout on free/realloc.
fn padding_for(align: usize) -> usize {
    if align <= UNIT_SIZE {
        0
    } else {
        align - UNIT_SIZE
    }
}

/// The mutable, mutex-guarded state of a [`CarrierAllocator`].
#[derive(Default)]
struct AllocatorState {
    /// Carriers currently mapped. Boxed so a block's recorded carrier address stays
    /// stable across later carrier insertions (never store a pointer into a growing
    /// `Vec<Carrier>`).
    carriers: Vec<Box<Carrier>>,
    /// `carrier base address -> index in `carriers``, for O(1) block→carrier
    /// recovery on the alloc/free paths.
    by_base: HashMap<usize, usize>,
    /// The per-size-class free-block lists.
    free_lists: FreeList,
    /// Instrumentation counters.
    counters: Counters,
    /// Total bytes held in mapped carriers (the footprint that `max_footprint`
    /// caps; mirrors `Counters::carrier_bytes`).
    mapped_bytes: usize,
}

impl AllocatorState {
    /// Installs `carrier` and returns its index.
    fn insert_carrier(&mut self, carrier: Carrier) -> usize {
        let index = self.carriers.len();
        self.by_base.insert(carrier.base() as usize, index);
        self.carriers.push(Box::new(carrier));
        index
    }

    /// The index of the carrier whose base address is `base`.
    fn carrier_index(&self, base: usize) -> Option<usize> {
        self.by_base.get(&base).copied()
    }

    /// Removes and returns the carrier whose base address is `base`.
    fn take_carrier(&mut self, base: usize) -> Option<Box<Carrier>> {
        let index = self.by_base.remove(&base)?;
        if index >= self.carriers.len() {
            return None;
        }
        let removed = self.carriers.swap_remove(index);
        debug_assert_eq!(
            removed.base() as usize,
            base,
            "carrier index table out of sync"
        );
        if index < self.carriers.len() {
            // `swap_remove` moved the last carrier into `index`; fix its record.
            let moved_base = self.carriers[index].base() as usize;
            self.by_base.insert(moved_base, index);
        }
        Some(removed)
    }

    /// Records a newly mapped carrier.
    fn note_carrier_up(&mut self, bytes: usize) {
        self.counters.note_carrier_up(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_add(bytes);
    }

    /// Records a carrier returned to the OS.
    fn note_carrier_down(&mut self, bytes: usize) {
        self.counters.note_carrier_down(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_sub(bytes);
    }

    /// Bumps the per-carrier free-block tally for `base`.
    fn add_free_block(&mut self, base: usize) {
        if let Some(index) = self.carrier_index(base) {
            let free = self.carriers[index].free_count().saturating_add(1);
            self.carriers[index].set_free_count(free);
        }
    }

    /// Drops the per-carrier free-block tally for `base`.
    fn remove_free_block(&mut self, base: usize) {
        if let Some(index) = self.carrier_index(base) {
            let free = self.carriers[index].free_count().saturating_sub(1);
            self.carriers[index].set_free_count(free);
        }
    }

    /// Returns a free block of `class` to its class list and updates the owning
    /// carrier's tally.
    fn return_block(&mut self, class: usize, payload: *mut u8, carrier_base: usize) -> Result<()> {
        self.free_lists.reserve(class, 1)?;
        self.add_free_block(carrier_base);
        // SAFETY: `payload` is the payload of a free block of `class` (its header is
        // directly below and intact, since the intrusive link lives in the payload),
        // and the caller guarantees it is not reachable from anywhere else.
        unsafe { self.free_lists.push(class, payload) };
        Ok(())
    }

    /// Takes a free block of `class` off its list, or `None` when it is empty.
    fn take_block(&mut self, class: usize) -> Option<*mut u8> {
        // SAFETY: `class` is a class index the caller reserved; the popped node is
        // removed from the list by this call.
        let payload = unsafe { self.free_lists.pop(class) }?;
        // SAFETY: every block this allocator creates gets a header when it is
        // subdivided or freed, and the header sits `BLOCK_HEADER_SIZE` below the
        // payload (the free-list link overwrites the payload, not the header).
        let header = unsafe { BlockHeader::load(payload.sub(BLOCK_HEADER_SIZE)) };
        self.remove_free_block(header.carrier as usize);
        Some(payload)
    }
}

impl Drop for AllocatorState {
    fn drop(&mut self) {
        for carrier in self.carriers.drain(..) {
            let carrier = *carrier;
            // SAFETY: this state exclusively owns every carrier it mapped and is
            // being dropped, so no live block is used afterwards; each mapping is
            // unmapped exactly once here (a `Carrier` has no `Drop` of its own).
            unsafe { carrier.unmap() };
        }
        self.by_base.clear();
    }
}

/// The concrete carrier-backed multi-block allocator.
///
/// Construction maps no memory; carriers are mapped lazily on the first allocation.
/// All mutation is serialised through a single [`parking_lot::Mutex`] (the M1
/// concurrency model — thread-specific allocators are a documented gap).
pub struct CarrierAllocator {
    config: AllocatorConfig,
    state: Mutex<AllocatorState>,
}

impl CarrierAllocator {
    /// Creates an allocator from `config`.
    pub fn new(config: AllocatorConfig) -> Result<CarrierAllocator> {
        Ok(CarrierAllocator {
            config,
            state: Mutex::new(AllocatorState::default()),
        })
    }

    /// Consumes this allocator into a shared [`AllocRef`] handle.
    pub fn share(self) -> AllocRef {
        Arc::new(self)
    }

    /// The number of carriers currently mapped (redundant with
    /// [`Allocator::stats`]).
    pub fn carrier_count(&self) -> u64 {
        self.state.lock().counters.snapshot().carriers
    }

    /// The total bytes held in mapped carriers (redundant with
    /// [`Allocator::stats`]).
    pub fn carrier_bytes(&self) -> usize {
        self.state.lock().counters.snapshot().carrier_bytes
    }

    /// Locates the header of the block whose payload is `payload`, given the
    /// alignment the allocation was made with.
    ///
    /// Returns `None` when `align` cannot be served by this allocator (so the block
    /// cannot be located at all) or when the header does not describe a block this
    /// allocator could have written. Callers must then *refuse* to free — the
    /// alternative would be `munmap`ing an address that is not a mapping.
    fn locate(payload: *mut u8, align: usize) -> Option<(*mut u8, BlockHdr)> {
        if !align.is_power_of_two() || align > os_page_size() {
            return None;
        }
        let offset = padding_for(align);
        let block_start = payload.wrapping_sub(BLOCK_HEADER_SIZE + offset);
        // SAFETY: `block_start` is where this allocator puts the header of a block
        // handed out with this alignment, so reading the 16-byte header is in-bounds
        // for any block it created. A caller that passes a wrong layout yields an
        // unverifiable address; the plausibility test below rejects such a header
        // instead of trusting it.
        let header = unsafe { BlockHeader::load(block_start) };
        if Self::plausible(&header, offset) {
            Some((block_start, header))
        } else {
            None
        }
    }

    /// Whether `header` looks like one this allocator wrote for a payload at
    /// padding `offset`.
    fn plausible(header: &BlockHdr, offset: usize) -> bool {
        let flags = BlockFlags::from_bits(header.flags);
        header.size % UNIT_SIZE == 0
            && header.size >= BLOCK_HEADER_SIZE + offset
            && header.size <= MAX_BLOCK_SIZE
            && header.payload_offset == offset
            && header.flags & !ALL_FLAGS == 0
            && (header.class == CLASS_NONE) == flags.is_large()
            && (header.class == CLASS_NONE || usize::from(header.class) < MAX_CLASS_COUNT)
    }

    /// Rejects an allocation that would push the mapped footprint past the
    /// configured cap.
    fn check_footprint(&self, state: &AllocatorState, bytes: usize) -> Result<()> {
        if let Some(cap) = self.config.max_footprint {
            if state.mapped_bytes.saturating_add(bytes) > cap {
                return Err(error::carrier_failure(bytes));
            }
        }
        Ok(())
    }

    /// The slab path: serves `size` bytes from the `size <= large_threshold` class
    /// with `MIN_ALIGN`-or-weaker alignment.
    fn alloc_small(&self, state: &mut AllocatorState, size: usize) -> Result<NonNull<u8>> {
        let classes = &self.config.classes;
        let Some(class) = classes.class_for(size) else {
            return Err(error::carrier_failure(size));
        };
        let Some(payload_capacity) = classes.class_size(class) else {
            return Err(error::carrier_failure(size));
        };
        debug_assert!(
            class < MAX_CLASS_COUNT,
            "class index must fit the block header"
        );
        let block_size = BLOCK_HEADER_SIZE + payload_capacity;
        if block_size > MAX_BLOCK_SIZE {
            return Err(error::carrier_failure(block_size));
        }
        state.free_lists.reserve(class, 1)?;
        if state.free_lists.free_blocks(class) == 0 {
            self.map_slab_carrier(state, class, block_size)?;
        }
        let Some(payload) = state.take_block(class) else {
            return Err(error::carrier_failure(block_size));
        };
        let block_start = payload.wrapping_sub(BLOCK_HEADER_SIZE);
        // SAFETY: every slab block has a header, written when the carrier was
        // subdivided or when the block was last freed.
        let previous = unsafe { BlockHeader::load(block_start) };
        let mut flags = BlockFlags::from_bits(previous.flags);
        flags.clear_free();
        let header = BlockHdr {
            size: block_size,
            payload_offset: 0,
            class: class as u16,
            carrier: previous.carrier,
            flags: flags.bits(),
        };
        // SAFETY: `block_start` is this block's header address: inside a live carrier
        // mapping, `UNIT_SIZE`-aligned, and not overlapping the payload handed out
        // below.
        unsafe { BlockHeader::store(block_start, header) };
        state.counters.note_alloc(block_size);
        // SAFETY: `payload` points into a live carrier mapping and is non-null (it is
        // derived from a successful `mmap` base).
        Ok(unsafe { NonNull::new_unchecked(payload) })
    }

    /// Maps a new carrier dedicated to `class` and subdivides it into `block_size`
    /// blocks pushed onto the class's free list.
    fn map_slab_carrier(
        &self,
        state: &mut AllocatorState,
        class: usize,
        block_size: usize,
    ) -> Result<()> {
        // A carrier must hold at least one block, so a carrier size smaller than the
        // block is raised to the block size (this is what makes the subdivision
        // below infallible).
        let want = round_up_unit(self.config.carrier_size.max(block_size)).max(UNIT_SIZE);
        self.check_footprint(state, want)?;
        // SAFETY: `alloc_map` hands back exclusive ownership of a fresh mapping;
        // `MIN_ALIGN` is a supported alignment.
        let mut carrier = unsafe { Carrier::alloc_map(want, MIN_ALIGN)? };
        carrier.set_class(None);
        carrier.set_block_count(0);
        carrier.set_free_count(0);
        let mapped = carrier.size();
        state.note_carrier_up(mapped);
        let index = state.insert_carrier(carrier);
        let class = class as u16;
        let AllocatorState {
            carriers,
            free_lists,
            ..
        } = state;
        carriers[index].subdivide_into(class, block_size, free_lists)
    }

    /// The large path: a dedicated single-block carrier for a request larger than
    /// the threshold or more aligned than [`MIN_ALIGN`].
    fn alloc_large(
        &self,
        state: &mut AllocatorState,
        size: usize,
        align: usize,
    ) -> Result<NonNull<u8>> {
        let payload_offset = padding_for(align);
        let needed = BLOCK_HEADER_SIZE
            .saturating_add(payload_offset)
            .saturating_add(size);
        if needed > MAX_BLOCK_SIZE {
            return Err(error::carrier_failure(size));
        }
        // The whole block (header + padding + payload) is one mapping that `munmap`s
        // as a unit when the block is freed, and its size is what the header records.
        let block_size = round_up_unit(needed);
        self.check_footprint(state, block_size)?;
        // SAFETY: `alloc_map` hands back exclusive ownership of a fresh mapping;
        // `align` is a power of two at most the page size (checked by `alloc`).
        let mut carrier = unsafe { Carrier::alloc_map(block_size, align)? };
        debug_assert_eq!(carrier.size(), block_size);
        let base = carrier.base();
        carrier.set_class(None);
        carrier.set_block_count(1);
        carrier.set_free_count(0);
        state.note_carrier_up(block_size);
        state.insert_carrier(carrier);
        let mut flags = BlockFlags::empty();
        flags.set_large();
        flags.set_last_in_carrier();
        let header = BlockHdr {
            size: block_size,
            payload_offset,
            class: CLASS_NONE,
            carrier: base.cast::<c_void>(),
            flags: flags.bits(),
        };
        // SAFETY: `base` is the start of the mapping this block owns, is
        // `UNIT_SIZE`-aligned, and the header occupies its first 16 bytes only.
        unsafe { BlockHeader::store(base, header) };
        state.counters.note_alloc(block_size);
        // SAFETY: the payload starts `BLOCK_HEADER_SIZE + payload_offset` bytes into a
        // mapping of `block_size >=` that plus `size` bytes, so it is in-bounds; it is
        // non-null because it derives from a page-aligned mapping base.
        let payload = unsafe { base.add(BLOCK_HEADER_SIZE + payload_offset) };
        Ok(unsafe { NonNull::new_unchecked(payload) })
    }
}

impl Allocator for CarrierAllocator {
    fn alloc(&self, layout: Layout) -> Result<NonNull<u8>> {
        let size = layout.size();
        let align = layout.align();
        if !align.is_power_of_two() || align > os_page_size() {
            return Err(error::bad_align(align));
        }
        let mut state = self.state.lock();
        if size <= self.config.classes.large_threshold() && align <= UNIT_SIZE {
            self.alloc_small(&mut state, size)
        } else {
            self.alloc_large(&mut state, size, align)
        }
    }

    fn realloc(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        let old_size = old_layout.size();
        let align = old_layout.align();
        let Some((_block_start, header)) = Self::locate(ptr.as_ptr(), align) else {
            return Err(error::bad_align(align));
        };
        // `locate` guarantees `header.size >= BLOCK_HEADER_SIZE + header.payload_offset`.
        let capacity = header.size - BLOCK_HEADER_SIZE - header.payload_offset;
        if new_size <= capacity {
            // The block's payload capacity is unchanged, so the header (and hence the
            // byte accounting) stays as it is: the caller simply uses less of it.
            self.state.lock().counters.note_realloc();
            return Ok(ptr);
        }
        let new_layout =
            Layout::from_size_align(new_size, align).map_err(|_| error::bad_align(align))?;
        // NOTE: the move performs a real alloc + free, so `alloc_calls`/`free_calls`
        // also advance; `live_bytes` stays exact because both sides use the block's
        // own recorded size.
        let new_ptr = self.alloc(new_layout)?;
        // SAFETY: both blocks are live and owned exclusively by the caller, the copy
        // length fits both (it is at most the old block's capacity and the new
        // block's), and the two blocks are distinct allocations, hence disjoint.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.as_ptr(), new_ptr.as_ptr(), old_size.min(new_size));
        }
        self.state.lock().counters.note_realloc();
        // SAFETY: `ptr`/`old_layout` are the caller's live allocation on this
        // allocator and it is not used after this call.
        unsafe { self.dealloc(ptr, old_layout) };
        Ok(new_ptr)
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        let payload = ptr.as_ptr();
        let Some((block_start, header)) = Self::locate(payload, layout.align()) else {
            beam_common::beam_debug!(
                "beam_alloc: refusing to free an unrecognised block at {payload:p}"
            );
            return;
        };
        let flags = BlockFlags::from_bits(header.flags);
        let mut state = self.state.lock();
        if flags.is_large() {
            match state.take_carrier(block_start as usize) {
                Some(carrier) => {
                    let carrier = *carrier;
                    // SAFETY: this block owns the whole mapping (nothing else was
                    // carved from it), the caller is done with `ptr`, and the carrier
                    // was just removed from the allocator's table, so the mapping is
                    // unmapped exactly once.
                    unsafe { carrier.unmap() };
                    state.note_carrier_down(header.size);
                    state.counters.note_free(header.size);
                }
                None => beam_common::beam_debug!(
                    "beam_alloc: large block without a registered carrier at {payload:p}"
                ),
            }
            return;
        }
        let class = usize::from(header.class);
        if class >= self.config.classes.len() {
            beam_common::beam_debug!("beam_alloc: block carries an out-of-range size class");
            return;
        }
        if state.carrier_index(header.carrier as usize).is_none() {
            // A plausible-looking header that names a carrier this allocator does not
            // own: returning the block would put foreign memory on a free list.
            beam_common::beam_debug!("beam_alloc: block claims an unregistered carrier");
            return;
        }
        let mut returned_flags = flags;
        returned_flags.set_free();
        let returned = BlockHdr {
            flags: returned_flags.bits(),
            ..header
        };
        // SAFETY: `block_start` is this block's header location, inside a live carrier
        // mapping, and the block is the caller's exclusively until it is returned here.
        unsafe { BlockHeader::store(block_start, returned) };
        match state.return_block(class, payload, header.carrier as usize) {
            Ok(()) => state.counters.note_free(header.size),
            Err(_) => beam_common::beam_debug!(
                "beam_alloc: could not return a block of class {class} to its free list"
            ),
        }
    }

    fn stats(&self) -> AllocStats {
        self.state.lock().counters.snapshot()
    }

    fn name(&self) -> &str {
        &self.config.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(size: usize, align: usize) -> Layout {
        Layout::from_size_align(size, align).expect("test layout")
    }

    fn allocator() -> CarrierAllocator {
        let config = AllocatorConfig::builder()
            .name("test")
            .carrier_size(64 * 1024)
            .build();
        CarrierAllocator::new(config).expect("allocator")
    }

    #[test]
    fn alignment_padding_is_a_function_of_the_alignment() {
        assert_eq!(padding_for(1), 0);
        assert_eq!(padding_for(8), 0);
        assert_eq!(padding_for(16), 0);
        assert_eq!(padding_for(32), 16);
        assert_eq!(padding_for(64), 48);
        assert_eq!(padding_for(os_page_size()), os_page_size() - UNIT_SIZE);
        // The padded payload is always at an `align` boundary given a page-aligned
        // block start.
        let base = 4096usize;
        for align in [1usize, 8, 16, 32, 64, 256, 4096] {
            let payload = base + BLOCK_HEADER_SIZE + padding_for(align);
            assert_eq!(payload % align, 0, "align {align}");
        }
    }

    #[test]
    fn plausible_rejects_foreign_headers() {
        let good = BlockHdr {
            size: 16 + 64,
            payload_offset: 0,
            class: 2,
            carrier: core::ptr::null_mut(),
            flags: BlockFlags::from_bits(0).bits(),
        };
        assert!(CarrierAllocator::plausible(&good, 0));
        // Wrong padding for the alignment the caller claimed.
        assert!(!CarrierAllocator::plausible(&good, 48));
        // Size smaller than the header plus padding.
        let mut too_small = good;
        too_small.size = 8;
        assert!(!CarrierAllocator::plausible(&too_small, 0));
        // Unknown flag bits.
        let mut bad_flags = good;
        bad_flags.flags = 0x80;
        assert!(!CarrierAllocator::plausible(&bad_flags, 0));
        // Large flag without the `CLASS_NONE` class and vice versa.
        let mut mismatched = good;
        mismatched.class = CLASS_NONE;
        assert!(!CarrierAllocator::plausible(&mismatched, 0));
        let mut mismatched = good;
        let mut flags = BlockFlags::empty();
        flags.set_large();
        mismatched.flags = flags.bits();
        assert!(!CarrierAllocator::plausible(&mismatched, 0));
    }

    #[test]
    fn alloc_dealloc_round_trip_returns_the_counters_to_zero() {
        let alloc = allocator();
        let baseline = alloc.stats();
        assert_eq!(baseline, AllocStats::default());
        let l = layout(100, 16);
        let ptr = alloc.alloc(l).expect("alloc");
        let live = alloc.stats();
        assert_eq!(live.alloc_calls, 1);
        assert_eq!(live.live_blocks, 1);
        assert!(live.live_bytes >= 100);
        assert_eq!(live.live_bytes, live.peak_live_bytes);
        assert_eq!(live.carriers, 1);
        let block_size = live.live_bytes;
        // SAFETY: `ptr` is a live allocation of 100 bytes on `alloc`.
        unsafe { alloc.dealloc(ptr, l) };
        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0);
        assert_eq!(after.live_bytes, 0);
        assert_eq!(after.free_calls, 1);
        assert_eq!(after.alloc_calls, 1);
        assert_eq!(after.peak_live_bytes, block_size);
        // The slab carrier is retained (M1 policy) even once fully free.
        assert_eq!(after.carriers, 1);
        assert_eq!(after.carrier_bytes, baseline.carrier_bytes + 64 * 1024);
    }

    #[test]
    fn large_blocks_get_dedicated_carriers_and_release_them() {
        let alloc = allocator();
        let baseline = alloc.stats();
        let l = layout(4097, 16);
        let first = alloc.alloc(l).expect("alloc");
        let second = alloc.alloc(l).expect("alloc");
        assert_ne!(first.as_ptr(), second.as_ptr());
        let live = alloc.stats();
        assert_eq!(live.carriers, baseline.carriers + 2);
        assert_eq!(live.live_blocks, 2);
        assert!(live.carrier_bytes > baseline.carrier_bytes);
        // SAFETY: both are live allocations on `alloc`.
        unsafe {
            alloc.dealloc(first, l);
        }
        // SAFETY: as above.
        unsafe {
            alloc.dealloc(second, l);
        }
        let after = alloc.stats();
        assert_eq!(after.carriers, baseline.carriers);
        assert_eq!(after.carrier_bytes, baseline.carrier_bytes);
        assert_eq!(after.live_bytes, 0);
        assert_eq!(after.live_blocks, 0);
        assert_eq!(after.free_calls, 2);
    }

    #[test]
    fn unaligned_large_blocks_keep_their_padding_in_the_header() {
        let alloc = allocator();
        for align in [32usize, 64, 256, 4096] {
            let l = layout(2000, align);
            let ptr = alloc.alloc(l).expect("alloc");
            assert_eq!(ptr.as_ptr() as usize % align, 0);
            // SAFETY: the block is live and was allocated with this alignment.
            unsafe { alloc.dealloc(ptr, l) };
        }
        assert_eq!(alloc.stats().live_bytes, 0);
        assert_eq!(
            alloc.stats().carriers,
            0,
            "every large carrier was released"
        );
    }

    #[test]
    fn slab_alignment_up_to_the_minimum_is_honoured() {
        let alloc = allocator();
        for align in [1usize, 2, 4, 8, 16] {
            let l = layout(17, align);
            let ptr = alloc.alloc(l).expect("alloc");
            assert_eq!(ptr.as_ptr() as usize % align, 0, "align {align}");
            // SAFETY: live allocation on `alloc`.
            unsafe { alloc.dealloc(ptr, l) };
        }
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn unsupported_alignments_are_rejected() {
        let alloc = allocator();
        // Larger than any plausible page size: this allocator cannot super-align a
        // carrier, so it must refuse rather than return a misaligned block.
        let l = layout(64, 1 << 20);
        assert!(matches!(
            alloc.alloc(l),
            Err(beam_common::BeamError::BadArg)
        ));
        assert_eq!(alloc.stats().alloc_calls, 0);
    }

    #[test]
    fn many_carriers_are_subdivided_and_reused() {
        let alloc = allocator();
        // Each 64 KiB carrier holds 63 whole blocks of class 1024 (+16 header), so
        // this needs dozens of carriers.
        let l = layout(1024, 16);
        let mut ptrs = Vec::new();
        for i in 0..4000usize {
            let ptr = alloc.alloc(l).expect("alloc");
            // Pattern the whole payload so a mis-sized block shows up as corruption.
            // SAFETY: the allocation holds 1024 writable bytes.
            unsafe { core::ptr::write_bytes(ptr.as_ptr(), (i & 0xFF) as u8, 1024) };
            ptrs.push((ptr, (i & 0xFF) as u8));
        }
        let stats = alloc.stats();
        assert!(
            stats.carriers >= 60,
            "expected many carriers, got {}",
            stats.carriers
        );
        assert_eq!(stats.live_blocks, 4000);
        for (ptr, pattern) in &ptrs {
            // SAFETY: still live, 1024 bytes long.
            let sample = unsafe { core::ptr::read_volatile(ptr.as_ptr().add(1023)) };
            assert_eq!(sample, *pattern, "block contents were clobbered");
        }
        for (ptr, _) in ptrs {
            // SAFETY: each pointer is freed exactly once here.
            unsafe { alloc.dealloc(ptr, l) };
        }
        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0);
        assert_eq!(after.live_bytes, 0);
        assert_eq!(after.alloc_calls, 4000);
        assert_eq!(after.free_calls, 4000);
    }

    #[test]
    fn realloc_grows_in_place_while_the_class_has_room() {
        let alloc = allocator();
        let l = layout(100, 16);
        let ptr = alloc.alloc(l).expect("alloc");
        // Class 128 holds 128 bytes of payload, so 120 still fits.
        let same = alloc.realloc(ptr, l, 120).expect("realloc");
        assert_eq!(same.as_ptr(), ptr.as_ptr(), "no move needed");
        assert_eq!(alloc.stats().realloc_calls, 1);
        assert_eq!(alloc.stats().live_blocks, 1);
        // SAFETY: `ptr` is still the live allocation.
        unsafe { alloc.dealloc(same, l) };
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn realloc_grow_moves_and_preserves_the_payload() {
        let alloc = allocator();
        let l = layout(100, 16);
        let ptr = alloc.alloc(l).expect("alloc");
        for i in 0..100 {
            // SAFETY: 100 writable bytes.
            unsafe { *ptr.as_ptr().add(i) = i as u8 };
        }
        let grown = alloc.realloc(ptr, l, 4000).expect("realloc");
        for i in 0..100 {
            // SAFETY: the grown block holds at least 100 initialised bytes.
            assert_eq!(unsafe { *grown.as_ptr().add(i) }, i as u8);
        }
        assert_eq!(alloc.stats().live_blocks, 1, "the old block was released");
        // SAFETY: `grown` is the live allocation (allocated with the same layout).
        unsafe { alloc.dealloc(grown, l) };
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn realloc_shrink_keeps_the_prefix() {
        let alloc = allocator();
        let l = layout(2000, 16);
        let ptr = alloc.alloc(l).expect("alloc");
        for i in 0..64 {
            // SAFETY: 2000 writable bytes.
            unsafe { *ptr.as_ptr().add(i) = (i as u8) ^ 0x5A };
        }
        let shrunk = alloc.realloc(ptr, l, 64).expect("realloc");
        for i in 0..64 {
            // SAFETY: the block still holds at least 64 bytes.
            assert_eq!(unsafe { *shrunk.as_ptr().add(i) }, (i as u8) ^ 0x5A);
        }
        // SAFETY: live allocation on `alloc`.
        unsafe { alloc.dealloc(shrunk, l) };
        assert_eq!(alloc.stats().live_bytes, 0);
        assert_eq!(alloc.stats().realloc_calls, 1);
    }

    #[test]
    fn realloc_of_a_foreign_pointer_reports_badarg() {
        let alloc = allocator();
        let foreign = layout(64, 16);
        // A block that was never handed out by this allocator: the header lookup
        // finds something implausible and the call fails instead of corrupting memory.
        let buffer = Box::new([0u8; 4096]);
        // SAFETY: offset 1024 is inside the 4096-byte buffer, so the 16 bytes below it
        // that `locate` inspects are in-bounds (they are zeroed, i.e. an implausible
        // header).
        let bogus =
            NonNull::new(unsafe { buffer.as_ptr().add(1024) as *mut u8 }).expect("non-null");
        assert!(matches!(
            alloc.realloc(bogus, foreign, 4096),
            Err(beam_common::BeamError::BadArg)
        ));
        // Whatever happened, no live block was created and nothing was freed.
        assert_eq!(alloc.stats(), AllocStats::default());
    }

    #[test]
    fn max_footprint_caps_carrier_acquisition() {
        let config = AllocatorConfig::builder()
            .name("capped")
            .carrier_size(64 * 1024)
            .max_footprint(Some(128 * 1024))
            .build();
        let alloc = CarrierAllocator::new(config).expect("allocator");
        let l = layout(1024, 16);
        let mut pointers = Vec::new();
        let failure = loop {
            match alloc.alloc(l) {
                Ok(ptr) => pointers.push(ptr),
                Err(error) => break error,
            }
        };
        assert_eq!(pointers.len(), 126, "two 64 KiB carriers of 63 blocks each");
        match failure {
            beam_common::BeamError::Alloc(failure) => {
                assert_eq!(failure.kind, beam_common::AllocKind::Carrier);
            }
            other => panic!("expected an allocation failure, got {other:?}"),
        }
        assert_eq!(alloc.stats().carriers, 2);
        for ptr in pointers {
            // SAFETY: each pointer is live and freed exactly once.
            unsafe { alloc.dealloc(ptr, l) };
        }
        assert_eq!(alloc.stats().live_bytes, 0);
        // The cap also bounds the large path.
        let big = layout(4096 * 1024, 16);
        assert!(matches!(
            alloc.alloc(big),
            Err(beam_common::BeamError::Alloc(_))
        ));
    }

    #[test]
    fn alloc_zeroed_hands_out_zeroed_memory() {
        let alloc = allocator();
        let l = layout(512, 16);
        let ptr = alloc.alloc(l).expect("alloc");
        // SAFETY: 512 writable bytes.
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0xFF, 512) };
        // SAFETY: live allocation freed exactly once.
        unsafe { alloc.dealloc(ptr, l) };
        let zeroed = alloc.alloc_zeroed(l).expect("alloc_zeroed");
        for i in 0..512 {
            // SAFETY: 512 initialised (zeroed) bytes.
            assert_eq!(unsafe { *zeroed.as_ptr().add(i) }, 0);
        }
        // SAFETY: live allocation freed exactly once.
        unsafe { alloc.dealloc(zeroed, l) };
    }

    #[test]
    fn alloc_bytes_uses_the_minimum_alignment() {
        let alloc = allocator();
        let ptr = alloc.alloc_bytes(48).expect("alloc_bytes");
        assert_eq!(ptr.as_ptr() as usize % MIN_ALIGN, 0);
        // SAFETY: 48 writable bytes.
        unsafe { core::ptr::write_bytes(ptr.as_ptr(), 7, 48) };
        let l = layout(48, MIN_ALIGN);
        // SAFETY: live allocation freed exactly once.
        unsafe { alloc.dealloc(ptr, l) };
    }

    #[test]
    fn zero_sized_allocations_still_return_a_pointer() {
        let alloc = allocator();
        let l = layout(0, 16);
        let ptr = alloc.alloc(l).expect("alloc");
        assert_eq!(ptr.as_ptr() as usize % MIN_ALIGN, 0);
        // SAFETY: live allocation freed exactly once.
        unsafe { alloc.dealloc(ptr, l) };
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn share_and_accessors_agree_with_stats() {
        let alloc = allocator();
        let l = layout(200, 16);
        let ptr = alloc.alloc(l).expect("alloc");
        assert_eq!(alloc.name(), "test");
        assert_eq!(alloc.carrier_count(), 1);
        assert_eq!(alloc.carrier_bytes(), 64 * 1024);
        // SAFETY: live allocation freed exactly once.
        unsafe { alloc.dealloc(ptr, l) };
        let shared: AllocRef = allocator().share();
        assert_eq!(shared.name(), "test");
        let ptr = shared.alloc_bytes(32).expect("alloc through the handle");
        // SAFETY: live allocation on `shared`, freed exactly once.
        unsafe { shared.dealloc(ptr, layout(32, MIN_ALIGN)) };
        assert_eq!(shared.stats().live_bytes, 0);
    }

    #[test]
    fn dealloc_of_a_foreign_pointer_is_ignored() {
        let alloc = allocator();
        let buffer = Box::new([0u8; 4096]);
        // SAFETY: offset 1024 is inside the buffer, so the header `dealloc` inspects
        // below it is in-bounds (and zeroed, hence implausible).
        let bogus =
            NonNull::new(unsafe { buffer.as_ptr().add(1024) as *mut u8 }).expect("non-null");
        // SAFETY: the pointer is not one this allocator handed out; the call must
        // detect that and leave the allocator's state untouched.
        unsafe { alloc.dealloc(bogus, layout(64, 16)) };
        assert_eq!(alloc.stats(), AllocStats::default());
    }
}
