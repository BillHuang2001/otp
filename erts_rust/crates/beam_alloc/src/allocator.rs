//! The allocator seam: the [`Allocator`] trait, its shared handle [`AllocRef`], the
//! [`AllocatorConfig`] builder and the concrete [`CarrierAllocator`].
//!
//! This is the **frozen cross-crate contract** of the crate. Dependents
//! (`beam_codec`, `beam_ets`, `beam_gc`, `beam_message`, `beam_process`) allocate
//! through an [`AllocRef`] and never touch carriers, blocks or free lists directly,
//! so the carrier framework can evolve behind this interface.
//!
//! `unsafe` is confined to this module's marked functions.

use core::alloc::Layout;
use std::ptr::NonNull;
use std::sync::Arc;

use beam_common::Result;
use parking_lot::Mutex;

use crate::block::UNIT_SIZE;
use crate::carrier::Carrier;
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
    fn realloc(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>>;

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

/// The mutable, mutex-guarded state of a [`CarrierAllocator`].
#[derive(Default)]
struct AllocatorState {
    /// Carriers currently mapped, grouped by class (M1 retains empty carriers).
    carriers: Vec<Carrier>,
    /// The per-size-class free-block lists.
    free_lists: FreeList,
    /// Instrumentation counters.
    counters: Counters,
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
}

impl Allocator for CarrierAllocator {
    fn alloc(&self, layout: Layout) -> Result<NonNull<u8>> {
        let _ = layout;
        unimplemented!(
            "CarrierAllocator::alloc: carrier selection + block carving (allocator implementation pass)"
        )
    }

    fn realloc(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_size: usize,
    ) -> Result<NonNull<u8>> {
        let _ = (ptr, old_layout, new_size);
        unimplemented!(
            "CarrierAllocator::realloc: in-place growth + copy-on-move (allocator implementation pass)"
        )
    }

    unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout) {
        let _ = (ptr, layout);
        unimplemented!(
            "CarrierAllocator::dealloc: block return to the carrier free list (allocator implementation pass)"
        )
    }

    fn stats(&self) -> AllocStats {
        self.state.lock().counters.snapshot()
    }

    fn name(&self) -> &str {
        &self.config.name
    }
}
