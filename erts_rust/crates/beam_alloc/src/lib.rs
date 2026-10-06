#![doc = "Allocator layer (erts_alloc replacement): carriers, size classes and the per-process heap bump allocator."]

//! The allocation layer of the Rust ERTS rewrite.
//!
//! This crate owns two responsibilities (Stage-0 foundations, Part II §H):
//!
//! 1. **Byte allocation** for anything off the process heap — ETS nodes, code,
//!    binaries and the carriers themselves — as a carrier-based multi-block
//!    allocator, mirroring the legacy `erts_alloc` / `erl_alloc_util.c` framework.
//!    Dependents allocate through the cheaply-clonable shared handle
//!    [`AllocRef`] and never touch carriers, blocks or free lists directly.
//! 2. **Term allocation** inside a fixed per-process heap ([`Heap`]) — the
//!    *production* bump arena the copy engine (`beam_message`) and the collector
//!    (`beam_gc`) construct terms in. It never reallocates: it is grown by
//!    *replacement* (GC), never by moving live terms (workspace `CONTEXT.md` §2).
//!
//! # Dependencies
//!
//! `beam_common` (the workspace error type), `beam_term` (the frozen [`Eterm`]
//! word), `libc` (the `mmap`-backed carriers) and `parking_lot` (the per-allocator
//! mutex). Nothing else.
//!
//! # The frozen seam
//!
//! The cross-crate contract is [`allocator`], [`heap`], [`size_class`],
//! [`registry`] and [`stats`]; [`block`], [`carrier`], [`free_list`] and
//! [`strategy`] are implementation modules (public for testing and reuse, but not
//! part of the contract). Errors are always `beam_common::{Result, BeamError}` —
//! this crate adds no error type of its own.
//!
//! [`Eterm`]: beam_term::prelude::Eterm

pub mod block;
pub mod carrier;
pub mod free_list;
pub mod size_class;
pub mod strategy;
pub mod stats;
pub mod allocator;
pub mod registry;
pub mod heap;
pub mod refcount;
pub mod binary;
mod error; // pub(crate) helpers only — NOT public

pub use crate::allocator::{
    AllocRef, Allocator, AllocatorConfig, AllocatorConfigBuilder, CarrierAllocator, MIN_ALIGN,
    WORD_ALIGN,
};
pub use crate::binary::RefcBinary;
pub use crate::heap::Heap;
pub use crate::refcount::RefCount;
pub use crate::registry::{all_stats, allocator, default_allocator, AllocType};
pub use crate::size_class::SizeClasses;
pub use crate::stats::AllocStats;
pub use crate::strategy::Strategy;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::allocator::{
        Allocator, AllocRef, AllocatorConfig, CarrierAllocator, MIN_ALIGN, WORD_ALIGN,
    };
    pub use crate::binary::RefcBinary;
    pub use crate::heap::Heap;
    pub use crate::refcount::RefCount;
    pub use crate::registry::{all_stats, allocator, default_allocator, AllocType};
    pub use crate::size_class::SizeClasses;
    pub use crate::stats::AllocStats;
}
