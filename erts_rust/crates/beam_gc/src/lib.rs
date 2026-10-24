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

#![doc = "Per-process generational semi-space copying GC, MSO sweep and heap sizing."]

//! Per-process garbage collector of the Rust ERTS rewrite (Stage 4).
//!
//! Owns the collector that the runtime drives at safepoints: a **generational
//! semi-space Cheney copying** collector over the per-process heap, **barrier-free
//! by construction** (Erlang terms are immutable, so a mature object can never refer
//! to a later-allocated young object and the mutator pays no per-store overhead),
//! the off-heap **refcounted large-object (MSO)** interface, and the process
//! **heap-size policy**. The root set is the flat contract of the crate layer:
//! `x_reg_array[0..Live)` plus the whole stack, with **no frame liveness maps**.
//! MMTk and Boehm are rejected; GenImmix is a measurement-gated mature-generation
//! option only. [`GenImmixCollector`] is the integrated, config-selectable form of that
//! option (copying nursery + mark-region mature), [`AnyCollector`] dispatches between
//! it and the default copying [`Collector`] on the configured
//! [`MatureKind`](config::MatureKind), and [`selection`] is the **measurement-derived
//! automatic rule** that chooses between them from the mature generation's size — the
//! default selection — with [`MatureSelection::Explicit`](config::MatureSelection::Explicit)
//! as the user override.
//!
//! Implements the GC half of Part II §D and the Stage-4 row of Part II §H; the
//! legacy `erts/emulator/beam/erl_gc.c` + `copy.c` are the ground-truth behaviour.
//!
//! `unsafe` is confined to `copy.rs` and `immix_collector.rs` (the word-level copier and
//! the promotion driver), `relocate.rs` (the region-generic relocation driver and its
//! host-owned flat destination), `reachable.rs` (the read-only, whole-heap reachability walk and
//! its one-bit-per-word visited set), `live.rs` (the exact-live counter a fullsweep sizes
//! itself from), `immix/` (the mark-region space's word-level mark/evacuation
//! walks), the raw-pointer range tests in `space.rs` (plus the `FromSet::from_ranges`
//! constructor, whose `unsafe` is the caller's assertion about a foreign region) and the two
//! narrow `NonNull`-deref / `Box::from_raw` blocks in `mso.rs`; `policy.rs`, `stats.rs`,
//! `bounds.rs`, `footprint.rs`, `offheap.rs` and the collector driving logic are safe Rust.
//!
//! # Region-generic relocating compaction
//!
//! [`Relocator`]/[`compact`] drive the same Cheney copy engine over a destination the **host**
//! owns, so a foreign word region (a `beam_alloc::ReservedHeap`, a pre-reserved `Vec<Eterm>`,
//! an mmap'd arena, the engine's term arena) can be compacted without a process heap: the
//! from-set is built from raw ranges ([`FromSet::from_ranges`]), the destination implements
//! [`EvacDest`] (or is the turnkey [`LinearDest`]), and the two walking helpers
//! [`live_words_in`]/[`for_each_reachable_in`] size and inspect it. Collection behaviour is
//! unaffected — nothing on the [`Collector`]/[`GenImmixCollector`]/[`AnyCollector`] path reads
//! this surface.

pub mod any_collector;
pub mod bin_vheap;
pub mod bounds;
pub mod collector;
pub mod config;
pub mod copy;
mod error; // pub(crate) helpers only — NOT public
pub mod footprint;
pub mod heap_info;
pub mod immix;
pub mod immix_collector;
pub(crate) mod live; // the read-only live-graph counter — NOT public
pub mod mso;
mod nursery; // the young nursery chain (the no-relocation growth seam) — NOT public
pub mod offheap;
pub mod policy;
pub(crate) mod reachable;
pub mod relocate; // the read-only whole-heap reachability walk — the `*_in` helpers are public
pub use crate::reachable::{for_each_reachable_in, ReachableObject}; // the walk itself stays crate-internal
pub mod selection;
pub mod space;
pub mod stats;

pub use crate::any_collector::*;
pub use crate::bin_vheap::BinVheap;
pub use crate::bounds::ScannedBounds;
pub use crate::collector::*;
pub use crate::config::*;
pub use crate::copy::*;
pub use crate::footprint::*;
pub use crate::heap_info::*;
pub use crate::immix::*;
pub use crate::immix_collector::*;
pub use crate::live::live_words_in;
pub use crate::mso::*;
pub use crate::offheap::{for_each_reachable_binary_payload, sweep_unreachable_binaries};
pub use crate::policy::*;
pub use crate::relocate::*;
pub use crate::selection::*;
pub use crate::space::*;
pub use crate::stats::*;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::any_collector::*;
    pub use crate::bin_vheap::BinVheap;
    pub use crate::bounds::ScannedBounds;
    pub use crate::collector::*;
    pub use crate::config::*;
    pub use crate::copy::*;
    pub use crate::footprint::*;
    pub use crate::heap_info::*;
    pub use crate::immix::*;
    pub use crate::immix_collector::*;
    pub use crate::live::live_words_in;
    pub use crate::mso::*;
    pub use crate::offheap::{for_each_reachable_binary_payload, sweep_unreachable_binaries};
    pub use crate::policy::*;
    pub use crate::reachable::{for_each_reachable_in, ReachableObject};
    pub use crate::relocate::*;
    pub use crate::selection::*;
    pub use crate::space::*;
    pub use crate::stats::*;
}
