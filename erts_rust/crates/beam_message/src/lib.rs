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

#![doc = "Copy engine (copy_struct) and messaging: send/receive, fragments and message queues."]

//! The copy engine and data-level messaging of the Rust ERTS rewrite (Stage 3).
//!
//! Owns the **copy half of the process memory model** — [`copy_struct`], the iterative copy of a
//! term graph from one process heap into another, and its sharing-preserving sibling
//! [`copy_struct_shared`] — plus the messaging primitives built on it: the [`Message`] envelope,
//! the per-process FIFO [`Queue`] and [`send`]/[`receive`]. Implements Part II §C/§D
//! (immutability bounds what may be shared; everything else is copied) and the Stage-3 row of
//! Part II §H; the legacy `erts/emulator/beam/copy.c` and `erl_message.c` are the ground-truth
//! behaviour.
//!
//! `unsafe` is confined to `copy.rs` (the raw word read/write of the copy loop) and the narrow
//! `layout.rs` accessors; `share.rs`, `fragment.rs`, `message.rs`, `mailbox.rs`, `off_heap.rs`,
//! `node.rs` and `error.rs` are safe Rust.
//!
//! # Allocation-failure classification
//!
//! A copy into a **destination heap** that cannot hold the copied term fails with
//! [`BeamError::Alloc`](beam_common::BeamError::Alloc) whose
//! [`AllocFailure::kind`](beam_common::AllocFailure) is
//! [`AllocKind::Message`](beam_common::AllocKind) and whose requested byte count is
//! `words * size_of::<Eterm>()`. This is a **soft, recoverable** signal: a host may collect and
//! grow the destination heap and retry, or -- at its configured hard limit (the emulator's
//! `+hmax` / `max_heap_size`) -- raise the emulator's documented **`error:system_limit`** (class
//! `error`, reason atom `system_limit`). Classify it with the public predicate
//! [`is_message_heap_exhaustion`] rather than matching the [`AllocKind`](beam_common::AllocKind)
//! at each call site.

pub mod copy;
#[cfg(test)]
mod copy_reference; // the retired destination-pricing walk, kept only as the equivalence oracle
mod error; // pub(crate) copy-failure constructors — NOT public
pub mod fragment;
mod layout; // pub(crate) layout helpers — NOT part of the public API
pub mod mailbox;
pub mod message;
mod node;
pub mod off_heap;
mod share;

pub use crate::copy::{copy_struct, copy_struct_simple, size_of_copy, CopyContext};
pub use crate::error::is_message_heap_exhaustion;
pub use crate::fragment::{Fragments, HeapFragment};
pub use crate::mailbox::{receive, receive_ref, send, send_fragmented};
pub use crate::message::{Message, Queue, QueueCursor};
pub use crate::node::NodeEntry;
pub use crate::off_heap::{MsoHandle, OffHeapTable};
pub use crate::share::copy_struct_shared;

/// The conventional glob-import surface for downstream crates.
pub mod prelude {
    pub use crate::copy::{copy_struct, copy_struct_simple, size_of_copy, CopyContext};
    pub use crate::error::is_message_heap_exhaustion;
    pub use crate::fragment::{Fragments, HeapFragment};
    pub use crate::mailbox::{receive, receive_ref, send, send_fragmented};
    pub use crate::message::{Message, Queue, QueueCursor};
    pub use crate::node::NodeEntry;
    pub use crate::off_heap::{MsoHandle, OffHeapTable};
    pub use crate::share::copy_struct_shared;
}
