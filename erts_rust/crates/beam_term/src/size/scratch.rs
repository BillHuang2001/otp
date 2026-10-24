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

//! [`SizingScratch`] — the caller-owned, reusable working memory of the sizing walk.
//!
//! Every [`crate::size`] entry point needs the same three buffers: the explicit DFS
//! stack, the set of compound objects on the **current** DFS path (the cycle guard), and
//! — for the `OverLimit` arm of the bounded form only — a `done` set for the acyclicity
//! pass. A caller that sizes graph after graph (a copier pricing every message it moves)
//! would otherwise pay a fresh `malloc` for each of them *per call*, which is pure
//! overhead: the buffers are scratch, not results.
//!
//! [`SizingScratch`] is that working memory as one owned value. It is **opaque** (no
//! public field, no accessor that exposes a buffer), so the layout of a walk stays an
//! implementation detail; the only things a caller can do with one are construct it,
//! hand `&mut` to [`size_object_with_scratch`](crate::size::size_object_with_scratch) /
//! [`size_object_bounded_with_scratch`](crate::size::size_object_bounded_with_scratch),
//! and shrink it.
//!
//! # Allocation behaviour
//!
//! The walks **clear** the buffers (which keeps their allocation) rather than dropping
//! them, so after one warm-up call whose graph was at least as big as the current one, a
//! sizing call performs **no allocation at all** — no `Vec` reservation, no hash-table
//! reservation, no rehash. A bigger graph grows the buffers once and the new capacity is
//! retained; the growth is the amortized part of the same story.
//!
//! # Reuse and state
//!
//! A scratch carries **no** state between calls: every walk clears the stack and both
//! sets before it starts, so a scratch used on a cyclic graph and then on an acyclic one
//! answers each one correctly (and a scratch reused on the *same* graph answers the same
//! number). That the buffers are retained is invisible to the results — the walk is a
//! pure function of `(term, limit, policy)`.
//!
//! # Memory
//!
//! Retaining capacity is the point, but it means an outlier graph (one enormous payload)
//! leaves the high-water-mark buffers behind: the stack costs `size_of::<Step>()` per
//! pending step and the path set roughly one machine word per object on the deepest
//! path. [`SizingScratch::shrink_to_fit`] returns that memory to the allocator for a
//! caller that would rather pay the re-allocation later.

use std::collections::HashSet;

use super::address_hash::AddressHashBuilder;
use super::Step;

/// Reusable working memory for one sizing walk — see the [module docs](self).
///
/// Construct with [`new`](Self::new) (or [`default`](Default::default)) and keep one
/// alongside whatever you size; [`with_capacity`](Self::with_capacity) pre-sizes it for
/// the payloads you expect. Pass it to
/// [`size_object_with_scratch`](crate::size::size_object_with_scratch) /
/// [`size_object_bounded_with_scratch`](crate::size::size_object_bounded_with_scratch).
///
/// `Default` and `new` are the same thing: empty buffers, no allocation at all until the
/// first walk needs one.
pub struct SizingScratch {
    /// The explicit DFS stack ([`crate::size`]'s `Step`).
    pub(super) stack: Vec<Step>,
    /// The compound objects on the current DFS path — the cycle guard.
    pub(super) on_path: HashSet<usize, AddressHashBuilder>,
    /// Objects fully explored by the acyclicity re-pass.
    pub(super) done: HashSet<usize, AddressHashBuilder>,
}

impl SizingScratch {
    /// An empty scratch: no allocation until a walk needs a buffer.
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            on_path: HashSet::with_hasher(AddressHashBuilder),
            done: HashSet::with_hasher(AddressHashBuilder),
        }
    }

    /// An empty scratch pre-sized for a graph with about `compound_objects` compound
    /// objects (and no deeper path than that), so even the first call does not have to
    /// grow its buffers.
    ///
    /// A wrong guess costs nothing but a growth event; it can never change a result.
    pub fn with_capacity(compound_objects: usize) -> Self {
        Self {
            stack: Vec::with_capacity(compound_objects),
            on_path: HashSet::with_capacity_and_hasher(compound_objects, AddressHashBuilder),
            done: HashSet::with_capacity_and_hasher(compound_objects, AddressHashBuilder),
        }
    }

    /// Releases the retained buffers back to the allocator (the next walk re-reserves
    /// them). Use it after an outlier payload, when the high-water mark is far above what
    /// the usual graphs need.
    pub fn shrink_to_fit(&mut self) {
        self.stack.shrink_to_fit();
        self.on_path.shrink_to_fit();
        self.done.shrink_to_fit();
    }
}

impl Default for SizingScratch {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for SizingScratch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The buffers are an implementation detail; report what a caller can reason about.
        f.debug_struct("SizingScratch")
            .field("stack_capacity", &self.stack.capacity())
            .field("path_capacity", &self.on_path.capacity())
            .field("done_capacity", &self.done.capacity())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_and_default_are_the_same_empty_scratch() {
        let mut a = SizingScratch::new();
        let b = SizingScratch::default();
        assert_eq!(a.stack.capacity(), 0);
        assert_eq!(b.stack.capacity(), 0);
        assert_eq!(a.on_path.capacity(), 0);
        assert_eq!(a.done.capacity(), 0);
        a.shrink_to_fit();
        assert_eq!(a.stack.capacity(), 0);
    }

    #[test]
    fn with_capacity_reserves_but_stays_empty() {
        let s = SizingScratch::with_capacity(64);
        assert!(s.stack.capacity() >= 64);
        assert!(s.on_path.capacity() >= 64);
        assert!(s.stack.is_empty());
        assert!(s.on_path.is_empty());
        assert!(s.done.is_empty());
    }

    #[test]
    fn the_scratch_is_send_and_sync() {
        // A host may own one per worker thread; nothing in the walk needs interior
        // mutability, so the scratch must not smuggle in a `!Send` buffer.
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SizingScratch>();
    }
}
