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

//! The two-generation collector: [`Collector`], [`Collection`] and [`CollectionKind`].
//!
//! A **minor** collection Cheney-copies every live young term straight into the
//! `mature` space and resets the nursery (immediate promotion); a **major**
//! (fullsweep) collection evacuates `young` and `mature` into one freshly-sized space.
//! Because terms are immutable, a mature object can never refer to a later-allocated
//! young object, so mature objects are never rescanned — the generational payoff.
//! The driving logic here is safe Rust; the word-level copier lives in
//! [`crate::copy`].

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::copy::Copier;
use crate::error::heap_exhausted;
use crate::heap_info::HeapObservable;
use crate::policy::HeapPolicy;
use crate::space::{FromSet, HeapSpace};
use crate::stats::GcStats;
/// Which collection a [`Collector`] ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    /// A young-only collection: the nursery is evacuated into the mature space.
    Minor,
    /// A fullsweep collection: young and mature are evacuated into a fresh space.
    Major,
}

/// The result of one [`Collector::collect`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collection {
    /// Which kind of collection ran.
    pub kind: CollectionKind,
    /// The statistics of that collection.
    pub stats: GcStats,
}

/// The two-generation collector owning the young and mature spaces.
pub struct Collector {
    policy: HeapPolicy,
    young: HeapSpace,
    mature: HeapSpace,
    minor_since_major: u32,
    stats: GcStats,
}

impl Collector {
    /// Creates a collector with the given `policy`, using the default allocator.
    pub fn new(policy: HeapPolicy) -> Result<Collector> {
        Self::with_allocator(beam_alloc::registry::default_allocator().clone(), policy)
    }

    /// Creates a collector with the given `policy`, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, policy: HeapPolicy) -> Result<Collector> {
        let young = HeapSpace::with_allocator(alloc.clone(), policy.initial_young_words)?;
        let mature = HeapSpace::with_allocator(alloc, policy.initial_mature_words)?;
        Ok(Collector {
            policy,
            young,
            mature,
            minor_since_major: 0,
            stats: GcStats::default(),
        })
    }

    /// The collector's heap-size policy.
    pub fn policy(&self) -> &HeapPolicy {
        &self.policy
    }

    /// The young (nursery) space.
    pub fn young(&self) -> &HeapSpace {
        &self.young
    }

    /// The young (nursery) space, mutably — where the mutator allocates.
    pub fn young_mut(&mut self) -> &mut HeapSpace {
        &mut self.young
    }

    /// The mature space.
    pub fn mature(&self) -> &HeapSpace {
        &self.mature
    }

    /// The collector's accumulated statistics.
    pub fn stats(&self) -> GcStats {
        self.stats
    }

    /// The number of minor collections run since the last fullsweep — the C `GEN_GCS`, and
    /// the value the `garbage_collection` item reports as `minor_gcs`.
    pub fn minor_gcs(&self) -> u32 {
        self.minor_since_major
    }

    /// Whether the nursery can no longer satisfy `need_words` more words.
    pub fn should_collect(&self, need_words: usize) -> bool {
        self.young.available_words() < need_words
    }

    /// Runs a collection, picking minor vs major by policy and escalating a
    /// would-not-fit minor to a major.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        let kind = if self.policy.fullsweep_due(self.minor_since_major)
            || self.mature.available_words() < self.young.used_words()
        {
            CollectionKind::Major
        } else {
            CollectionKind::Minor
        };
        let stats = match kind {
            CollectionKind::Minor => self.collect_minor(roots)?,
            CollectionKind::Major => self.collect_major(roots)?,
        };
        Ok(Collection { kind, stats })
    }

    /// Runs a minor collection: evacuate the nursery's live terms into the mature space.
    ///
    /// The destination must fit the whole nursery up front (immediate promotion, no
    /// partial-copy rollback). [`collect`](Self::collect) escalates to a major when it would
    /// not; a **direct** call instead reports the typed heap exhaustion *before* a single
    /// word is copied, so a too-small mature space can never leave the nursery
    /// half-forwarded.
    pub fn collect_minor(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let needed = self.young.used_words();
        if self.mature.available_words() < needed {
            return Err(heap_exhausted(needed));
        }
        let mut from = FromSet::new();
        from.add(&self.young);
        let stats = Copier::new(&from, &mut self.mature).run(roots)?;
        self.young.reset();
        self.minor_since_major += 1;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.minor_collections += 1;
        // The nursery is empty again: size it for the next round from what the collection
        // *left in it* — for a minor that is exactly the words it promoted
        // (`GcStats::words_copied`), i.e. the C's minor-time young-heap resize fed with the
        // surviving usage (`erl_gc.c:1493`). Never the pre-collection fill: a nursery is only
        // collected when it is full, so that stale count is ≈ the capacity and would grow the
        // nursery on every collection, without bound.
        self.resize_nursery(stats.words_copied)?;
        Ok(stats)
    }

    /// Runs a major (fullsweep) collection: evacuate young and mature into a fresh space.
    ///
    /// The fresh space is sized up front from the heap policy, so the copy always fits and
    /// the two from-spaces are never left half-forwarded. Should
    /// [`max_heap_words`](HeapPolicy::max_heap_words) clamp that size below the live set,
    /// the typed heap exhaustion is returned *before* a single word is copied rather than
    /// mid-copy.
    pub fn collect_major(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let mut from = FromSet::new();
        from.add(&self.young);
        from.add(&self.mature);
        let used = self.young.used_words();
        let live = used + self.mature.used_words();
        // Size the fresh space from the heap policy: grow, shrink or keep the current
        // mature capacity (`HeapPolicy::mature_after_fullsweep`, the C
        // `adjust_after_fullsweep`).
        let size = self
            .policy
            .mature_after_fullsweep(self.mature.capacity_words(), live, 0);
        let alloc = self.young.allocator().clone();
        // Sized up front so the copy always fits — a collection never leaves the
        // from-space half-forwarded.
        let mut fresh = HeapSpace::with_allocator(alloc, size)?;
        // `max_heap_words` can clamp `size` below the live set: report the typed heap
        // exhaustion before copying anything, so a doomed fullsweep leaves both from-spaces
        // untouched (a mid-copy failure would half-forward them).
        if fresh.available_words() < live {
            return Err(heap_exhausted(live));
        }
        let stats = Copier::new(&from, &mut fresh).run(roots)?;
        self.mature = fresh; // drops the old mature space
        self.young.reset();
        self.minor_since_major = 0;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.major_collections += 1;
        // The fullsweep relocated *every* live word — the nursery's included — into the fresh
        // mature space, so the nursery's surviving occupancy is now zero: the resize can only
        // bring it down towards its reserved minimum, never up (sizing it from the live set
        // would make the nursery track the mature generation's contents).
        self.resize_nursery(0)?;
        Ok(stats)
    }

    /// Grows the nursery **on demand** so that a `need_words`-word allocation can proceed —
    /// the seam a runtime calls when [`should_collect`](Self::should_collect) reports the
    /// nursery is full.
    ///
    /// It collects once (a minor, escalating to a major when a minor cannot make room), then
    /// grows the now-empty nursery to the smallest curated size that covers `need_words`.
    /// The growth is *explicit* because a collection sizes the nursery from what it left alive
    /// in it, not from what the caller is about to allocate — without this step a request
    /// larger than the nursery could never be met by collecting alone. The
    /// returned [`Collection`] is the one it ran.
    ///
    /// Afterwards `young().available_words() >= need_words`. If
    /// [`max_heap_words`](HeapPolicy::max_heap_words) (or the growth table's ceiling) makes
    /// the request unreachable, the typed heap-exhaustion
    /// [`BeamError`](beam_common::BeamError) is returned instead — the collector is left
    /// consistent (never a half-forwarded heap) and, for an unreachable request, untouched.
    /// The work is bounded, so this never loops forever.
    pub fn ensure_capacity(
        &mut self,
        need_words: usize,
        roots: &mut [Eterm],
    ) -> Result<Collection> {
        // Fail fast when the policy's ceiling makes the request unreachable: there is no
        // point collecting, and the collector is left exactly as it was.
        if self.policy.next_words(need_words, 0) < need_words {
            return Err(heap_exhausted(need_words));
        }
        let collection = self.collect(roots)?;
        grow_nursery(&mut self.young, &self.policy, need_words)?;
        Ok(collection)
    }

    /// Sizes the (now empty) nursery for the next round from the heap-size policy
    /// ([`HeapPolicy::young_after_collection`]), where `live_words` is the number of words the
    /// collection **left in the nursery** — the promoted words for a minor, `0` after a
    /// fullsweep. A nursery the round left mostly empty shrinks, one that is already about
    /// right keeps its size, and the capacity never grows merely because a collection ran (see
    /// that method's contract); the space is only replaced when the target differs, so a
    /// steady-state workload pays nothing.
    fn resize_nursery(&mut self, live_words: usize) -> Result<()> {
        let capacity = self.young.capacity_words();
        let target = self.policy.young_after_collection(capacity, live_words);
        if target != capacity {
            let alloc = self.young.allocator().clone();
            self.young = HeapSpace::with_allocator(alloc, target)?;
        }
        Ok(())
    }
}

/// Grows an **empty** nursery to the smallest size the policy allows that covers
/// `need_words`, or returns the typed heap exhaustion when no such size exists.
///
/// Shared by both mature-generation strategies (the copying [`Collector`] and
/// [`crate::GenImmixCollector`]) so the growth arithmetic lives in exactly one place. The
/// caller has just collected, so `nursery` is empty: this only ever *replaces* it with a
/// larger space, never moving a live term. Each step targets
/// `policy.next_words(need_words, 0)` (the smallest curated size that covers the request,
/// floored by [`min_heap_words`](HeapPolicy::min_heap_words) and clamped by
/// [`max_heap_words`](HeapPolicy::max_heap_words)); the loop is bounded because it stops as
/// soon as the target no longer exceeds the current capacity.
pub(crate) fn grow_nursery(
    nursery: &mut HeapSpace,
    policy: &HeapPolicy,
    need_words: usize,
) -> Result<()> {
    while nursery.available_words() < need_words {
        let capacity = nursery.capacity_words();
        let target = policy.next_words(need_words, 0);
        if target <= capacity {
            // The policy's ceiling (or the growth table's) leaves no room to grow, so
            // `need_words` is unreachable under this policy.
            return Err(heap_exhausted(need_words));
        }
        let alloc = nursery.allocator().clone();
        *nursery = HeapSpace::with_allocator(alloc, target)?;
    }
    Ok(())
}

impl HeapObservable for Collector {
    /// Minors since the last fullsweep.
    fn minor_gcs(&self) -> u32 {
        self.minor_since_major
    }

    /// The nursery (the "heap" in the C sense) capacity, in words.
    fn heap_size_words(&self) -> usize {
        self.young.capacity_words()
    }

    /// The nursery plus the mature generation (the C's `heap_sz + old heap`), in words.
    fn total_heap_size_words(&self) -> usize {
        self.young.capacity_words() + self.mature.capacity_words()
    }

    fn heap_policy(&self) -> &HeapPolicy {
        &self.policy
    }
}