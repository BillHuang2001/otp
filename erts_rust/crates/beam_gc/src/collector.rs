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
use crate::heap_info::{old_heap_size, HeapObservable};
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
    ///
    /// The young space is created at [`HeapPolicy::initial_heap_words`] — the C's initial-heap
    /// rule, `max(heap_need, min_heap_size)` — so an emulator `+hms` or a `spawn_opt`
    /// `{min_heap_size, N}` sizes a new process's first heap, and
    /// `erlang:process_info(P, heap_size)` reports it.
    pub fn with_allocator(alloc: AllocRef, policy: HeapPolicy) -> Result<Collector> {
        let nursery = Nursery::new(alloc.clone(), policy.initial_heap_words())?;
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
    /// must instead cover the whole chain and size itself from the live set it keeps, which is
    /// why [`collect`](Self::collect) decides minor vs major on the mature space's room against
    /// the **live** young word count ([`minor_promotion_fits`](Self::minor_promotion_fits)).
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
    /// The words a **minor** of the current nursery will copy into the mature space: the exact
    /// live graph of the nursery chain, counted read-only (no forwarding marker is written, so
    /// the count may run immediately before the copy — [`crate::live`]).
    ///
    /// This is the quantity the C's minor decision stands for, and the one
    /// [`GcStats::words_copied`] an evacuation of the same from-set and roots reports exactly;
    /// it is therefore both the escalation test's input ([`collect`](Self::collect)) and the
    /// minor's own room requirement ([`collect_minor`](Self::collect_minor)).
    fn young_promotion_words(&self, roots: &[Eterm]) -> usize {
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from);
        crate::live::live(&from, roots).words
    }

    /// Whether the mature space can receive the nursery's promotion — the C's
    /// `mature_size <= OLD_HEND(p) - OLD_HTOP(p)` test (`erts/emulator/beam/erl_gc.c:1486-1488`;
    /// when it fails, `minor_collection` returns `-1` and `garbage_collect` runs a major,
    /// `:1573-1576`, `:839-849`).
    ///
    /// The input is the **live** young word count, never the nursery's fill. The fill
    /// ([`Nursery::used_words`], chain-summed) also counts every word the mutator has already
    /// abandoned since the last collection, and in this runtime that is an allocation-history
    /// figure rather than a live-set one: a host's registered-BIF copy-out bump-allocates into
    /// the process heap and never rewinds, so a nursery can carry thousands of dead words. The
    /// C never compares that figure against the old heap's room because its young usage is
    /// `HEAP_TOP(p) - HEAP_START(p)`, bounded by `HEAP_SIZE(p)` by construction (`erl_gc.c:696`,
    /// `:1439`), and `HEAP_SIZE` is re-derived from the live set at every collection
    /// (`erl_gc.c:1995`/`:2061-2098` for a fullsweep, `:1512-1554` for a minor). Here the
    /// live-set-bounded quantity is the live count itself, which is exactly what the C's
    /// `mature_size` stands for: the data a minor has to move. Comparing the room against the
    /// fill instead made every collection escalate to a fullsweep as soon as one dead burst
    /// exceeded the mature space's room — permanently, because the fullsweep's own sizing is a
    /// function of the **live set** ([`Self::collect_major`]), so no fullsweep can ever buy
    /// room for a dead figure.
    ///
    /// The two bounds agree by construction: a fullsweep sizes the mature space from the live
    /// set plus the young occupancy it clamps to the nursery capacity that collapse installs
    /// (`HeapPolicy::mature_after_fullsweep`), so the room a fullsweep leaves is always at
    /// least a live young set's worth — the collection after it is a minor. The fill is still
    /// consulted **first** as a cheap short-circuit: it is an upper bound on the live count, so
    /// when it already fits the answer is the same and the walk is skipped.
    fn minor_promotion_fits(&self, roots: &[Eterm]) -> bool {
        let available = self.mature.available_words();
        available >= self.nursery.used_words() || available >= self.young_promotion_words(roots)
    }

    /// Whether the old generation **exists** yet — the Rust mirror of the C's
    /// `OLD_HEAP(p) != NULL`, i.e. "there is somewhere for a minor to promote into".
    ///
    /// The C creates the old heap **inside a collection**, never at spawn
    /// (`erl_gc.c:1470-1478`, the "Create new, empty old_heap" block of `minor_collection`;
    /// `major_collection` discards and re-creates it). A process's first collection therefore
    /// always finds `OLD_HEAP(p) == NULL`: `minor_collection`'s "is there an old heap *and* is it
    /// large enough?" test is
    ///
    /// ```c
    /// if (OLD_HEAP(p) &&
    ///     ((mature_size <= OLD_HEND(p) - OLD_HTOP(p)) && …))
    /// ```
    ///
    /// (`erl_gc.c:1486-1488`), so with no old heap the minor cannot take its copy branch: it
    /// falls through to the "Not enough room for a minor collection. Must force a major
    /// collection." `return -1` (`erl_gc.c:1574-1577`), which `garbage_collect` turns into its
    /// `do_major_collection` branch (`erl_gc.c:839-849`). The **first** collection of every
    /// process is that fullsweep.
    ///
    /// This crate's old-generation fact is [`HeapObservable::old_heap_size_words`] — the C's
    /// `p->old_heap ? old_hend - old_heap : 0` (`erts/emulator/beam/erl_bif_info.c:1023-1030`),
    /// which answers `0` exactly while no collection has created the generation. It is a
    /// *derived, observable* fact here rather than the raw pointer the C reads, but it is the
    /// same fact: the mature space is pre-allocated empty by
    /// [`Collector::with_allocator`](Collector::with_allocator), and only a collection makes it
    /// the old generation.
    ///
    /// It also covers the second, independent ground, capacity: a mature space of
    /// `capacity_words() == 0` cannot receive a single promoted word, so no minor that has
    /// anything to promote can succeed into it — and such a minor would not create the
    /// generation either, leaving the collector permanently on the wrong side of the C's test.
    /// The `old_heap_size_words()` spelling answers `0` for both readings (never collected, or
    /// an empty-capacity old generation), so it is the one this guard relies on.
    ///
    /// What the guard must not be replaced by is [`minor_promotion_fits`] alone: that predicate
    /// answers `available >= used_words`, which is **vacuously true** for an empty nursery
    /// (`0 >= 0`) — and the first collection of a fresh process is precisely the run that has to
    /// create the old generation, whether or not it has anything to promote yet.
    fn has_old_generation(&self) -> bool {
        self.old_heap_size_words() != 0
    }

    /// Runs a collection, picking minor vs major by policy and escalating a
    /// would-not-fit minor to a major.
    ///
    /// A major is chosen when the fullsweep cadence is due
    /// ([`HeapPolicy::fullsweep_due`], the C `GEN_GCS(p) >= MAX_GEN_GCS(p)`,
    /// `erl_gc.c:694,831`), **or** when there is no old generation yet
    /// ([`has_old_generation`] — the C `OLD_HEAP(p)` test at `erl_gc.c:1486-1488` whose failure
    /// sends the first collection to `do_major_collection`, `erl_gc.c:1574-1577`, `:839-849`),
    /// **or** when the mature space cannot receive the nursery's live promotion
    /// ([`minor_promotion_fits`] — the same `erl_gc.c:1486-1488` room test). Otherwise a minor
    /// runs.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        let kind = if self.policy.fullsweep_due(self.minor_since_major)
            || !self.has_old_generation()
            || !self.minor_promotion_fits(roots)
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

    /// Runs a collection of **exactly** `kind` — the kind-pinned counterpart of
    /// [`collect`](Self::collect).
    ///
    /// [`collect`](Self::collect) *chooses* the kind (the fullsweep cadence, or a mature space
    /// too small for the nursery's **live** promotion —
    /// [`minor_promotion_fits`](Self::minor_promotion_fits)) and **escalates** a would-not-fit
    /// minor to a major. This entry point never chooses and never escalates:
    /// [`CollectionKind::Minor`] runs [`collect_minor`](Self::collect_minor) and
    /// [`CollectionKind::Major`] runs [`collect_major`](Self::collect_major), so on success the
    /// returned [`Collection::kind`] is the requested `kind`.
    ///
    /// The difference is observable exactly when a minor would not fit its destination:
    /// `collect` would run a major instead, while `collect_kind(CollectionKind::Minor)` returns
    /// the typed heap-exhaustion [`BeamError`](beam_common::BeamError) **before a single word is
    /// copied**, leaving the nursery and the mature space untouched (never a half-forwarded
    /// heap, and no counter advanced).
    ///
    /// This is a thin dispatch: the collection logic, the heap sizing and the counters all stay
    /// in the two `collect_*` methods.
    ///
    /// # Errors
    ///
    /// The selected collection's own `BeamError` — the typed heap exhaustion above for a minor
    /// that cannot fit, or an allocation failure. A `Minor` failure is never masked by a major.
    pub fn collect_kind(
        &mut self,
        kind: CollectionKind,
        roots: &mut [Eterm],
    ) -> Result<Collection> {
        let stats = match kind {
            CollectionKind::Minor => self.collect_minor(roots)?,
            CollectionKind::Major => self.collect_major(roots)?,
        };
        Ok(Collection { kind, stats })
    }



    /// Runs a minor collection: evacuate the nursery's live terms into the mature space.
    ///
    /// The destination must fit the promotion up front (immediate promotion, no partial-copy
    /// rollback). [`collect`](Self::collect) escalates to a major when it would not; a
    /// **direct** call instead reports the typed heap exhaustion *before* a single word is
    /// copied, so a too-small mature space can never leave the nursery half-forwarded.
    ///
    /// The room requirement is the **live** young word count
    /// ([`young_promotion_words`](Self::young_promotion_words)) — the exact number of words
    /// this call writes into the mature space, which is both what the copier needs and the
    /// quantity the C's minor decision stands for (`erl_gc.c:1486-1488`; see
    /// [`minor_promotion_fits`](Self::minor_promotion_fits)). The nursery's chain-summed fill
    /// is an upper bound on it and is consulted first, so the read-only walk that counts the
    /// live graph runs only when that cheap bound does not already fit. A fill-keyed test would
    /// refuse a promotion the live set fits into whenever the mutator had abandoned more words
    /// than the mature space has room for — permanently, because nothing ever buys room for a
    /// dead figure.
    ///
    /// Every space of a chained nursery is evacuated, and the chain is then
    /// [collapsed](crate::nursery::Nursery::collapse) back to one space, sized from the **young
    /// live set** the round evacuated — its copy volume
    /// ([`HeapPolicy::young_after_collection`]).
    pub fn collect_minor(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from);
        if self.mature.available_words() < self.nursery.used_words() {
            // The whole fill does not fit, so only the live set can decide: count it.
            let live = crate::live::live(&from, roots).words;
            if self.mature.available_words() < live {
                return Err(heap_exhausted(live));
            }
        }
        let stats = Copier::new(&from, &mut self.mature).run(roots)?;
        self.minor_since_major += 1;
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        self.stats.minor_collections += 1;
        // The nursery is empty again: size it from the **young live set** this round evacuated —
        // the words it copied (`HeapPolicy::young_after_collection`). A minor's copy volume *is*
        // its young live set, and that is the only quantity the round measured; the mature
        // space's occupancy is not it, because a minor never copies the mature generation (only
        // a fullsweep does), so it carries the garbage the minors since the last fullsweep
        // promoted and would inflate the target until the capacity tracked the run's total
        // allocation instead of the live set.
        self.nursery.collapse(&self.policy, stats.words_copied)?;
        Ok(stats)
    }

    /// Runs a major (fullsweep) collection: evacuate young and mature into a fresh space.
    ///
    /// The fresh space is sized up front so the copy always fits and the two from-spaces are
    /// never left half-forwarded: the policy is asked for room for the **exact live set** the
    /// fullsweep evacuates plus the **young heap's occupancy**, clamped to the capacity that
    /// nursery is about to have (a whole nursery's worth of promotion, the C `need_after`,
    /// `erl_gc.c:2061-2098`; the C's `size_before` term is one young block's usage inside that
    /// block's `HEAP_SIZE`, `erl_gc.c:1875-1888`) plus its reserve, so the next collections can
    /// be minors again. Should [`max_heap_words`](HeapPolicy::max_heap_words) clamp that size
    /// below the live set, the typed heap exhaustion is returned *before* a single word is
    /// copied rather than mid-copy.
    ///
    /// The nursery is then re-sized from the **exact live set** the fullsweep measured
    /// ([`HeapPolicy::young_after_fullsweep`]), so it neither re-copies the live set every
    /// round nor keeps the capacity a dead burst left behind.
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
        // by now), so feeding the policy the **mature** occupancy inflates `need_after` past
        // the space's own capacity and its grow bracket (`capacity < target`) fires on every
        // fullsweep, so the mature capacity creeps up one growth-table step
        // per fullsweep, without bound, for a live set that never changes (see
        // [`crate::live`]).
        //
        // The young occupancy is a different quantity and *is* included: the fresh mature
        // space has to be able to receive a whole nursery's worth of promotion after the
        // fullsweep, otherwise the very next collection cannot be a minor and every
        // collection escalates to a fullsweep — the generational cadence never runs. But it is
        // included **as the C includes it**, clamped to the capacity that nursery will
        // actually have: the C's `need_after` term is `HEAP_TOP(p) - HEAP_START(p)`, the usage
        // of a *single* young block, so it can never exceed that block's `HEAP_SIZE` — and the C
        // re-sizes the young block from the live set at every collection (a fullsweep frees the
        // old heap, `erl_gc.c:1875-1888`, and `adjust_after_fullsweep` sizes the next one from
        // the post-copy live set, `erl_gc.c:2061-2098`), so `used <= HEAP_SIZE` is a
        // live-set-derived bound. The bound restored here is exactly the capacity
        // [`crate::nursery::Nursery::collapse_after_fullsweep`] installs immediately below:
        // [`HeapPolicy::young_after_fullsweep`] of the capacity the nursery chain has now and the
        // live set this fullsweep measured. That is a pure function of the live set, upper-bounded
        // by `YOUNG_SHRINK_FACTOR` (8) times `HeapPolicy::young_target_words(live)` — past that
        // the collapse installs the target itself — so it is bounded, and it is the room the next
        // minor really has.
        //
        // The clamp is what keeps the fresh mature space a function of the **live set**.
        // `Nursery::used_words` is the chain-summed occupancy of every nursery space, so it
        // also counts every dead word the mutator (or a host's unrooted BIF copy-out, which
        // bumps the process heap and never rewinds) accumulated since the last collection.
        // That figure is genuinely unbounded — a runaway process fills the nursery with
        // millions of dead words — and folded into `need_after` it is baked into the next mature
        // space: the following fullsweep then measures a larger live set and grows again — the
        // mature-capacity feedback (see `tests/mature_capacity_feedback.rs`). Clamping leaves the
        // term's purpose (a whole nursery's worth of promotion room, and therefore a *run* of
        // minors between fullsweeps) intact while making it independent of the allocation
        // history.
        let live = crate::live::live(&from, roots).words;
        let young_capacity_after = self
            .policy
            .young_after_fullsweep(self.nursery.capacity_words(), live);
        let young_occupancy = self.nursery.used_words().min(young_capacity_after);
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
        // young set to size it from. It is sized from the **live set** instead
        // ([`HeapPolicy::young_after_fullsweep`]): a fullsweep is the one collection that
        // measures the whole heap exactly, so it is where the nursery learns how large it
        // should be — and where a nursery the growth seam stepped up for a burst is given back
        // once that burst is dead.
        self.nursery
            .collapse_after_fullsweep(&self.policy, live)?;
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
    /// consistent (never a half-forwarded heap) and, for a request the growth table itself
    /// cannot reach, untouched. The work is bounded, so this never loops forever.
    ///
    /// [`max_heap_words`](HeapPolicy::max_heap_words) bounds the nursery chain's **summed**
    /// capacity, which is what the explicit [`grow_nursery`](Self::grow_nursery) step enforces:
    /// a request the remaining budget cannot reach is refused there. That refusal comes *after*
    /// the collection — deliberately, because the collection collapses the chain and therefore
    /// gives the budget back, so pre-checking the chain before collecting would refuse requests
    /// a collection makes reachable. Only the growth table's own ceiling (the single-space
    /// `next_words` rule, which no collection can change) is checked up front, which is why the
    /// fast path below reads a per-space size and not the chain.
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
    ///
    /// The ceiling bounds the chain's **summed** capacity ([`heap_size_words`](Self::heap_size_words)),
    /// not each individual space, so the fresh space may only claim the budget the chain has left
    /// and `heap_size_words() <= max_heap_words` after every successful growth — a growth loop can
    /// no longer append one ceiling-sized space per call. A retired space a previous growth left
    /// provably **empty** (`used_words() == 0`) is re-adopted instead of replaced, which consumes
    /// no budget at all.
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

    /// The mature generation's capacity **once a collection has populated it**, in words —
    /// the C's `old_hend - old_heap`, which exists only after the old generation has been
    /// created by a collection (`erl_gc.c:1470-1478`).
    fn old_heap_size_words(&self) -> usize {
        old_heap_size(
            self.stats.minor_collections + self.stats.major_collections,
            self.mature.capacity_words(),
        )
    }
    /// The words handed out across **every** nursery space — the allocatable current one and
    /// each space a [`grow_nursery`](Self::grow_nursery) retired — so a chained nursery reports
    /// its whole occupancy rather than only the space new terms go into.
    fn heap_used_words(&self) -> usize {
        self.nursery.used_words()
    }

    /// The whole nursery chain's occupancy plus the mature generation's, in words.
    fn total_heap_used_words(&self) -> usize {
        self.nursery.used_words() + self.mature.used_words()
    }
    fn heap_policy(&self) -> &HeapPolicy {
        &self.policy
    }
}

#[cfg(test)]
mod tests {
    //! The kind-pinned collection entry point: [`Collector::collect_kind`] runs **exactly** the
    //! requested [`CollectionKind`] — no policy choice and no minor→major escalation — whether or
    //! not the policy would have picked a major, and a minor whose promotion cannot fit the
    //! mature space reports the typed heap exhaustion with every counter and both spaces
    //! untouched (never a half-forwarded heap, never a silent fullsweep).
    //!
    //! `unsafe` below only reads words this test allocated itself or that a collection just
    //! relocated; every block carries a `// SAFETY:` rationale.

    use super::*;
    use beam_common::BeamError;
    use beam_term::prelude::{arityval, boxed_val, is_arity_value, make_small, signed_value};

    /// Allocates the 2-tuple `{1,2}` in the nursery.
    fn build_tuple(c: &mut Collector) -> Eterm {
        c.young_mut()
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("the tuple fits the nursery")
    }

    /// Asserts the relocated root is a live 2-tuple `{1,2}` in the mature space.
    fn assert_tuple_preserved(c: &Collector, root: Eterm) {
        assert!(
            c.mature().contains_term(root),
            "the root must be resident in the mature space"
        );
        assert!(
            !c.young().contains_term(root),
            "the root must no longer be in the nursery"
        );
        // SAFETY: `root` is a live 2-element tuple in the mature space (asserted above).
        unsafe {
            let header = *boxed_val(root);
            assert!(is_arity_value(header));
            assert_eq!(arityval(header), 2);
            assert_eq!(signed_value(*boxed_val(root).add(1)), 1);
            assert_eq!(signed_value(*boxed_val(root).add(2)), 2);
        }
    }

    /// `fullsweep_after: 0` makes the cadence fire on every collection, so
    /// [`Collector::collect`] picks a major; `collect_kind(Minor)` still runs a minor — the pin
    /// is what decides, not the policy — and the live term survives relocation.
    #[test]
    fn collect_kind_minor_runs_a_minor_even_when_the_policy_forces_a_major() {
        let policy = HeapPolicy {
            fullsweep_after: 0,
            ..HeapPolicy::default()
        };

        // `collect` (unchanged) selects a major under this policy.
        let mut chosen = Collector::new(policy.clone()).expect("collector");
        let chosen_root = build_tuple(&mut chosen);
        let mut chosen_roots = [chosen_root];
        let chosen_collection = chosen.collect(&mut chosen_roots).expect("collect");
        assert_eq!(
            chosen_collection.kind,
            CollectionKind::Major,
            "fullsweep_after = 0 makes the policy pick a major"
        );

        // `collect_kind(Minor)` ignores that choice and runs a minor.
        let mut c = Collector::new(policy).expect("collector");
        let root = build_tuple(&mut c);
        let mut roots = [root];
        let pinned = c
            .collect_kind(CollectionKind::Minor, &mut roots)
            .expect("the pinned minor fits");

        assert_eq!(pinned.kind, CollectionKind::Minor, "the kind is pinned");
        assert_eq!(pinned.stats.words_copied, 3, "the tuple is 3 words");
        assert_eq!(c.minor_gcs(), 1, "the minor counter advanced");
        // The returned `stats` is the copier's per-collection delta, whose collection counters
        // stay 0; the counters themselves accumulate on the collector's own `stats()`.
        assert_eq!(c.stats().minor_collections, 1);
        assert_eq!(c.stats().major_collections, 0, "no major ran");
        assert_eq!(c.young().used_words(), 0, "the nursery is emptied");
        assert_tuple_preserved(&c, roots[0]);
    }

    /// `collect_kind(Major)` runs a fullsweep, and a fullsweep resets `minor_gcs` to 0 (the C
    /// `GEN_GCS` reset) while advancing the major counters.
    #[test]
    fn collect_kind_major_runs_a_fullsweep_and_resets_minor_gcs() {
        let mut c = Collector::new(HeapPolicy::default()).expect("collector");
        let root = build_tuple(&mut c);
        let mut roots = [root];

        let minor = c
            .collect_kind(CollectionKind::Minor, &mut roots)
            .expect("pinned minor");
        assert_eq!(minor.kind, CollectionKind::Minor);
        assert_eq!(c.minor_gcs(), 1);

        let major = c
            .collect_kind(CollectionKind::Major, &mut roots)
            .expect("pinned major");
        assert_eq!(major.kind, CollectionKind::Major);
        assert_eq!(major.stats.words_copied, 3, "the live 3-word tuple");
        assert_eq!(c.stats().major_collections, 1);
        assert_eq!(c.stats().minor_collections, 1, "no extra minor ran");
        assert_eq!(c.minor_gcs(), 0, "a major resets the minor count");
        assert_tuple_preserved(&c, roots[0]);
    }

    /// A pinned minor whose promotion cannot fit the mature space returns the typed
    /// heap-exhaustion error — it is **not** escalated to a major (which would make room) — and
    /// leaves every counter and both spaces exactly as they were.
    #[test]
    fn collect_kind_minor_never_escalates_and_leaves_the_heap_untouched() {
        // A mature space of 8 words cannot receive a 233-word nursery's promotion.
        let policy = HeapPolicy {
            initial_young_words: 233,
            initial_mature_words: 8,
            ..HeapPolicy::default()
        };
        let mut c = Collector::new(policy).expect("collector");

        let heap = c.young_mut().heap_mut();
        let mut live = Vec::new();
        for i in 0..20i64 {
            live.push(
                heap.cons(make_small(i), beam_term::prelude::NIL)
                    .expect("cell"),
            );
        }
        let mut roots = live.clone();
        assert!(
            c.young().used_words() > c.mature().available_words(),
            "the promotion really cannot fit"
        );

        let stats_before = c.stats();
        let young_capacity = c.young().capacity_words();
        let mature_capacity = c.mature().capacity_words();
        let mature_used = c.mature().used_words();
        let spaces = c.young_space_count();

        let err = c
            .collect_kind(CollectionKind::Minor, &mut roots)
            .expect_err("the pinned minor must not fit");
        assert!(matches!(err, BeamError::Alloc(_)), "got {err:?}");

        // No escalation, no copy, no counter and no geometry change.
        assert_eq!(c.stats(), stats_before, "no counter advanced");
        assert_eq!(c.minor_gcs(), 0);
        assert_eq!(c.young().capacity_words(), young_capacity);
        assert_eq!(c.mature().capacity_words(), mature_capacity);
        assert_eq!(c.mature().used_words(), mature_used, "nothing was copied");
        assert_eq!(c.young_space_count(), spaces);
        for root in &roots {
            assert!(
                c.young().contains_term(*root),
                "every root is still in the nursery"
            );
        }

        // The same collector *does* make room when the caller asks for a major.
        let escalated = c
            .collect_kind(CollectionKind::Major, &mut roots)
            .expect("the major makes room");
        assert_eq!(escalated.kind, CollectionKind::Major);
        assert_eq!(c.stats().major_collections, 1);
    }
}
