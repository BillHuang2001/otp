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
use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::sync::{Arc, LazyLock};

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
use crate::stats::{AllocStats, AllocatorInfo, Counters, TypeStats};
use crate::strategy::Strategy;
use crate::types::{AllocatorId, TypeId};

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

    // -- Typed (interposition) surface ----------------------------------------------
    //
    // The methods below are the *typed* counterparts of `alloc`/`realloc`/`dealloc`:
    // they carry the allocation [`TypeId`] the caller is allocating for, so an
    // allocator can attribute the request to the legacy per-type table
    // (`erts_alloc(TYPE, size)` -> the `blocks[<name>]` rows of
    // `erts_alloc:info/0`). Every one of them has a default that forwards to the
    // untyped method, so the trait stays object-safe and an allocator that does not
    // track types (`stats.rs`'s per-type table is optional) needs no change.
    //
    // Untyped and typed calls are *not* interchangeable accounting-wise: an allocator
    // that implements the typed methods must record the type-attributed event once
    // (which, for this crate's carrier allocator, also performs the aggregate
    // accounting), and must never additionally record the untyped aggregate event.

    /// Allocates `layout` for allocation type `ty`.
    ///
    /// The default forwards to [`alloc`](Allocator::alloc) and records nothing about
    /// `ty`; an allocator that keeps a per-type table overrides this (and must then
    /// perform the aggregate accounting itself so an event is recorded exactly once).
    fn alloc_typed(&self, _ty: TypeId, layout: Layout) -> Result<NonNull<u8>> {
        self.alloc(layout)
    }

    /// Grows or shrinks an allocation of type `ty` to `new_size` bytes, preserving its
    /// contents up to the smaller of the old and new sizes.
    ///
    /// The typed counterpart of [`realloc`](Allocator::realloc); the default forwards
    /// to it.
    fn realloc_typed(
        &self,
        _ty: TypeId,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        self.realloc(ptr, old_layout, new_size)
    }

    /// Frees an allocation of type `ty` previously returned by
    /// [`alloc_typed`](Allocator::alloc_typed) (or [`realloc_typed`](Allocator::realloc_typed))
    /// on *this* allocator.
    ///
    /// The typed counterpart of [`dealloc`](Allocator::dealloc); the default forwards
    /// to it.
    ///
    /// # Safety
    ///
    /// `ptr` must have been returned by `alloc_typed`/`realloc_typed` on this
    /// allocator, must not be used afterwards, and `layout` must be the layout it was
    /// allocated with.
    unsafe fn dealloc_typed(&self, _ty: TypeId, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: the caller upheld `dealloc`'s contract.
        unsafe { self.dealloc(ptr, layout) }
    }

    /// Allocates `size` bytes of allocation type `ty` at the default alignment
    /// ([`MIN_ALIGN`]).
    ///
    /// "Unsized" because the caller supplies only a byte count, not a full
    /// [`Layout`] — the shape the legacy `erts_alloc(TYPE, size)` macro and a driver's
    /// `erl_drv_alloc` have. The default forwards to
    /// [`alloc_bytes`](Allocator::alloc_bytes).
    fn alloc_unsized(&self, _ty: TypeId, size: usize) -> Result<NonNull<u8>> {
        self.alloc_bytes(size)
    }

    /// Frees the block whose payload is `ptr`, knowing only the pointer.
    ///
    /// This is the interposition-friendly inverse of
    /// [`alloc_unsized`](Allocator::alloc_unsized) (`erts_free(TYPE, ptr)` /
    /// `erl_drv_free`): the caller does not — and cannot — pass the original layout,
    /// so the allocator must recover it from the block itself.
    ///
    /// # Default behaviour (a bounded header probe)
    ///
    /// An allocator that keeps a header *in band* (immediately before the payload) can
    /// reconstruct the layout; one that does not cannot identify a bare pointer and
    /// must not free anything. The default therefore probes, in order, the candidate
    /// alignments this crate's carrier model supports — [`MIN_ALIGN`] followed by
    /// every power of two up to and including [`os_page_size`] — and calls
    /// [`dealloc_typed`](Allocator::dealloc_typed) for the **first** candidate whose
    /// in-band header passes the plausibility check. Two properties make that safe
    /// and cheap:
    ///
    /// * **Bounded.** Only `1 + log2(os_page_size() / MIN_ALIGN)` candidates are
    ///   tried (9 for a 4 KiB page), whatever the alignment the block was allocated
    ///   with.
    /// * **No double free.** The candidate set produces pairwise *distinct* payload
    ///   offsets `padding_for(align)` — `0` for [`MIN_ALIGN`] and `align - UNIT_SIZE`
    ///   for each larger power of two — and the plausibility check requires the
    ///   header's recorded offset to equal the candidate's, so at most one candidate
    ///   can ever describe a given block.
    ///
    /// A pointer that no candidate recognises is left alone (logged, never freed), so
    /// the default never frees a pointer it cannot identify. The probed layout carries
    /// only the recovered alignment (its size is `0`): a layout-less free cannot know
    /// the size, and this crate's [`dealloc`](Allocator::dealloc) derives the whole
    /// block, size included, from the alignment and the recorded header. An allocator
    /// whose `dealloc` needs the exact size must override this method.
    ///
    /// # Safety
    ///
    /// `ptr` must be `null` or a pointer previously returned by
    /// [`alloc_unsized`](Allocator::alloc_unsized)/[`alloc_typed`](Allocator::alloc_typed)
    /// on *this* allocator and not yet freed. A pointer the allocator does not
    /// recognise is refused, but the pointer itself still has to be a value the
    /// allocator could plausibly have produced (in particular, a pointer into memory
    /// this process may read just below).
    unsafe fn dealloc_unsized(&self, ty: TypeId, ptr: NonNull<u8>) {
        let payload = ptr.as_ptr();
        for align in probe_alignments() {
            let offset = padding_for(align);
            let block_start = payload.wrapping_sub(BLOCK_HEADER_SIZE + offset);
            if !plausible_header_at(block_start, offset) {
                continue;
            }
            // SAFETY: `dealloc_typed`'s contract is `ptr` came from a typed allocation
            // on this allocator; the in-band header just identified it as a block
            // allocated with alignment `align`, and it is freed exactly once because
            // no other candidate can share this payload offset.
            if let Ok(layout) = Layout::from_size_align(0, align) {
                unsafe { self.dealloc_typed(ty, ptr, layout) };
            }
            return;
        }
        beam_common::beam_debug!(
            "beam_alloc: refusing to free an unrecognised block at {payload:p}"
        );
    }

    /// The per-*type* counters this allocator has accumulated for allocation type
    /// `ty`.
    ///
    /// The default reports the all-zero value: an allocator that does not keep the
    /// per-type table records nothing. Introspection only — it must not be called on
    /// an allocation hot path.
    fn type_stats(&self, _ty: TypeId) -> TypeStats {
        TypeStats::default()
    }

    /// The allocator's `=allocator:<name>` report section.
    ///
    /// Returns the per-allocator item list of
    /// [`AllocatorInfo::from_stats`](crate::stats::AllocatorInfo::from_stats) over
    /// [`stats`](Allocator::stats). The default adds no per-type rows; an allocator
    /// that keeps a per-type table overrides this to append one
    /// [`push_type`](crate::stats::AllocatorInfo::push_type) row set per allocation
    /// type bound to it.
    ///
    /// Introspection only — it snapshots the counters and builds a `Vec`, so it must
    /// never run on an allocation hot path.
    fn allocator_info(&self) -> AllocatorInfo {
        AllocatorInfo::from_stats(report_name(self.name()), &self.stats())
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

/// The alignments [`Allocator::dealloc_unsized`] probes, in order.
///
/// [`MIN_ALIGN`] comes first and stands in for every alignment at or below it (they
/// all have a zero payload offset); each following entry is the next power of two up
/// to and including [`os_page_size`], the largest alignment the carrier model serves.
/// The sequence is finite — `1 + log2(os_page_size() / MIN_ALIGN)` items — and its
/// entries have pairwise distinct payload offsets ([`padding_for`]), which is what
/// keeps the probe from ever matching one block twice.
fn probe_alignments() -> impl Iterator<Item = usize> {
    let page = os_page_size();
    let mut next = MIN_ALIGN;
    core::iter::from_fn(move || {
        if next > page {
            return None;
        }
        let align = next;
        next = next.saturating_mul(2);
        Some(align)
    })
}

/// Whether `header` looks like one this crate's carrier allocator wrote for a payload
/// at padding `offset`.
fn plausible_header(header: &BlockHdr, offset: usize) -> bool {
    let flags = BlockFlags::from_bits(header.flags);
    header.size % UNIT_SIZE == 0
        && header.size >= BLOCK_HEADER_SIZE + offset
        && header.size <= MAX_BLOCK_SIZE
        && header.payload_offset == offset
        && header.flags & !ALL_FLAGS == 0
        && (header.class == CLASS_NONE) == flags.is_large()
        && (header.class == CLASS_NONE || usize::from(header.class) < MAX_CLASS_COUNT)
}

/// The module-level form of the header plausibility check, reading the header word at
/// `block_start`.
///
/// Used by the default [`Allocator::dealloc_unsized`] probe (the concrete
/// [`CarrierAllocator`] reaches the same predicate through its own `locate`).
fn plausible_header_at(block_start: *mut u8, offset: usize) -> bool {
    // SAFETY: `block_start` is where this crate's carrier allocator puts the header of
    // a block allocated with the candidate alignment, so the 16-byte read is in-bounds
    // for any block *this crate* created. The caller's contract is that `block_start`
    // is a plausible address (a pointer this allocator could have produced); a header
    // that is not one of ours is rejected by the predicate below instead of trusted,
    // and a caller that violates the contract yields an unverifiable address that the
    // predicate merely declines to recognise.
    let header = unsafe { BlockHeader::load(block_start) };
    plausible_header(&header, offset)
}

/// The [`AllocatorId`] whose declared name is `name`, if any.
///
/// The registry names every allocator after its id ([`AllocatorId::as_str`]), so this
/// is how an allocator recovers "which declared allocator am I?" for the
/// `=allocator:<name>` report and for its allocation-type listing.
fn allocator_id_for_name(name: &str) -> Option<AllocatorId> {
    AllocatorId::ALL.into_iter().find(|id| id.as_str() == name)
}

/// Resolves an allocator's runtime `name` to the `&'static str` an
/// [`AllocatorInfo`] carries.
///
/// The report stores its allocator name as a `&'static str` (it is compared against
/// the declared allocator identifiers), while an allocator's configured name is a
/// runtime `String`. A name that matches a declared [`AllocatorId`] — every allocator
/// the registry builds — resolves to that id's already-`'static` name and allocates
/// nothing. Any other name (only a directly-constructed test allocator) is interned
/// **once** in a process-global table, so repeated report snapshots neither leak
/// repeatedly nor grow without bound.
fn report_name(name: &str) -> &'static str {
    if let Some(id) = allocator_id_for_name(name) {
        return id.as_str();
    }
    static INTERNED: LazyLock<Mutex<HashSet<&'static str>>> =
        LazyLock::new(|| Mutex::new(HashSet::new()));
    let mut interned = INTERNED.lock();
    if let Some(&known) = interned.get(name) {
        return known;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    interned.insert(leaked);
    leaked
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

    /// Records a newly mapped *single-block* carrier (the dedicated carrier of a large
    /// request), which the report counts as an SBC and as part of the aggregate.
    fn note_sbc_carrier_up(&mut self, bytes: usize) {
        self.counters.note_sbc_carrier_up(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_add(bytes);
    }

    /// Records a single-block carrier of `bytes` returned to the OS.
    fn note_sbc_carrier_down(&mut self, bytes: usize) {
        self.counters.note_sbc_carrier_down(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_sub(bytes);
    }

    /// Records a newly mapped *multi-block* carrier (a slab carrier subdivided into one
    /// class's blocks), which the report counts as an MBC and as part of the aggregate.
    fn note_mbc_carrier_up(&mut self, bytes: usize) {
        self.counters.note_mbc_carrier_up(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_add(bytes);
    }

    /// Records a multi-block carrier of `bytes` returned to the OS.
    #[allow(dead_code)]
    fn note_mbc_carrier_down(&mut self, bytes: usize) {
        self.counters.note_mbc_carrier_down(bytes);
        self.mapped_bytes = self.mapped_bytes.saturating_sub(bytes);
    }

    /// Records a successful allocation of `bytes`, attributing it to `ty` when the
    /// caller asked for per-type accounting.
    ///
    /// The typed counter method performs the aggregate accounting itself, so exactly
    /// one of the two branches runs for a given event: the
    /// `stats.carriers == stats.sbc_carriers + stats.mbc_carriers` invariant and the
    /// `live_blocks`/`live_bytes` totals stay exact.
    fn note_alloc_for(&mut self, ty: Option<TypeId>, bytes: usize) {
        match ty {
            Some(ty) => self.counters.note_alloc_typed(ty.index(), bytes),
            None => self.counters.note_alloc(bytes),
        }
    }

    /// Records a free of `bytes`, attributing it to `ty` when the caller asked for
    /// per-type accounting.
    fn note_free_for(&mut self, ty: Option<TypeId>, bytes: usize) {
        match ty {
            Some(ty) => self.counters.note_free_typed(ty.index(), bytes),
            None => self.counters.note_free(bytes),
        }
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
        plausible_header(header, offset)
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
    /// with `MIN_ALIGN`-or-weaker alignment, recording the allocation for `ty`.
    fn alloc_small(
        &self,
        state: &mut AllocatorState,
        size: usize,
        ty: Option<TypeId>,
    ) -> Result<NonNull<u8>> {
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
        state.note_alloc_for(ty, block_size);
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
        state.note_mbc_carrier_up(mapped);
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
    /// the threshold or more aligned than [`MIN_ALIGN`], recording the allocation for
    /// `ty`. The alignment padding in front of the payload is zeroed, so a segment
    /// reused from the `mseg` cache cannot leave a previous owner's bytes where the
    /// unsized-dealloc probe would read a candidate block header.
    fn alloc_large(
        &self,
        state: &mut AllocatorState,
        size: usize,
        align: usize,
        ty: Option<TypeId>,
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
        state.note_sbc_carrier_up(block_size);
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
        if payload_offset > 0 {
            // Zero the alignment padding between the block header and the payload.
            //
            // `dealloc_unsized` locates a block's header by *probing* candidate
            // alignments and returning on the **first** plausibility match. For an
            // over-aligned block every candidate below the real alignment reads its
            // 16-byte header from `[base + BLOCK_HEADER_SIZE, payload)` — the padding
            // this allocator never writes. Carriers come from the process-wide segment
            // (`mseg`) reuse cache, which hands a released segment back at the *same*
            // address *unzeroed*, so a previous owner's block header (e.g. a size-80
            // slab block of another allocator) can survive in this padding. The probe's
            // first candidate (`align = MIN_ALIGN`, offset 0) would then decode that
            // stale-but-plausible header, "free" the live payload through it (wrong
            // byte accounting, a live payload pushed onto a size-class free list) and
            // leave the real dedicated carrier mapped. Zeroed padding can never pass
            // `plausible_header` (a zero header has `size == 0 < BLOCK_HEADER_SIZE`),
            // so only the real header at `base` can ever match.
            //
            // SAFETY: the padding is the `payload_offset` bytes starting at
            // `base + BLOCK_HEADER_SIZE`, entirely inside the `block_size`-byte mapping
            // this block owns and disjoint from both the 16-byte header (which starts at
            // `base`) and the payload (which starts at `base + BLOCK_HEADER_SIZE +
            // payload_offset`); it is never handed to the caller and never written
            // again for the block's lifetime.
            unsafe { core::ptr::write_bytes(base.add(BLOCK_HEADER_SIZE), 0, payload_offset) };
        }
        state.note_alloc_for(ty, block_size);
        // SAFETY: the payload starts `BLOCK_HEADER_SIZE + payload_offset` bytes into a
        // mapping of `block_size >=` that plus `size` bytes, so it is in-bounds; it is
        // non-null because it derives from a page-aligned mapping base.
        let payload = unsafe { base.add(BLOCK_HEADER_SIZE + payload_offset) };
        Ok(unsafe { NonNull::new_unchecked(payload) })
    }

    /// The shared allocation path behind [`Allocator::alloc`] and
    /// [`Allocator::alloc_typed`].
    ///
    /// `ty` is `None` for the untyped entry point (aggregate accounting only) and
    /// `Some(..)` when the caller asked for per-type attribution; the chosen branch
    /// records the allocation exactly once.
    fn alloc_impl(&self, layout: Layout, ty: Option<TypeId>) -> Result<NonNull<u8>> {
        let size = layout.size();
        let align = layout.align();
        if !align.is_power_of_two() || align > os_page_size() {
            return Err(error::bad_align(align));
        }
        let mut state = self.state.lock();
        if size <= self.config.classes.large_threshold() && align <= UNIT_SIZE {
            self.alloc_small(&mut state, size, ty)
        } else {
            self.alloc_large(&mut state, size, align, ty)
        }
    }

    /// The shared reallocation path behind [`Allocator::realloc`] and
    /// [`Allocator::realloc_typed`]; `ty` selects the accounting of the alloc + free
    /// pair a growing move performs.
    fn realloc_impl(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
        ty: Option<TypeId>,
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
            // byte accounting) stays as it is: the caller simply uses less of it. The
            // block keeps its allocation type, so no per-type event is due either.
            self.state.lock().counters.note_realloc();
            return Ok(ptr);
        }
        let new_layout =
            Layout::from_size_align(new_size, align).map_err(|_| error::bad_align(align))?;
        // NOTE: the move performs a real alloc + free, so `alloc_calls`/`free_calls`
        // also advance; `live_bytes` stays exact because both sides use the block's
        // own recorded size.
        let new_ptr = self.alloc_impl(new_layout, ty)?;
        // SAFETY: both blocks are live and owned exclusively by the caller, the copy
        // length fits both (it is at most the old block's capacity and the new
        // block's), and the two blocks are distinct allocations, hence disjoint.
        unsafe {
            core::ptr::copy_nonoverlapping(ptr.as_ptr(), new_ptr.as_ptr(), old_size.min(new_size));
        }
        self.state.lock().counters.note_realloc();
        // SAFETY: `ptr`/`old_layout` are the caller's live allocation on this
        // allocator and it is not used after this call.
        unsafe { self.dealloc_impl(ptr, old_layout, ty) };
        Ok(new_ptr)
    }

    /// The shared free path behind [`Allocator::dealloc`] and
    /// [`Allocator::dealloc_typed`].
    ///
    /// # Safety
    ///
    /// Same contract as [`Allocator::dealloc`]: `ptr` must be a live allocation of
    /// this allocator made with `layout`, and must not be used afterwards.
    unsafe fn dealloc_impl(&self, ptr: NonNull<u8>, layout: Layout, ty: Option<TypeId>) {
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
                    state.note_sbc_carrier_down(header.size);
                    state.note_free_for(ty, header.size);
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
            Ok(()) => state.note_free_for(ty, header.size),
            Err(_) => beam_common::beam_debug!(
                "beam_alloc: could not return a block of class {class} to its free list"
            ),
        }
    }
}

impl Allocator for CarrierAllocator {
    fn alloc(&self, layout: Layout) -> Result<NonNull<u8>> {
        self.alloc_impl(layout, None)
    }

    fn alloc_typed(&self, ty: TypeId, layout: Layout) -> Result<NonNull<u8>> {
        self.alloc_impl(layout, Some(ty))
    }

    fn realloc(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        self.realloc_impl(ptr, old_layout, new_size, None)
    }

    fn realloc_typed(
        &self,
        ty: TypeId,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        self.realloc_impl(ptr, old_layout, new_size, Some(ty))
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded unchanged from the caller.
        unsafe { self.dealloc_impl(ptr, layout, None) };
    }

    unsafe fn dealloc_typed(&self, ty: TypeId, ptr: NonNull<u8>, layout: Layout) {
        // SAFETY: forwarded unchanged from the caller.
        unsafe { self.dealloc_impl(ptr, layout, Some(ty)) };
    }

    fn alloc_unsized(&self, ty: TypeId, size: usize) -> Result<NonNull<u8>> {
        let layout = Layout::from_size_align(size, MIN_ALIGN)
            .map_err(|_| error::bad_align(MIN_ALIGN))?;
        self.alloc_impl(layout, Some(ty))
    }

    unsafe fn dealloc_unsized(&self, ty: TypeId, ptr: NonNull<u8>) {
        let payload = ptr.as_ptr();
        // The same bounded probe the trait default describes, driven by this
        // allocator's own `locate`/`plausible` header check: `MIN_ALIGN` (payload
        // offset 0, covering every alignment at or below it) followed by each power of
        // two up to the page size. The candidates have pairwise distinct payload
        // offsets, so at most one can match and the block is freed at most once; a
        // pointer nothing recognises is refused.
        for align in probe_alignments() {
            if Self::locate(payload, align).is_some() {
                if let Ok(layout) = Layout::from_size_align(0, align) {
                    // SAFETY: `ptr` is a live block of this allocator (the caller's
                    // contract) whose header `locate` has just accepted for `align`.
                    unsafe { self.dealloc_impl(ptr, layout, Some(ty)) };
                }
                return;
            }
        }
        beam_common::beam_debug!(
            "beam_alloc: refusing to free an unrecognised block at {payload:p}"
        );
    }

    fn type_stats(&self, ty: TypeId) -> TypeStats {
        // The per-type table lives in the allocator's counters (a mismatched typed
        // free saturates at zero rather than corrupting it), so the snapshot is the
        // only reader: `AllocStats::type_stats` is the accessor for one slot.
        self.state.lock().counters.snapshot().type_stats(ty.index())
    }

    fn allocator_info(&self) -> AllocatorInfo {
        let stats = self.stats();
        let mut info = AllocatorInfo::from_stats(report_name(self.name()), &stats);
        // A registry allocator is named after its `AllocatorId`, which is what tells it
        // which allocation types are bound to it; a directly-constructed allocator has
        // no id and reports only its per-allocator items.
        if let Some(id) = allocator_id_for_name(self.name()) {
            for ty in crate::types::types_for_allocator(id) {
                info.push_type(ty.name(), stats.type_stats(ty.index()));
            }
        }
        info
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

    // -- Typed (interposition) surface ---------------------------------------------

    /// The [`TypeId`] named `name` (every name used here is one of the 243 active
    /// types).
    fn type_id(name: &str) -> TypeId {
        TypeId::from_name(name).unwrap_or_else(|| panic!("type {name} must be active"))
    }

    /// A minimal implementor that overrides **nothing** of the typed surface, so every
    /// typed call exercises a trait default; it counts the `alloc`s that reach it and
    /// forwards everything to an inner [`CarrierAllocator`].
    struct Forwarding {
        inner: AllocRef,
        allocs: std::sync::atomic::AtomicUsize,
    }

    impl Allocator for Forwarding {
        fn alloc(&self, layout: Layout) -> Result<NonNull<u8>> {
            self.allocs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.inner.alloc(layout)
        }

        fn realloc(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_size: usize,
        ) -> Result<NonNull<u8>> {
            self.inner.realloc(ptr, old_layout, new_size)
        }

        unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
            // SAFETY: forwarded unchanged from the caller.
            unsafe { self.inner.dealloc(ptr, layout) };
        }

        fn stats(&self) -> AllocStats {
            self.inner.stats()
        }

        fn name(&self) -> &str {
            "forwarding"
        }
    }

    #[test]
    fn the_type_attributed_allocations_land_in_their_own_slot() {
        let alloc = allocator();
        let heap = type_id("HEAP");
        let msg = type_id("MSG");
        let untouched = type_id("ATOM");
        assert_ne!(heap, msg);

        let l = layout(64, 16);
        let a = alloc.alloc_typed(heap, l).expect("typed heap alloc");
        let b = alloc.alloc_typed(msg, l).expect("typed msg alloc");
        let stats = alloc.stats();
        assert_eq!(stats.alloc_calls, 2);
        assert_eq!(stats.live_blocks, 2);

        let heap_stats = alloc.type_stats(heap);
        let msg_stats = alloc.type_stats(msg);
        assert_eq!(heap_stats.blocks, 1);
        assert_eq!(msg_stats.blocks, 1);
        // Both requests are the same size class, so each type holds exactly one block
        // and the two slots add up to the allocator's live total.
        assert_eq!(heap_stats.bytes, msg_stats.bytes);
        assert_eq!(heap_stats.bytes + msg_stats.bytes, stats.live_bytes);
        assert_eq!(heap_stats.peak_bytes, heap_stats.bytes);
        // A type nobody allocated for stays at zero.
        assert_eq!(alloc.type_stats(untouched), TypeStats::default());

        // An *untyped* allocation is aggregate-only: it must not be charged to `heap`.
        let plain = alloc.alloc(l).expect("untyped alloc");
        assert_eq!(alloc.type_stats(heap).blocks, 1);
        assert_eq!(alloc.type_stats(heap).bytes, heap_stats.bytes);
        assert_eq!(alloc.stats().live_blocks, 3);
        // SAFETY: live allocation, freed exactly once, untyped as it was allocated.
        unsafe { alloc.dealloc(plain, l) };

        // SAFETY: both live typed allocations, freed exactly once each with the layout
        // they were allocated with.
        unsafe { alloc.dealloc_typed(heap, a, l) };
        // SAFETY: as above.
        unsafe { alloc.dealloc_typed(msg, b, l) };
        let after = alloc.stats();
        assert_eq!(after.live_blocks, 0);
        assert_eq!(after.live_bytes, 0);
        assert_eq!(after.free_calls, 3);
        assert_eq!(alloc.type_stats(heap).blocks, 0);
        assert_eq!(alloc.type_stats(heap).bytes, 0);
        assert_eq!(alloc.type_stats(msg).blocks, 0);
        assert_eq!(alloc.type_stats(msg).bytes, 0);
        // The per-type high-water mark survives the free.
        assert_eq!(alloc.type_stats(heap).peak_bytes, heap_stats.bytes);
        assert_eq!(alloc.type_stats(untouched), TypeStats::default());
    }

    #[test]
    fn realloc_typed_keeps_the_attribution_on_the_same_type() {
        let alloc = allocator();
        let msg = type_id("MSG");
        let other = type_id("HEAP");
        let l = layout(64, 16);
        let ptr = alloc.alloc_typed(msg, l).expect("alloc");

        // Shrinking within the block's capacity does not move it, so neither the block
        // nor its attribution changes.
        let same = alloc.realloc_typed(msg, ptr, l, 32).expect("shrink in place");
        assert_eq!(same, ptr);
        assert_eq!(alloc.type_stats(msg).blocks, 1);
        assert_eq!(alloc.stats().realloc_calls, 1);

        // Growing past the capacity moves the block through a real typed alloc + free,
        // both attributed to `msg`.
        let grown = alloc.realloc_typed(msg, ptr, l, 4097).expect("grow");
        assert_ne!(grown.as_ptr(), ptr.as_ptr());
        let stats = alloc.stats();
        assert_eq!(stats.realloc_calls, 2);
        assert_eq!(stats.live_blocks, 1);
        assert_eq!(stats.sbc_carriers, 1, "the grown block owns a dedicated carrier");
        assert_eq!(alloc.type_stats(msg).blocks, 1);
        assert_eq!(alloc.type_stats(msg).bytes, stats.live_bytes);
        assert_eq!(alloc.type_stats(other), TypeStats::default());

        // SAFETY: the live grown block, freed exactly once with its layout.
        unsafe { alloc.dealloc_typed(msg, grown, layout(4097, 16)) };
        assert_eq!(alloc.stats().live_bytes, 0);
        assert_eq!(alloc.type_stats(msg).blocks, 0);
        assert_eq!(alloc.type_stats(msg).bytes, 0);
    }

    #[test]
    fn alloc_unsized_round_trips_at_the_minimum_alignment() {
        let alloc = allocator();
        let ty = type_id("MSG");
        for size in [0usize, 1, 63, 64, 65, 1024, 4097] {
            let ptr = alloc.alloc_unsized(ty, size).expect("alloc_unsized");
            assert_eq!(ptr.as_ptr() as usize % MIN_ALIGN, 0, "size {size}");
            if size > 0 {
                // SAFETY: `size` writable bytes in a live allocation.
                unsafe { core::ptr::write_bytes(ptr.as_ptr(), 0xAB, size) };
            }
            assert_eq!(alloc.type_stats(ty).blocks, 1);
            let before = alloc.stats();
            assert_eq!(before.live_blocks, 1);
            // SAFETY: the live typed allocation, freed exactly once without a layout.
            unsafe { alloc.dealloc_unsized(ty, ptr) };
            let after = alloc.stats();
            assert_eq!(after.live_blocks, 0, "size {size}");
            assert_eq!(after.live_bytes, 0, "size {size}");
            // The slot's block/byte counters return to zero; the high-water mark is
            // the per-type history and is kept.
            assert_eq!(alloc.type_stats(ty).blocks, 0, "size {size}");
            assert_eq!(alloc.type_stats(ty).bytes, 0, "size {size}");
            assert!(alloc.type_stats(ty).peak_bytes > 0, "size {size}");
        }
        let stats = alloc.stats();
        assert_eq!(stats.live_bytes, 0);
        // The over-threshold last request owned a dedicated carrier that went back to
        // the OS; the slab carriers the smaller requests mapped stay (M1 policy).
        assert_eq!(stats.sbc_carriers, 0);
        assert_eq!(stats.carriers, stats.sbc_carriers + stats.mbc_carriers);
    }

    #[test]
    fn dealloc_unsized_recovers_every_supported_alignment() {
        let alloc = allocator();
        let ty = type_id("HEAP");
        // Over-aligned requests take the large (dedicated single-block carrier) path,
        // whose payload offset differs per alignment; the plain size takes the slab
        // path (offset zero).
        for align in [16usize, 32, 64, 256, 4096] {
            let l = layout(2000, align);
            let ptr = alloc.alloc_typed(ty, l).expect("alloc");
            assert_eq!(ptr.as_ptr() as usize % align, 0, "align {align}");
            // SAFETY: the live typed allocation, freed exactly once without a layout.
            unsafe { alloc.dealloc_unsized(ty, ptr) };
            assert_eq!(alloc.stats().live_blocks, 0, "align {align}");
        }
        let l = layout(64, 16);
        let slab = alloc.alloc_typed(ty, l).expect("slab alloc");
        // SAFETY: as above.
        unsafe { alloc.dealloc_unsized(ty, slab) };
        let stats = alloc.stats();
        assert_eq!(stats.live_bytes, 0);
        // Every dedicated carrier went back to the OS; only the slab carrier the last
        // (multi-block) allocation mapped is retained.
        assert_eq!(stats.sbc_carriers, 0);
        assert_eq!(stats.mbc_carriers, 1);
        assert_eq!(stats.carriers, stats.sbc_carriers + stats.mbc_carriers);
        assert_eq!(alloc.type_stats(ty).blocks, 0);
        assert_eq!(alloc.type_stats(ty).bytes, 0);
    }

    #[test]
    fn over_aligned_large_blocks_leave_their_alignment_padding_zeroed() {
        let alloc = allocator();
        let ty = type_id("HEAP");
        // `mseg`'s reuse cache hands a released segment back at the *same* address and
        // does not zero it, so the padding in front of an over-aligned payload can hold
        // a previous owner's block header unless `alloc_large` clears it. A stale but
        // plausible header there would be matched by the first candidate of the
        // unsized-dealloc probe — see
        // `a_reused_segment_cannot_resurrect_a_stale_header_in_the_padding` for the
        // end-to-end sequence.
        for align in [32usize, 64, 256, 4096] {
            let l = layout(2000, align);
            let ptr = alloc.alloc_typed(ty, l).expect("alloc");
            let payload = ptr.as_ptr();
            assert_eq!(payload as usize % align, 0, "align {align}");
            // The payload sits `BLOCK_HEADER_SIZE + padding_for(align)` bytes after the
            // block start, which is exactly `align` bytes after it.
            let base = payload.wrapping_sub(align);
            let padding = align - BLOCK_HEADER_SIZE;
            assert_eq!(padding, padding_for(align), "align {align}");
            // SAFETY: the padding is `padding_for(align)` bytes inside the live
            // dedicated carrier this block owns, between its own header and its
            // payload; it is neither handed to the caller nor overlapped by anything.
            let bytes =
                unsafe { core::slice::from_raw_parts(base.add(BLOCK_HEADER_SIZE), padding) };
            assert!(
                bytes.iter().all(|&b| b == 0),
                "align {align}: the alignment padding must be zeroed"
            );
            // The probe's first candidate (`MIN_ALIGN`, payload offset 0) reads exactly
            // at `payload - MIN_ALIGN`; it must no longer look like one of our headers.
            assert!(
                !plausible_header_at(payload.wrapping_sub(MIN_ALIGN), padding_for(MIN_ALIGN)),
                "align {align}: the padding must not decode as a block header"
            );
            // ... and the probe still finds the real header, so the block frees fully.
            // SAFETY: the live typed allocation, freed exactly once without a layout.
            unsafe { alloc.dealloc_unsized(ty, ptr) };
            assert_eq!(alloc.stats().live_blocks, 0, "align {align}");
            assert_eq!(alloc.stats().live_bytes, 0, "align {align}");
        }
        let stats = alloc.stats();
        assert_eq!(stats.sbc_carriers, 0);
        assert_eq!(stats.carriers, stats.sbc_carriers + stats.mbc_carriers);
    }

    #[test]
    fn a_reused_segment_cannot_resurrect_a_stale_header_in_the_padding() {
        let alloc = allocator();
        let ty = type_id("HEAP");
        let l = layout(2000, 256);

        // A previous owner of the segment leaves a plausible-looking block header at
        // exactly the position the unsized-dealloc probe reads first for its
        // `MIN_ALIGN` candidate (`payload - MIN_ALIGN`). `mseg`'s reuse cache does not
        // zero the segment it hands back, so only the allocator's own padding zeroing
        // can remove it.
        let first = alloc.alloc_typed(ty, l).expect("first alloc");
        let base = first.as_ptr().wrapping_sub(256);
        let stale = BlockHdr {
            size: 80,
            payload_offset: 0,
            class: 0,
            carrier: base.cast::<c_void>(),
            flags: {
                let mut flags = BlockFlags::empty();
                flags.set_free();
                flags.bits()
            },
        };
        // SAFETY: `first` is a live over-aligned block, so `payload - MIN_ALIGN` lies
        // inside its own alignment padding — allocator-internal memory that is never
        // handed out, so no live payload is overwritten.
        unsafe { BlockHeader::store(first.as_ptr().wrapping_sub(MIN_ALIGN), stale) };
        assert!(
            plausible_header_at(
                first.as_ptr().wrapping_sub(MIN_ALIGN),
                padding_for(MIN_ALIGN)
            ),
            "the planted header must be one the probe would accept"
        );

        // Freeing with the correct layout hands the poisoned segment to the reuse cache.
        // SAFETY: the live typed allocation, freed exactly once with its own layout.
        unsafe { alloc.dealloc_typed(ty, first, l) };
        assert_eq!(alloc.stats().live_bytes, 0);

        // The next allocation of the same size is the cache's best (smallest fitting)
        // candidate, so it usually gets that very segment back, stale bytes and all.
        // `mseg` is a process-wide singleton shared with the concurrently running
        // tests, so the segment may have been taken by another test in between; that
        // case is reported and *not* asserted (the invariant test above is the
        // deterministic guarantee), so this test never fails merely because the cache
        // was not reused.
        let second = alloc.alloc_typed(ty, l).expect("second alloc");
        if second.as_ptr() == first.as_ptr() {
            assert!(
                !plausible_header_at(
                    second.as_ptr().wrapping_sub(MIN_ALIGN),
                    padding_for(MIN_ALIGN)
                ),
                "the reused segment's stale header must have been cleared"
            );
        } else {
            eprintln!(
                "beam_alloc: mseg did not reuse the released segment ({:p} -> {:p}); \
                 skipping the reuse assertion",
                first.as_ptr(),
                second.as_ptr()
            );
        }

        // Whichever segment the block landed on, the unsized free must recover every
        // byte (with the stale header gone it matches the real header at the block
        // start, the only candidate left).
        // SAFETY: the live typed allocation, freed exactly once without a layout.
        unsafe { alloc.dealloc_unsized(ty, second) };
        let stats = alloc.stats();
        assert_eq!(stats.live_bytes, 0);
        assert_eq!(stats.live_blocks, 0);
        assert_eq!(stats.sbc_carriers, 0);
        assert_eq!(alloc.type_stats(ty).blocks, 0);
        assert_eq!(alloc.type_stats(ty).bytes, 0);
    }

    #[test]
    fn dealloc_unsized_leaves_a_foreign_pointer_alone() {
        let alloc = allocator();
        let ty = type_id("MSG");
        let live = alloc.alloc_typed(ty, layout(64, 16)).expect("alloc");
        // A zeroed buffer big enough that even the widest probe reads inside it: the
        // pointer sits 32 KiB in, and the probe reaches at most `os_page_size()` below.
        let buffer = vec![0u8; 64 * 1024];
        let bogus = NonNull::new(unsafe { buffer.as_ptr().add(32 * 1024) as *mut u8 })
            .expect("non-null");
        let before = alloc.stats();
        // SAFETY: `bogus` is not a block this allocator handed out; the call must
        // detect that (every candidate header is zeroed, hence no size) and leave the
        // allocator's state untouched.
        unsafe { alloc.dealloc_unsized(ty, bogus) };
        assert_eq!(alloc.stats(), before);
        assert_eq!(alloc.type_stats(ty).blocks, 1);
        // SAFETY: the live typed allocation, freed exactly once.
        unsafe { alloc.dealloc_unsized(ty, live) };
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn the_dealloc_probe_is_bounded_and_uses_distinct_payload_offsets() {
        let aligns: Vec<usize> = probe_alignments().collect();
        assert_eq!(aligns.first().copied(), Some(MIN_ALIGN));
        assert_eq!(aligns.last().copied(), Some(os_page_size()));
        // Exactly one candidate per payload offset: 1 + log2(page / UNIT_SIZE).
        assert_eq!(
            aligns.len(),
            1 + (os_page_size() / MIN_ALIGN).ilog2() as usize
        );
        assert!(aligns.len() <= 1 + 16, "the probe stays small");
        assert!(aligns.iter().all(|a| a.is_power_of_two() && *a <= os_page_size()));
        assert!(aligns.windows(2).all(|w| w[1] == w[0] * 2));
        let offsets: Vec<usize> = aligns.iter().map(|a| padding_for(*a)).collect();
        let mut unique = offsets.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(
            unique.len(),
            offsets.len(),
            "payload offsets must be pairwise distinct"
        );
    }

    #[test]
    fn carriers_split_into_sbc_and_mbc_and_the_totals_agree() {
        let alloc = allocator();
        let ty = type_id("MSG");
        let baseline = alloc.stats();
        assert_eq!(baseline.carriers, baseline.sbc_carriers + baseline.mbc_carriers);

        // A slab request maps one multi-block carrier.
        let small = alloc.alloc_typed(ty, layout(64, 16)).expect("slab alloc");
        let after_slab = alloc.stats();
        assert_eq!(after_slab.mbc_carriers, baseline.mbc_carriers + 1);
        assert_eq!(after_slab.sbc_carriers, baseline.sbc_carriers);
        assert_eq!(after_slab.carriers, baseline.carriers + 1);
        assert_eq!(
            after_slab.carriers,
            after_slab.sbc_carriers + after_slab.mbc_carriers
        );
        assert!(after_slab.carrier_bytes > baseline.carrier_bytes);

        // A request past the size-class threshold maps a dedicated single-block
        // carrier.
        let large = alloc.alloc_typed(ty, layout(4097, 16)).expect("large alloc");
        let after_large = alloc.stats();
        assert_eq!(after_large.sbc_carriers, baseline.sbc_carriers + 1);
        assert_eq!(after_large.mbc_carriers, after_slab.mbc_carriers);
        assert_eq!(after_large.carriers, after_slab.carriers + 1);
        assert_eq!(
            after_large.carriers,
            after_large.sbc_carriers + after_large.mbc_carriers
        );
        let peak = after_large.peak_carrier_bytes;
        assert!(peak >= after_large.carrier_bytes);

        // Freeing it releases exactly the single-block carrier (the slab carrier is
        // retained, the M1 policy), and the aggregate follows the split.
        // SAFETY: the live large typed allocation, freed exactly once.
        unsafe { alloc.dealloc_typed(ty, large, layout(4097, 16)) };
        let after_free = alloc.stats();
        assert_eq!(after_free.sbc_carriers, baseline.sbc_carriers);
        assert_eq!(after_free.mbc_carriers, baseline.mbc_carriers + 1);
        assert_eq!(after_free.carriers, baseline.carriers + 1);
        assert_eq!(
            after_free.carriers,
            after_free.sbc_carriers + after_free.mbc_carriers
        );
        assert!(after_free.carrier_bytes < after_large.carrier_bytes);
        assert_eq!(after_free.peak_carrier_bytes, peak);

        // SAFETY: the remaining live typed allocation, freed exactly once.
        unsafe { alloc.dealloc_typed(ty, small, layout(64, 16)) };
        let done = alloc.stats();
        // The slab carrier is retained even once fully free (the M1 policy), so only
        // the split invariant is testable here.
        assert_eq!(done.mbc_carriers, baseline.mbc_carriers + 1);
        assert_eq!(done.sbc_carriers, baseline.sbc_carriers);
        assert_eq!(done.carriers, baseline.carriers + 1);
        assert_eq!(done.carriers, done.sbc_carriers + done.mbc_carriers);
    }

    #[test]
    fn allocator_info_lists_the_rows_of_every_type_bound_to_the_allocator() {
        use crate::stats::InfoValue;

        let config = AllocatorConfig::builder()
            .name(AllocatorId::EHeap.as_str())
            .carrier_size(64 * 1024)
            .build();
        let alloc = CarrierAllocator::new(config).expect("allocator");
        let heap = type_id("HEAP");
        let l = layout(64, 16);
        let ptr = alloc.alloc_typed(heap, l).expect("alloc");

        let info = alloc.allocator_info();
        assert_eq!(info.name, "eheap_alloc");
        let bound = crate::types::types_for_allocator(AllocatorId::EHeap).count();
        assert_eq!(info.items.len(), 13 + 4 * bound);
        assert_eq!(info.items[0].key, "blocks");

        // Every type bound to `eheap_alloc` gets a four-item row set; exactly the one
        // that was allocated for is non-zero.
        let mut non_zero = 0usize;
        let mut seen_heap = false;
        for row in info.items[13..].chunks_exact(4) {
            assert_eq!(row[0].key, "type");
            assert_eq!(row[1].key, "type blocks");
            assert_eq!(row[2].key, "type bytes");
            assert_eq!(row[3].key, "type peak_bytes");
            let InfoValue::Text(name) = row[0].value else {
                panic!("the type header must be text");
            };
            let expected = alloc.type_stats(type_id(name));
            assert_eq!(row[1].value, InfoValue::U64(expected.blocks), "{name}");
            assert_eq!(row[2].value, InfoValue::Usize(expected.bytes), "{name}");
            assert_eq!(row[3].value, InfoValue::Usize(expected.peak_bytes), "{name}");
            if expected.blocks > 0 {
                non_zero += 1;
                assert_eq!(name, "HEAP");
                seen_heap = true;
            }
        }
        assert!(seen_heap, "the allocated-for type must appear");
        assert_eq!(non_zero, 1, "only the allocated-for type is charged");

        // SAFETY: the live typed allocation, freed exactly once.
        unsafe { alloc.dealloc_typed(heap, ptr, l) };
    }

    #[test]
    fn a_directly_constructed_allocator_reports_no_type_rows() {
        let alloc = allocator();
        let info = alloc.allocator_info();
        // The configured name is not a declared `AllocatorId`, so there is no type
        // listing to add — and the per-allocator items are still complete.
        assert_eq!(info.name, "test");
        assert_eq!(info.items.len(), 13);
        assert_eq!(info.items[0].key, "blocks");
        assert_eq!(info.items[2].key, "carriers");
        assert_eq!(info.items[4].key, "sbcs carriers");
        assert_eq!(info.items[5].key, "mbcs carriers");
    }

    #[test]
    fn the_typed_defaults_forward_to_the_untyped_methods() {
        let inner = allocator().share();
        let subject = Forwarding {
            inner,
            allocs: std::sync::atomic::AtomicUsize::new(0),
        };
        assert_eq!(subject.name(), "forwarding");
        let ty = type_id("MSG");
        let l = layout(64, 16);

        // `alloc_unsized` -> `alloc_bytes` -> `alloc`.
        let ptr = subject.alloc_unsized(ty, 64).expect("alloc_unsized");
        assert_eq!(
            subject.allocs.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the default must forward to `alloc`"
        );
        // A default that records no type data reports the zero value.
        assert_eq!(subject.type_stats(ty), TypeStats::default());
        // ... and a default report carries the per-allocator items only.
        let info = subject.allocator_info();
        assert_eq!(info.name, "forwarding");
        assert_eq!(info.items.len(), 13);

        // The default `dealloc_unsized` probe recognises the block through its in-band
        // header and forwards to `dealloc`.
        // SAFETY: the live allocation, freed exactly once through the default.
        unsafe { subject.dealloc_unsized(ty, ptr) };
        assert_eq!(subject.stats().live_bytes, 0);
        assert_eq!(subject.stats().live_blocks, 0);

        // `alloc_typed`/`realloc_typed`/`dealloc_typed` forward too.
        let typed = subject.alloc_typed(ty, l).expect("alloc_typed");
        let grown = subject
            .realloc_typed(ty, typed, l, 128)
            .expect("realloc_typed");
        // SAFETY: the live (possibly moved) allocation, freed exactly once.
        unsafe { subject.dealloc_typed(ty, grown, layout(128, 16)) };
        assert_eq!(subject.stats().live_bytes, 0);
    }
}
