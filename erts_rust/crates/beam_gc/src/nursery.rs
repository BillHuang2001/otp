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

//! The young nursery as a **chain of copying spaces**: the growth-only, no-relocation
//! enlargement seam.
//!
//! A [`HeapSpace`] wraps `beam_alloc::Heap`, a bump allocator that never reallocates —
//! growth is by *replacement*, not by moving live terms (workspace `CONTEXT.md` §2). A
//! runtime that holds **no root set** (so cannot collect) must still be able to enlarge the
//! nursery so a large allocation can proceed. Replacing the nursery would strand every live
//! word, so [`Nursery::grow`] instead **retains the full old space** in `retired` and points
//! the allocator at a fresh, larger one: live words stay addressable at their original
//! address and nothing is moved.
//!
//! A retained space is only *kept* when it is **provably live-or-dormant**, never when it is
//! provably dead: the one-way rule this module enforces is that a superseded space may be
//! dropped only when no live term can name one of its words — either it never handed a single
//! word to the mutator (`used_words() == 0`, e.g. the space a [collapse](Nursery::collapse)
//! just emptied), or a collection has evacuated it ([`collapse`](Nursery::collapse)).
//! [`retains_old_spaces`](Nursery::retains_old_spaces) reports the retained chain — the
//! "collection debt" a growth accumulates and only a collection can clear.
//!
//! Once a collection has evacuated **every** space the chain is no longer needed, so
//! [`Nursery::collapse`] drops the retired spaces and re-sizes the single remaining current
//! space — the chain is exactly one space before any mutator allocates again. While a chain
//! is live, every space is part of the nursery: [`spaces`](Nursery::spaces) enumerates them
//! all, [`contains`](Nursery::contains) tests every one, and
//! [`add_to_from_set`](Nursery::add_to_from_set) adds every live region to a collection's
//! from-set, so a collection evacuates a chained nursery whole.
//!
//! This module is safe Rust — it only manipulates [`HeapSpace`] values; `unsafe` stays
//! confined to the word-level modules of the crate.

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::error::heap_exhausted;
use crate::policy::HeapPolicy;
use crate::space::{FromSet, HeapSpace};

/// The young nursery: the space the mutator allocates in (`current`) plus every space a
/// growth has **retired** (the larger space a growth installed replaces it, never moves it).
pub(crate) struct Nursery {
    /// The space the mutator allocates in.
    current: HeapSpace,
    /// The spaces a growth retired, oldest first. Their live words are still addressable and
    /// are still part of the nursery (a collection evacuates them; a range test covers them).
    retired: Vec<HeapSpace>,
}

impl Nursery {
    /// A nursery with a single empty `capacity_words`-word current space and no retired
    /// spaces, backed by `alloc`.
    pub(crate) fn new(alloc: AllocRef, capacity_words: usize) -> Result<Nursery> {
        Ok(Nursery {
            current: HeapSpace::with_allocator(alloc, capacity_words)?,
            retired: Vec::new(),
        })
    }

    /// The space the mutator allocates in.
    pub(crate) fn current(&self) -> &HeapSpace {
        &self.current
    }

    /// The space the mutator allocates in, mutably.
    pub(crate) fn current_mut(&mut self) -> &mut HeapSpace {
        &mut self.current
    }

    /// Every nursery space, **retired first then current** — the chain a collection evacuates
    /// and a range test has to cover.
    pub(crate) fn spaces(&self) -> impl Iterator<Item = &HeapSpace> {
        self.retired.iter().chain(core::iter::once(&self.current))
    }

    /// How many spaces the chain holds: `1` until a growth retires one, back to `1` after a
    /// collection [collapses](Nursery::collapse) the chain.
    pub(crate) fn space_count(&self) -> usize {
        self.retired.len() + 1
    }

    /// Whether a growth has **retired** a space — the chain's *collection debt*: words that a
    /// growth can never reclaim (it moves nothing) and that only a
    /// [collection](Nursery::collapse) can free.
    ///
    /// This is the second half of the collectors' admission signal
    /// ([`Collector::should_collect`](crate::Collector::should_collect)): a host asks that
    /// question at its heap-admission point to decide whether to collect, so reporting the debt
    /// is what stops the host from suppressing the very collection that clears it.
    pub(crate) fn retains_old_spaces(&self) -> bool {
        !self.retired.is_empty()
    }
    /// The words handed out across **every** nursery space — the exact word count a collection
    /// of a chained nursery evacuates.
    pub(crate) fn used_words(&self) -> usize {
        self.spaces().map(HeapSpace::used_words).sum()
    }

    /// The capacity in words **summed** over every nursery space (a chained nursery is as
    /// large as all its spaces together).
    pub(crate) fn capacity_words(&self) -> usize {
        self.spaces().map(HeapSpace::capacity_words).sum()
    }

    /// Whether `ptr` addresses a word of **any** nursery space — current or a retired space a
    /// growth retained.
    pub(crate) fn contains(&self, ptr: *const Eterm) -> bool {
        self.spaces().any(|space| space.contains(ptr))
    }

    /// Adds the live region of **every** nursery space to `from`, so a collection evacuates the
    /// whole chain.
    pub(crate) fn add_to_from_set(&self, from: &mut FromSet) {
        for space in self.spaces() {
            from.add(space);
        }
    }

    /// Enlarges the nursery so a `need_words`-word allocation can proceed — the
    /// **NO-RELOCATION** growth seam.
    ///
    /// It **never collects and never moves a single live term**: the old space is retained in
    /// `retired` (so every live word stays addressable at its original address) and the
    /// allocator is pointed at a fresh, larger space. A caller that holds no root set may
    /// therefore use it safely.
    ///
    /// A superseded space is retained **only while it could still hold a live word**; one that
    /// never handed a single word to the mutator is dropped instead. `used_words() == 0` is
    /// exactly that proof — a [`HeapSpace`] hands words out only through its own bump pointer
    /// and is emptied only by [`HeapSpace::reset`], which a
    /// [collection](Nursery::collapse) runs *after* it has evacuated every word — so no term
    /// can name into it and dropping it frees the backing allocation at once. This is what
    /// keeps a growth that follows a collection (the collected space is empty) from retaining
    /// anything, and so breaks the collect/grow oscillation.
    ///
    /// The fresh space is the smallest size the policy allows that covers the request
    /// (`policy.next_words(need_words, 0)`, never smaller than the space it replaces); when the
    /// current space already has room the call is a no-op. The growth is **bounded to one
    /// step**, and a [`max_heap_words`](HeapPolicy::max_heap_words) (or growth-table) ceiling
    /// that cannot reach the request yields the typed
    /// [`BeamError::Alloc`](beam_common::BeamError::Alloc) and leaves the nursery untouched —
    /// the current space is not replaced and nothing is retired.
    pub(crate) fn grow(&mut self, policy: &HeapPolicy, need_words: usize) -> Result<()> {
        if self.current.available_words() >= need_words {
            return Ok(());
        }
        let target = policy
            .next_words(need_words, 0)
            .max(self.current.capacity_words());
        if target < need_words {
            return Err(heap_exhausted(need_words));
        }
        let fresh = HeapSpace::with_allocator(self.current.allocator().clone(), target)?;
        let old = core::mem::replace(&mut self.current, fresh);
        if old.used_words() != 0 {
            self.retired.push(old);
        }
        Ok(())
    }
    /// Collapses the chain back to **exactly one** space, after a collection has evacuated
    /// every space.
    ///
    /// It drops the retired spaces and re-sizes the single remaining current space from the
    /// heap-size policy ([`HeapPolicy::young_after_collection`]) over the capacity the chain
    /// held when it was collected, where `live_words` is what the collection left in the
    /// nursery (the promoted words for a minor, `0` after a fullsweep). The space is only
    /// replaced when the target differs; otherwise its bump pointer is merely rewound.
    pub(crate) fn collapse(&mut self, policy: &HeapPolicy, live_words: usize) -> Result<()> {
        let collected_capacity = self.capacity_words();
        let target = policy.young_after_collection(collected_capacity, live_words);
        self.retired.clear();
        if self.current.capacity_words() == target {
            self.current.reset();
        } else {
            let alloc = self.current.allocator().clone();
            self.current = HeapSpace::with_allocator(alloc, target)?;
        }
        Ok(())
    }
}
