//! The process-wide allocator registry (`erts_allctrs` equivalent).
//!
//! Mirrors the legacy one-allocator-per-`AllocType` scheme of
//! `erts/emulator/beam/erl_alloc.types`: the runtime's allocation classes are
//! addressed by a stable name and resolved through a lazily-initialised, one-time
//! table, so `erts_alloc(TYPE, size)` (and its Rust replacement) has a single
//! allocator instance per type.
//!
//! In Milestone 1 every type uses the same default [`AllocatorConfig`]; per-type
//! tuning and thread-specific instances are documented gaps.

use std::sync::{Arc, LazyLock};

use crate::allocator::{AllocRef, AllocatorConfig, CarrierAllocator};
use crate::stats::AllocStats;

/// The allocation classes the runtime registers.
///
/// Curated from the `allocator` records of `erts/emulator/beam/erl_alloc.types`
/// (the canonical names are returned by [`AllocType::as_str`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum AllocType {
    /// Long-lived, system-wide allocations (`sys_alloc`).
    System,
    /// Very short-lived temporary allocations (`temp_alloc`).
    Temporary,
    /// Short-lived allocations (`sl_alloc`).
    ShortLived,
    /// General-purpose allocations (`std_alloc`).
    Standard,
    /// Long-lived allocations (`ll_alloc`).
    LongLived,
    /// Erlang process heaps (`eheap_alloc`).
    EHeap,
    /// ETS table data (`ets_alloc`).
    Ets,
    /// Literals / constant terms (`literal_alloc`).
    Literal,
    /// Off-heap binaries (`binary_alloc`).
    Binary,
    /// Port drivers (`driver_alloc`).
    Driver,
    /// Fixed-size blocks (`fix_alloc`).
    FixedSize,
    /// Test allocators (`test_alloc`).
    Test,
}

impl AllocType {
    /// Every allocation type, in registration order.
    pub const ALL: [AllocType; 12] = [
        AllocType::System,
        AllocType::Temporary,
        AllocType::ShortLived,
        AllocType::Standard,
        AllocType::LongLived,
        AllocType::EHeap,
        AllocType::Ets,
        AllocType::Literal,
        AllocType::Binary,
        AllocType::Driver,
        AllocType::FixedSize,
        AllocType::Test,
    ];

    /// The stable allocator name of this type (as in `erl_alloc.types`).
    pub fn as_str(&self) -> &'static str {
        match self {
            AllocType::System => "sys_alloc",
            AllocType::Temporary => "temp_alloc",
            AllocType::ShortLived => "sl_alloc",
            AllocType::Standard => "std_alloc",
            AllocType::LongLived => "ll_alloc",
            AllocType::EHeap => "eheap_alloc",
            AllocType::Ets => "ets_alloc",
            AllocType::Literal => "literal_alloc",
            AllocType::Binary => "binary_alloc",
            AllocType::Driver => "driver_alloc",
            AllocType::FixedSize => "fix_alloc",
            AllocType::Test => "test_alloc",
        }
    }

    /// The index of this type in [`AllocType::ALL`].
    pub fn index(&self) -> usize {
        *self as usize
    }
}

/// One lazily-created allocator per [`AllocType`], indexed by [`AllocType::index`].
static ALLOCATORS: LazyLock<[AllocRef; 12]> = LazyLock::new(|| {
    AllocType::ALL.map(|ty| -> AllocRef {
        let config = AllocatorConfig::builder().name(ty.as_str()).build();
        // `CarrierAllocator::new` only reserves bookkeeping and maps no carrier, so a
        // failure here means a broken build, not runtime OOM. This `expect` is the
        // SINGLE permitted panic on the allocator path (registry initialisation);
        // the per-call allocator API never panics on OOM — it returns
        // `BeamError::Alloc` (see the crate `CONTEXT.md`, Design Decisions).
        Arc::new(
            CarrierAllocator::new(config)
                .expect("beam_alloc registry: CarrierAllocator::new must not fail"),
        )
    })
});

/// The shared allocator registered for `ty`.
pub fn allocator(ty: AllocType) -> &'static AllocRef {
    &ALLOCATORS[ty.index()]
}

/// The default allocator (the [`AllocType::Standard`] instance).
pub fn default_allocator() -> &'static AllocRef {
    allocator(AllocType::Standard)
}

/// A snapshot of every registered allocator's counters.
pub fn all_stats() -> Vec<(AllocType, AllocStats)> {
    AllocType::ALL
        .iter()
        .map(|&ty| (ty, allocator(ty).stats()))
        .collect()
}
