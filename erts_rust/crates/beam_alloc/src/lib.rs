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
//! [`registry`], [`stats`] and [`types`] (the `erts_alloc` allocator/class/type
//! inventory); [`block`], [`carrier`], [`free_list`], [`mbc`], [`strategy`],
//! [`policy`] and [`mseg`] are implementation modules (public for testing and reuse,
//! but not part of the contract — [`policy`]'s [`CarrierPolicy`] is allocator
//! *configuration*, the same role [`strategy`]'s [`Strategy`] plays for
//! [`crate::allocator::AllocatorConfig`]). Errors are always
//! `beam_common::{Result, BeamError}` — this crate adds no error type of its own.
//!
//! [`Eterm`]: beam_term::prelude::Eterm

pub mod block;
pub mod carrier;
pub mod free_list;
pub mod mbc;
pub mod size_class;
pub mod strategy;
pub mod policy;
pub mod stats;
pub mod types;
pub mod mseg;
pub mod allocator;
pub mod registry;
pub mod heap;
pub mod reserved_heap;
pub mod refcount;
pub mod binary;
mod bump; // pub(crate) primitives only — NOT public
mod error; // pub(crate) helpers only — NOT public

pub use crate::allocator::{
    AllocRef, Allocator, AllocatorConfig, AllocatorConfigBuilder, CarrierAllocator, MIN_ALIGN,
    WORD_ALIGN,
};
pub use crate::binary::RefcBinary;
pub use crate::heap::Heap;
pub use crate::policy::{CarrierPolicy, PolicyOptionKind};
pub use crate::refcount::RefCount;
pub use crate::registry::{all_stats, allocator, default_allocator, AllocType};
pub use crate::reserved_heap::{GrowOutcome, ReservedHeap};
pub use crate::size_class::SizeClasses;
pub use crate::stats::AllocStats;
pub use crate::strategy::Strategy;
pub use crate::types::{
    type_by_name, types_for_allocator, AllocatorId, ClassId, TypeEntry, TypeFlags, TypeId,
    ALLOC_TYPES, EXPECTED_TYPE_COUNT, TYPE_COUNT,
};

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::allocator::{
        Allocator, AllocRef, AllocatorConfig, CarrierAllocator, MIN_ALIGN, WORD_ALIGN,
    };
    pub use crate::binary::RefcBinary;
    pub use crate::heap::Heap;
    pub use crate::refcount::RefCount;
    pub use crate::registry::{all_stats, allocator, default_allocator, AllocType};
    pub use crate::reserved_heap::{GrowOutcome, ReservedHeap};
    pub use crate::size_class::SizeClasses;
    pub use crate::stats::AllocStats;
    pub use crate::types::{
        type_by_name, types_for_allocator, AllocatorId, ClassId, TypeEntry, TypeFlags, TypeId,
        ALLOC_TYPES, TYPE_COUNT,
    };
}
