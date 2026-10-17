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

use crate::bounds::ScannedBounds;
use crate::copy::Copier;
use crate::error::heap_exhausted;
use crate::heap_info::HeapObservable;
use crate::nursery::Nursery;
use crate::policy::HeapPolicy;
use crate::reachable::ReachableObject;
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
    nursery: Nursery,
    mature: HeapSpace,
    minor_since_major: u32,
    stats: GcStats,
    /// How many times the mature space has been **replaced** (the fullsweep's relocation
    /// install). The mature generation's half of the [`ScannedBounds`] geometry epoch: a
    /// [`HeapSpace`] never moves or resizes, so a replacement is the only event that can change
    /// the mature extent — and each one installs a *different* space at a different address.
    mature_generation: u64,
}

impl Collector {
    /// Creates a collector with the given `policy`, using the default allocator.
    pub fn new(policy: HeapPolicy) -> Result<Collector> {
        Self::with_allocator(beam_alloc::registry::default_allocator().clone(), policy)
    }

    /// Creates a collector with the given `policy`, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, policy: HeapPolicy) -> Result<Collector> {
        let nursery = Nursery::new(alloc.clone(), policy.initial_young_words)?;
        let mature = HeapSpace::with_allocator(alloc, policy.initial_mature_words)?;
        Ok(Collector {
            policy,
            nursery,
            mature,
            minor_since_major: 0,
            stats: GcStats::default(),
            mature_generation: 0,
        })
    }

    /// The collector's heap-size policy.
    pub fn policy(&self) -> &HeapPolicy {
        &self.policy
    }

    /// The young (nursery) space the mutator allocates in.
    ///
    /// A [growth](Self::grow_nursery) retains the space it replaced, so this is the **current**
    /// space only: the retained ones are still part of the nursery for a collection and a
    /// range test ([`young_contains`](Self::young_contains),
    /// [`young_space_count`](Self::young_space_count)), but nothing new is allocated in them.
    pub fn young(&self) -> &HeapSpace {
        self.nursery.current()
    }

    /// The young (nursery) space, mutably — where the mutator allocates.
    pub fn young_mut(&mut self) -> &mut HeapSpace {
        self.nursery.current_mut()
    }

    /// The nursery chain, crate-visibly — the collection and range-test half of the nursery
    /// ([`crate::live`]'s tests size a fullsweep from it).
    pub(crate) fn nursery(&self) -> &Nursery {
        &self.nursery
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
    ///
    /// This reads the **current** (allocatable) space only — a [growth](Self::grow_nursery)
    /// retires the space it replaced and nothing is ever allocated in a retired one, so the
    /// current space is the correct one for an allocation-admission signal. A *collection*
    /// must instead cover the whole chain, which is why [`collect`](Self::collect) compares
    /// the mature's room against the chain-wide [`Nursery::used_words`].
    ///
    /// The chain's [collection debt](Self::collection_debt) is deliberately **not** folded in —
    /// see that method for the opt-in seam a host consults alongside this one.
    pub fn should_collect(&self, need_words: usize) -> bool {
        self.nursery.current().available_words() < need_words
    }

    /// Whether the nursery chain carries a space a [growth](Self::grow_nursery) retired — the
    /// chain's **collection debt**.
    ///
    /// A growth never collects and never moves a term, so the only thing that reclaims a
    /// retired space is a [collection](Self::collect) (which collapses the chain). A `true`
    /// answer therefore means **a collection is required to reclaim the chain: a
    /// heap-admission door should treat it like a full nursery** and collect, even when the
    /// current space still has room for the request.
    ///
    /// This is an **opt-in** seam, not part of [`should_collect`](Self::should_collect): a
    /// caller that ignores it pays nothing for it (one `Vec::is_empty`). `beam_rt`'s
    /// heap-admission doors (`src/context/gc.rs::ensure_heap` / `ensure_heap_full`) must OR it
    /// into their `should_collect` decision once they unconditionally admit the collection —
    /// see the handoff in the crate `CONTEXT.md`. Without that, a host suppresses the very
    /// collection that clears the chain and the spent capacity of every growth accumulates for
    /// as long as the nursery keeps being enlarged.
    pub fn collection_debt(&self) -> bool {
        self.nursery.retains_old_spaces()
    }
    /// Runs a collection, picking minor vs major by policy and escalating a
    /// would-not-fit minor to a major.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        let kind = if self.policy.fullsweep_due(self.minor_since_major)
            || self.mature.available_words() < self.nursery.used_words()
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
    ///
    /// Every space of a chained nursery is evacuated, and the chain is then
    /// [collapsed](crate::nursery::Nursery::collapse) back to one space.
    pub fn collect_minor(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let needed = self.nursery().used_words();
        if self.mature.available_words() < needed {
            return Err(heap_exhausted(needed));
        }
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from);
        let stats = Copier::new(&from, &mut self.mature).run(roots)?;
        self.minor_since_major += 1;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.minor_collections += 1;
        // The nursery is empty again: size it for the next round from what the collection
        // *left in it* — for a minor that is exactly the words it promoted
        // (`GcStats::words_copied`), i.e. the C's minor-time young-heap resize fed with the
        // surviving usage (`erl_gc.c:1493`). That is a quantity the round measured exactly,
        // and (unlike a fullsweep, which relocates everything) a minor really does leave a
        // live young set behind, so it is the minor's own sizing input.
        self.nursery.collapse(&self.policy, stats.words_copied)?;
        Ok(stats)
    }

    /// Runs a major (fullsweep) collection: evacuate young and mature into a fresh space.
    ///
    /// The fresh space is sized up front so the copy always fits and the two from-spaces are
    /// never left half-forwarded: the policy is asked for room for the **exact live set** the
    /// fullsweep evacuates plus the **young heap's occupancy** (a whole nursery's worth of
    /// promotion, the C `need_after`, `erl_gc.c:2072`) plus its reserve, so the next
    /// collections can be minors again. Should
    /// [`max_heap_words`](HeapPolicy::max_heap_words) clamp that size below the live set,
    /// the typed heap exhaustion is returned *before* a single word is copied rather than
    /// mid-copy.
    ///
    /// The nursery is re-sized by the C's **two-step** rule
    /// ([`HeapPolicy::young_after_fullsweep`]): its pre-copy occupancy first
    /// (`erl_gc.c:1866`), then the exact post-copy live set
    /// (`adjust_after_fullsweep`, `erl_gc.c:2062-2098`).
    ///
    /// Every space of a chained nursery is evacuated, and the chain is then
    /// [collapsed](crate::nursery::Nursery::collapse_after_fullsweep) back to one space.
    pub fn collect_major(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from);
        from.add(&self.mature);
        // Size the fresh space from the **exact live word count** of the graph this
        // fullsweep evacuates, plus the **young heap's occupancy** the C folds into the same
        // `need_after` — the C `adjust_after_fullsweep`'s
        // `need_after = (HEAP_TOP(p) - HEAP_START(p)) + need + …` (`erl_gc.c:2072`).
        //
        // The live term is deliberately *not* the pre-collection occupancy: a
        // fullsweep-sized generational heap is *full* when the cadence fires (the live set
        // plus every promotion the minors since the last fullsweep made, all of it garbage
        // by now), so feeding the policy the **mature** occupancy makes its `should_grow`
        // fire on every fullsweep and the mature capacity creeps up one growth-table step
        // per fullsweep, without bound, for a live set that never changes (see
        // [`crate::live`]).
        //
        // The young occupancy is a different quantity and *is* included: the fresh mature
        // space has to be able to receive a whole nursery's worth of promotion after the
        // fullsweep, otherwise the very next collection cannot be a minor and every
        // collection escalates to a fullsweep — the generational cadence never runs. It is
        // bounded by the nursery capacity, which [`HeapPolicy::young_after_collection`]
        // keeps bounded, so including it still converges.
        let live = crate::live::live(&from, roots).words;
        let young_occupancy = self.nursery.used_words();
        // Grow, shrink or keep the current mature capacity
        // (`HeapPolicy::mature_after_fullsweep`, the C `adjust_after_fullsweep`).
        let size =
            self.policy
                .mature_after_fullsweep(self.mature.capacity_words(), live, young_occupancy);
        let alloc = self.nursery().current().allocator().clone();
        // Sized up front so the copy always fits — a collection never leaves the
        // from-space half-forwarded. With `live` exact, this is an exact fit test rather
        // than an upper bound.
        let mut fresh = HeapSpace::with_allocator(alloc, size)?;
        // `max_heap_words` can clamp `size` below the live set: report the typed heap
        // exhaustion before copying anything, so a doomed fullsweep leaves both from-spaces
        // untouched (a mid-copy failure would half-forward them).
        if fresh.available_words() < live {
            return Err(heap_exhausted(live));
        }
        let stats = Copier::new(&from, &mut fresh).run(roots)?;
        self.mature = fresh; // drops the old mature space
        self.mature_generation = self.mature_generation.saturating_add(1);
        self.minor_since_major = 0;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.major_collections += 1;
        // The fullsweep relocated *every* live word — the nursery's included — into the fresh
        // mature space, so the nursery's surviving occupancy is now zero and there is no live
        // young set to size it from. It is therefore sized by the C's **two-step** rule
        // ([`HeapPolicy::young_after_fullsweep`]): the **demand the round observed**
        // (`young_occupancy`, sampled before the copy — the `major_collection` pre-copy sizing,
        // `erl_gc.c:1866`) followed by the **post-copy live set** (`live`, this fullsweep's
        // exact reachable word count — the `adjust_after_fullsweep` shrink,
        // `erl_gc.c:2062-2098`). The second step is what stops a nursery the growth seam
        // stepped up for a burst from staying that size forever once the burst is dead: step 1
        // compares the capacity against the *occupancy*, which a fullsweep always finds full,
        // so only the live set can reveal the over-provisioning.
        self.nursery
            .collapse_after_fullsweep(&self.policy, young_occupancy, live)?;
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
        self.nursery.grow(&self.policy, need_words)?;
        Ok(collection)
    }

    /// Enlarges the young nursery so a `need_words`-word allocation can proceed, **without
    /// collecting and without moving any live term** — the no-relocation growth seam.
    ///
    /// It is safe for a caller that holds no root set (so cannot collect): the space it
    /// replaces is **retained** and every live word stays addressable at its original address,
    /// so a chained nursery is just as collectable later as a single space
    /// ([`young_contains`](Self::young_contains) covers the retained spaces and the next
    /// collection evacuates them all). When the current space already has room this is a
    /// no-op; when a [`max_heap_words`](HeapPolicy::max_heap_words) (or growth-table) ceiling
    /// cannot reach the request, the typed heap-exhaustion
    /// [`BeamError`](beam_common::BeamError) is returned and the collector is left untouched.
    pub fn grow_nursery(&mut self, need_words: usize) -> Result<()> {
        self.nursery.grow(&self.policy, need_words)
    }

    /// Whether `ptr` addresses a word of **any** nursery space — the current one or a retired
    /// one a [growth](Self::grow_nursery) retained. The nursery-range test a host's
    /// unscanned-root valve needs.
    pub fn young_contains(&self, ptr: *const Eterm) -> bool {
        self.nursery.contains(ptr)
    }

    /// How many spaces the nursery chain holds: `1` until a [growth](Self::grow_nursery)
    /// retains a space, back to `1` once a collection collapses the chain.
    pub fn young_space_count(&self) -> usize {
        self.nursery.space_count()
    }

    /// The `O(1)` aggregate extent over **every** region this collector scans — the whole
    /// nursery chain (the current space and every space a [growth](Self::grow_nursery) retired)
    /// plus the mature generation — tagged with the geometry epoch it describes.
    ///
    /// This is the **fast-reject** filter a host's unscanned-root valve evaluates per reference
    /// word: [`may_contain`](ScannedBounds::may_contain)` == false` proves the address is in no
    /// scanned region (so the exact [`young_contains`](Self::young_contains) and
    /// [`mature_contains`](crate::AnyCollector::mature_contains) tests are answered without touching
    /// the chain's ordered index or the mature space), while `== true` is **not** a containment
    /// claim and must fall through to them. See [`ScannedBounds`] for why only the reject
    /// direction is sound.
    ///
    /// `O(1)`, allocation-free, callable through `&self`: the nursery chain's half is cached
    /// inside the chain (`src/nursery.rs`) and updated only when the chain's set of spaces
    /// changes; the mature half is two reads of a space that never moves or resizes. Both use
    /// each space's **reserved** extent (`[base, limit)`, not the moving live fill) so the
    /// interval cannot go stale between allocations; the [generation](ScannedBounds::generation)
    /// advances on every event that can change either half — a nursery space installed, retired
    /// or collapsed, or the mature space replaced by a fullsweep.
    pub fn scanned_bounds(&self) -> ScannedBounds {
        let young = self.nursery.bounds();
        let mature = ScannedBounds::at(
            self.mature.base() as usize,
            self.mature.limit() as usize,
            self.mature_generation,
        );
        // Both halves carry a monotone event counter (the chain's geometry epochs, the mature
        // space's replacements), so their sum changes whenever either does: one number that is
        // exact for "the geometry moved".
        young
            .covering(mature)
            .with_generation(young.generation().saturating_add(mature.generation()))
    }
    /// Visits every **boxed object** reachable from `roots` across the **whole heap** — every
    /// space of the nursery chain (the current one and any a
    /// [`grow_nursery`](Self::grow_nursery) retired) plus the mature generation — passing the
    /// object's header word and a pointer to that header, each object **exactly once**.
    ///
    /// This is the **reachability half** of an off-heap liveness scan: a caller that owns an
    /// off-heap object list (e.g. `beam_process`'s owned refc binaries) can enumerate the heap
    /// to find which on-heap handles still name a payload, instead of retaining every entry it
    /// cannot decide. The walk itself is **layout-agnostic**: it reports *which* objects are
    /// reachable, never what they contain, so the caller applies **its own** predicate to the
    /// reported `(header, ptr)` pairs — e.g. a subtag test that picks out the
    /// `ErlSubBits`/`BinRef` handles and reads their off-heap payload pointer word. No
    /// container interior is assumed here.
    ///
    /// # Contract
    ///
    /// * **Read-only.** No term is written and no forwarding marker is laid down, so it may run
    ///   at any safepoint, immediately before a collection.
    /// * **Whole-heap.** A growth-retained nursery space and the mature generation are both
    ///   covered ([`young_contains`](Self::young_contains)'s rule); a reference into the
    ///   literal area, another heap or off-heap memory is *not* visited and not descended into
    ///   (the copier's [`Target::Foreign`](crate::copy::Target::Foreign) rule).
    /// * **Iterative and cycle-safe.** A 50 000-object chain or a cycle neither recurses nor
    ///   hangs, and a shared object is reported once.
    /// * A **cons cell** is traversed (so the boxed objects behind it are reached) but not
    ///   reported: it carries no header word.
    ///
    /// Returns the crate's typed [`Result`]; the walk can only fail through the shared
    /// reference-enumeration primitive, whose signature is fallible for its copier callers —
    /// this method's own callback is infallible.
    pub fn for_each_reachable_object(
        &self,
        roots: &[Eterm],
        visit: impl FnMut(Eterm, *const Eterm),
    ) -> Result<()> {
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from); // every space of the chain
        from.add(&self.mature); // the live region of the copying mature space
        crate::reachable::for_each_reachable(&from, roots, visit)
    }

    /// The **safe** form of [`for_each_reachable_object`](Self::for_each_reachable_object):
    /// visits every **boxed object** reachable from `roots` across the **whole heap** (every
    /// space of the nursery chain plus the mature generation), each object **exactly once**,
    /// passing a read-only [`ReachableObject`] view of it.
    ///
    /// It builds the **same from-set** as the raw sibling and drives the same walk, so the two
    /// report the same objects in the same order; the only difference is that the visitor reads
    /// the object's words through [`ReachableObject::word`] / [`ReachableObject::words`] instead
    /// of a raw pointer, so an inspection-only caller needs **no `unsafe`** — e.g. the off-heap
    /// `BinRef` probe reads the payload pointer word with `obj.word(1)`. See
    /// [`for_each_reachable_object`](Self::for_each_reachable_object) for the full contract
    /// (read-only, whole-heap, layout-agnostic, iterative and cycle-safe, each object once, cons
    /// cells traversed but not reported).
    pub fn for_each_reachable_object_view(
        &self,
        roots: &[Eterm],
        visit: impl FnMut(ReachableObject<'_>),
    ) -> Result<()> {
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from); // every space of the chain
        from.add(&self.mature); // the live region of the copying mature space
        crate::reachable::for_each_reachable_view(&from, roots, visit)
    }
}

impl HeapObservable for Collector {
    /// Minors since the last fullsweep.
    fn minor_gcs(&self) -> u32 {
        self.minor_since_major
    }

    /// The nursery (the "heap" in the C sense) capacity, in words — the **summed** capacity of
    /// every nursery space, so a chained nursery reports its full size.
    fn heap_size_words(&self) -> usize {
        self.nursery.capacity_words()
    }

    /// The nursery plus the mature generation (the C's `heap_sz + old heap`), in words.
    fn total_heap_size_words(&self) -> usize {
        self.nursery.capacity_words() + self.mature.capacity_words()
    }

    fn heap_policy(&self) -> &HeapPolicy {
        &self.policy
    }
}
