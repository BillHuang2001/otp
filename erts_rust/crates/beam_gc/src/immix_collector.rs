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

//! [`GenImmixCollector`] — the **copying young nursery** paired with the GenImmix-style
//! **mark-region mature space** ([`ImmixSpace`](crate::immix::ImmixSpace)).
//!
//! This is the integrated, config-selectable form of the Stage-4 GenImmix option: it
//! replaces the copying collector's mature `HeapSpace` with the line/block-based
//! [`ImmixSpace`] while keeping the nursery a plain barrier-free copying space. The two
//! strategies share everything the workspace pins down — the frozen term layout, the
//! flat root-slice contract, the shared word-level evacuation primitives of
//! [`crate::copy`], and the off-heap/out-of-space rules — so the only difference is
//! *where* the mature generation lives and *how* it is reclaimed.
//!
//! # The two collections
//!
//! * A **minor** promotes the nursery's live terms straight into the mature Immix space
//!   (the same immediate-promotion rule as [`crate::Collector`]). Reclaiming the mature
//!   generation is deferred, so a minor copies only what the nursery kept alive.
//! * A **major** first reclaims the mature generation **in place**
//!   ([`ImmixSpace::collect`]: mark, classify, evacuate `Partial` blocks, pin `AllMarked`
//!   blocks, free recycled holes) and then promotes the nursery. See "Collection flow"
//!   below for the exact ordering and why it is the sound one.
//!
//! # Barrier-free invariant
//!
//! A **mature object must never point at a young object** (that is what lets mature
//! objects be left untouched by a minor). Promotion only ever moves young objects *into*
//! the mature space, so the invariant holds after every collection. It also holds across
//! the major's two phases because the promotion — which follows every young reference
//! transitively — runs after the in-place mature reclaim, never the other way round
//! without first resolving the nursery's references (see below).
//!
//! # Collection flow
//!
//! Both collections begin with [`scan_young`], a read-only walk of the live nursery graph
//! reachable from `roots` that reports two things: the number of Immix **lines** copies of
//! the live objects will occupy (`needed`), and every nursery reference word that points
//! **into the mature space** (aligned `(slot, term)` pairs).
//!
//! * **Minor** ([`GenImmixCollector::run_minor`]): if `mature.free_lines() >= needed` the
//!   nursery is [promoted](promote::promote) and reset. `needed` is the exact line count, so
//!   the check is never optimistic about the *total* room a promotion needs.
//! * **Major** ([`GenImmixCollector::run_major`]):
//!   1. **Reclaim mature in place.** The `(slot, term)` pairs are passed as `promoted`
//!      roots to [`ImmixSpace::collect`], so every mature object merely *reachable through
//!      the nursery* is treated as live and is not dropped. A mature object can only be
//!      referenced from a young object (never the reverse), so without this the in-place
//!      reclaim would free objects that the nursery still points at.
//!   2. **Resolve the nursery's references.** [`ImmixSpace::collect`] may *evacuate* a
//!      live object in a `Partial` block, so each recorded nursery slot is rewritten to
//!      the object's final address (the `promoted` entries it was aligned with hold the
//!      updated term). From here the nursery holds only final mature addresses.
//!   3. **Promote the nursery.** Now that the nursery's mature references are final, the
//!      promotion leaves them verbatim (`Foreign`) and cannot create a stale pointer.
//!   4. **Grow instead of failing.** Should the promotion not fit the reclaimed region —
//!      either for want of total room (`free_lines() < needed`) or for want of a
//!      *contiguous run* inside one block, the line-granular failure
//!      [`promote::PromoteOutcome::WouldNotFit`] reports — the fullsweep leaves room
//!      another way: it is relocated wholesale into a fresh mark-region space
//!      ([`GenImmixCollector::grow_major`]), which packs monotonically from empty. A
//!      [minor](GenImmixCollector::run_minor) that is refused is *escalated* to exactly
//!      that fullsweep by [`GenImmixCollector::collect`], so a growable heap never turns a
//!      working program into a failing one. Only a
//!      [`HeapPolicy::max_heap_words`] ceiling that cannot reach a region holding the live
//!      set reports the typed heap-exhaustion [`BeamError`](beam_common::BeamError), and
//!      always *before* anything is promoted: never a panic and never a half-forwarded
//!      heap.
//!
//! # Sharing with the copying engine
//!
//! [`promote`](promote::promote) is driven entirely by [`crate::copy`]'s `pub(crate)`
//! primitives: object sizing is [`move_size`](crate::copy::move_size), reference enumeration
//! is [`for_each_ref`](crate::copy::for_each_ref), and the word-level marker scheme it writes
//! (the forwarding marker, the `copy_nonoverlapping`, the `Target::Forwarded` follow) is the
//! one [`forward_slot`](crate::copy::forward_slot) implements — the promotion re-implements
//! it in two phases to make it *atomic* (see the [`promote`] module), while the copying
//! collector and the mark-region in-place reclaim keep driving `forward_slot` directly.

use std::collections::HashSet;

use beam_alloc::AllocRef;
use beam_common::Result;use beam_term::prelude::{is_boxed, is_header, is_list, ptr_val, Eterm};

use crate::collector::{Collection, CollectionKind};
use crate::config::GcConfig;
use crate::copy::{for_each_ref, move_size};
use crate::error::heap_exhausted;
use crate::heap_info::HeapObservable;
use crate::immix::{lines_for, ImmixSpace, ImmixStats, LINE_WORDS};
use crate::nursery::Nursery;
use crate::policy::HeapPolicy;
use crate::reachable::ReachableObject;
use crate::space::{FromSet, HeapSpace};
use crate::stats::GcStats;

mod promote;
use promote::{promote, PromoteFrom, PromoteOutcome};

/// The result of a nursery scan: the Immix lines a promotion needs, and every nursery
/// reference word that points into the mature space (`(slot, term)` order-aligned).
type YoungScan = (usize, Vec<(*mut Eterm, Eterm)>);
/// Walks the live nursery graph reachable from `roots`, without copying anything.
///
/// Reports the number of Immix **lines** a promotion of that graph will occupy (`needed`)
/// and each nursery reference word pointing **into** `mature` (kept order-aligned with the
/// mature terms they hold). Worklist-based, so a deep list cannot grow the real stack; the
/// graph is a DAG, so the visited set also keeps `needed` an exact object count. `young` is
/// the whole nursery chain, so a growth-retained space is scanned too.
fn scan_young(roots: &[Eterm], young: &Nursery, mature: &ImmixSpace) -> Result<YoungScan> {
    let mut seen: HashSet<usize> = HashSet::new();
    let mut work: Vec<*mut Eterm> = Vec::new();
    let mut needed = 0usize;
    let mut refs: Vec<(*mut Eterm, Eterm)> = Vec::new();

    for &root in roots {
        seed_young(root, young, &mut seen, &mut work);
    }

    while let Some(obj) = work.pop() {
        // SAFETY: `obj` is the first word of a live, reachable nursery object (seeded only
        // through `young.contains`), so it is readable.
        let first = unsafe { *obj };
        // A cons cell has no header; its first word is the car, a term that never carries
        // the `Header` primary tag (`beam_term::tag::is_header`), so the tag says the shape.
        let is_cons = !is_header(first);
        let total = if is_cons { 2 } else { move_size(first, obj) };
        needed += lines_for(total);

        // SAFETY: `obj` addresses a live nursery object of `total` words, all readable.
        unsafe {
            for_each_ref(obj, total, &mut |slot| {
                // SAFETY: `for_each_ref` yields the address of a live reference word of a
                // live nursery object, so reading it is covered by this block.
                let term = *slot;
                if is_boxed(term) || is_list(term) {
                    let ptr = ptr_val(term) as *const Eterm;
                    if young.contains(ptr) {
                        if seen.insert(ptr as usize) {
                            work.push(ptr as *mut Eterm);
                        }
                    } else if mature.contains(ptr) {
                        refs.push((slot, term));
                    }
                }
                Ok(())
            })?;
        }
    }

    Ok((needed, refs))
}

/// Seeds the worklist with `term` when it is a live nursery-chained object.
fn seed_young(
    term: Eterm,
    young: &Nursery,
    seen: &mut HashSet<usize>,
    work: &mut Vec<*mut Eterm>,
) {
    if is_boxed(term) || is_list(term) {
        let ptr = ptr_val(term) as *const Eterm;
        if young.contains(ptr) && seen.insert(ptr as usize) {
            work.push(ptr as *mut Eterm);
        }
    }
}

/// A two-generation collector whose nursery is the copying [`HeapSpace`] and whose mature
/// generation is the mark-region [`ImmixSpace`].
///
/// Selected by [`crate::config::MatureKind::Immix`]; the default configuration still
/// resolves to the copying [`crate::Collector`] (see [`crate::AnyCollector`]). The public
/// surface mirrors [`crate::Collector`] apart from [`mature`](Self::mature), which returns
/// an [`ImmixSpace`] rather than a [`HeapSpace`].
pub struct GenImmixCollector {
    policy: HeapPolicy,
    nursery: Nursery,
    mature: ImmixSpace,
    minor_since_major: u32,
    stats: GcStats,
    last_immix: Option<ImmixStats>,
}

impl GenImmixCollector {
    /// Creates a collector for `config`, over the default allocator.
    ///
    /// The [`MatureKind`](crate::config::MatureKind) of `config` is **ignored**: this type
    /// *is* the Immix-backed collector, so it always builds one over `config.policy()`.
    /// Use [`crate::AnyCollector::from_config`] to dispatch on the configured strategy.
    pub fn with_config(config: &GcConfig) -> Result<GenImmixCollector> {
        Self::from_policy(config.policy().clone())
    }

    /// Creates an Immix-backed collector with `policy`, over the default allocator.
    pub fn from_policy(policy: HeapPolicy) -> Result<GenImmixCollector> {
        Self::with_allocator(beam_alloc::registry::default_allocator().clone(), policy)
    }

    /// Creates an Immix-backed collector with `policy`, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, policy: HeapPolicy) -> Result<GenImmixCollector> {
        let nursery = Nursery::new(alloc.clone(), policy.initial_young_words)?;
        let mature = ImmixSpace::with_allocator(alloc, policy.initial_mature_words)?;
        Ok(GenImmixCollector {
            policy,
            nursery,
            mature,
            minor_since_major: 0,
            stats: GcStats::default(),
            last_immix: None,
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

    /// The mature mark-region space.
    pub fn mature(&self) -> &ImmixSpace {
        &self.mature
    }

    /// The collector's accumulated statistics.
    pub fn stats(&self) -> GcStats {
        self.stats
    }

    /// The work the most recent **major** collection's in-place mature reclaim performed
    /// (`None` until a major has run, or when the most recent major was a *relocating*
    /// fullsweep that resized the region instead — see
    /// [`HeapPolicy::mature_after_fullsweep`]). Lets a caller read the mark-region
    /// accounting — the objects evacuated vs pinned, the blocks reclaimed — that
    /// [`GcStats`] folds away.
    pub fn last_immix_stats(&self) -> Option<ImmixStats> {
        self.last_immix
    }

    /// The number of minor collections run since the last fullsweep — the C `GEN_GCS`, and
    /// the value the `garbage_collection` item reports as `minor_gcs`.
    pub fn minor_gcs(&self) -> u32 {
        self.minor_since_major
    }

    /// Whether the nursery can no longer satisfy `need_words` more words — the same signal
    /// [`crate::Collector`] uses (an exact-fit allocation still fits).
    ///
    /// This reads the **current** (allocatable) space only — a [growth](Self::grow_nursery)
    /// retires the space it replaced and nothing is ever allocated in a retired one, so the
    /// current space is the correct one for an allocation-admission signal. A *collection*
    /// must instead cover the whole chain, which is why [`collect`](Self::collect) sizes the
    /// promotion (and the region the fullsweep leaves behind) from the chain-wide
    /// [`Nursery::used_words`].
    ///
    /// The chain's [collection debt](Self::collection_debt) is deliberately **not** folded in —
    /// see that method for the opt-in seam a host consults alongside this one.
    pub fn should_collect(&self, need_words: usize) -> bool {
        self.nursery.current().available_words() < need_words
    }

    /// Whether the nursery chain carries a space a [growth](Self::grow_nursery) retired — the
    /// chain's **collection debt** (the [`crate::Collector`] seam, made strategy-independent).
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
    /// see the handoff in the crate `CONTEXT.md`.
    pub fn collection_debt(&self) -> bool {
        self.nursery.retains_old_spaces()
    }

    /// Runs a collection, picking minor vs major by policy and escalating a would-not-fit
    /// minor to a major.
    ///
    /// A major is chosen when the fullsweep cadence is due *or* when a promotion of the
    /// live nursery (`needed` lines) plainly exceeds the mature space's free lines — the
    /// escalation makes the major's in-place reclaim, rather than a doomed copy, the way
    /// room is found. The chosen minor can still be **refused** by the destination
    /// ([`PromoteOutcome::WouldNotFit`]): `free_lines()` counts the region's free lines in
    /// total, while a line-granular reservation needs a *contiguous run* inside one block, so
    /// a fragmented region can satisfy the first and not the second. Because the promotion is
    /// atomic, that refusal leaves the nursery and the mature region exactly as they were, and
    /// this method runs the major the plain check would have chosen had it seen the
    /// fragmentation.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        let (needed, refs) = scan_young(roots, &self.nursery, &self.mature)?;
        let tight = self.mature.free_lines() < needed;
        let kind = if self.policy.fullsweep_due(self.minor_since_major) || tight {
            CollectionKind::Major
        } else {
            CollectionKind::Minor
        };
        let (kind, stats) = match kind {
            CollectionKind::Minor => match self.run_minor(roots)? {
                PromoteOutcome::Done(stats) => (CollectionKind::Minor, stats),
                // The region has the free lines but not the run an object of the promotion
                // needs: the atomic attempt left nothing behind, so the same `needed`/`refs`
                // drive the escalation the check above could not see.
                PromoteOutcome::WouldNotFit => (
                    CollectionKind::Major,
                    self.run_major(roots, needed, refs)?,
                ),
            },
            CollectionKind::Major => (
                CollectionKind::Major,
                self.run_major(roots, needed, refs)?,
            ),
        };
        self.accumulate(&stats, kind);
        Ok(Collection { kind, stats })
    }

    /// Runs a **minor** collection: promote the nursery's live terms into the mature space.
    ///
    /// Checks up front that the mature space's free lines cover the promotion, so a
    /// too-large minor is a typed heap exhaustion *before* a single word is copied (never a
    /// half-forwarded heap); [`collect`](Self::collect) escalates to a major instead. The
    /// promotion itself is atomic, so a region whose free lines are too *fragmented* for the
    /// promotion reports the same typed exhaustion — equally with the nursery byte-identical
    /// and the mature region untouched — rather than a half-forwarded heap.
    pub fn collect_minor(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let (needed, _refs) = scan_young(roots, &self.nursery, &self.mature)?;
        if self.mature.free_lines() < needed {
            return Err(heap_exhausted(needed.saturating_mul(LINE_WORDS)));
        }
        match self.run_minor(roots)? {
            PromoteOutcome::Done(stats) => {
                self.accumulate(&stats, CollectionKind::Minor);
                Ok(stats)
            }
            // As above: the region's free lines exist but no contiguous run of them can hold
            // an object of the promotion. Nothing was copied and nothing rewritten, so the
            // typed exhaustion is exactly the one the up-front check raises.
            PromoteOutcome::WouldNotFit => Err(heap_exhausted(needed.saturating_mul(LINE_WORDS))),
        }
    }

    /// Runs a **major** collection: reclaim the mature generation in place, then promote
    /// the nursery into it.
    pub fn collect_major(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        let (needed, refs) = scan_young(roots, &self.nursery, &self.mature)?;
        let stats = self.run_major(roots, needed, refs)?;
        self.accumulate(&stats, CollectionKind::Major);
        Ok(stats)
    }

    /// Grows the nursery **on demand** so that a `need_words`-word allocation can proceed —
    /// the [`crate::Collector::ensure_capacity`] seam for the mark-region strategy.
    ///
    /// It collects once (a minor, escalated to a major when the promotion would not fit),
    /// then grows the now-empty nursery to the smallest curated size that covers
    /// `need_words` (via the shared [`crate::nursery::Nursery::grow`]). The returned
    /// [`Collection`] is the one it ran; afterwards `young().available_words() >= need_words`
    /// or the typed heap-exhaustion error is returned (never a half-forwarded heap, never an
    /// unbounded loop).
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
    /// collecting and without moving any live term** — the no-relocation growth seam, the
    /// [`crate::Collector::grow_nursery`] counterpart for the mark-region strategy.
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

    /// Visits every **boxed object** reachable from `roots` across the **whole heap** — every
    /// space of the nursery chain (the current one and any a
    /// [`grow_nursery`](Self::grow_nursery) retired) plus the mark-region mature generation —
    /// passing the object's header word and a pointer to that header, each object **exactly
    /// once**.
    ///
    /// The mark-region sibling of
    /// [`Collector::for_each_reachable_object`](crate::Collector::for_each_reachable_object);
    /// see it for the full contract. The only difference is the mature range: a
    /// [`ImmixSpace`] is one contiguous block-aligned region whose free lines are still inside
    /// it, so (as in [`crate::live::live_full`]) the from-set covers `base .. base + capacity`
    /// — a **deliberately over-inclusive** range, harmless here because a walk only ever visits
    /// objects a live reference reaches, and a free line is never a live reference's target.
    ///
    /// **Layout-agnostic**, **read-only**, **iterative and cycle-safe**; a cons cell is
    /// traversed but not reported (it carries no header word).
    pub fn for_each_reachable_object(
        &self,
        roots: &[Eterm],
        visit: impl FnMut(Eterm, *const Eterm),
    ) -> Result<()> {
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from); // every space of the chain
        let base = self.mature.base() as usize;
        from.add_range(base, base + self.mature.capacity_words() * core::mem::size_of::<Eterm>());
        crate::reachable::for_each_reachable(&from, roots, visit)
    }

    /// The **safe** form of [`for_each_reachable_object`](Self::for_each_reachable_object):
    /// visits every **boxed object** reachable from `roots` across the **whole heap** (every
    /// space of the nursery chain plus the mark-region mature generation), each object
    /// **exactly once**, passing a read-only [`ReachableObject`] view of it.
    ///
    /// It builds the **same from-set** as the raw sibling and drives the same walk, so the two
    /// report the same objects in the same order; the only difference is that the visitor reads
    /// the object's words through [`ReachableObject::word`] / [`ReachableObject::words`] instead
    /// of a raw pointer, so an inspection-only caller needs **no `unsafe`**. See
    /// [`Collector::for_each_reachable_object`](crate::Collector::for_each_reachable_object) for
    /// the full contract; the mature range is the mark-region space's deliberately
    /// over-inclusive `base .. base + capacity`.
    pub fn for_each_reachable_object_view(
        &self,
        roots: &[Eterm],
        visit: impl FnMut(ReachableObject<'_>),
    ) -> Result<()> {
        let mut from = FromSet::new();
        self.nursery.add_to_from_set(&mut from); // every space of the chain
        let base = self.mature.base() as usize;
        from.add_range(base, base + self.mature.capacity_words() * core::mem::size_of::<Eterm>());
        crate::reachable::for_each_reachable_view(&from, roots, visit)
    }

    /// Folds one collection's [`GcStats`] into the collector's accumulated counters.
    fn accumulate(&mut self, stats: &GcStats, kind: CollectionKind) {
        self.stats.words_copied += stats.words_copied;
        self.stats.words_reclaimed += stats.words_reclaimed;
        self.stats.objects_copied += stats.objects_copied;
        match kind {
            CollectionKind::Minor => self.stats.minor_collections += 1,
            CollectionKind::Major => self.stats.major_collections += 1,
        }
    }

    /// Promotes the nursery's live terms into the mature space and resets the nursery.
    ///
    /// The caller has already checked that `mature.free_lines()` covers the promotion's
    /// `needed` lines. That check is region-wide, though, so the promotion itself can still be
    /// refused by a fragmented region (no contiguous run for an object); the attempt is then
    /// [`PromoteOutcome::WouldNotFit`] — the nursery and the mature region exactly as they
    /// were, no marker written, no root rewritten, no counter advanced — which
    /// [`collect`](Self::collect) escalates to a fullsweep. Every space of a chained nursery
    /// is promoted out of, and the chain is then
    /// [collapsed](crate::nursery::Nursery::collapse) back to one space.
    fn run_minor(&mut self, roots: &mut [Eterm]) -> Result<PromoteOutcome> {
        let from = PromoteFrom::new(&self.nursery, None);
        let PromoteOutcome::Done(mut stats) = promote(&from, &mut self.mature, roots)? else {
            return Ok(PromoteOutcome::WouldNotFit);
        };
        let used = self.nursery.used_words();
        // The nursery words that were not promoted were garbage: promotion is the whole of
        // a minor's reclamation.
        stats.words_reclaimed = used.saturating_sub(stats.words_copied);
        self.minor_since_major += 1;
        // What the minor left in the nursery is exactly what it promoted (see
        // `HeapPolicy::young_after_collection`), never the pre-collection fill.
        self.nursery.collapse(&self.policy, stats.words_copied)?;
        Ok(PromoteOutcome::Done(stats))
    }
    /// Reclaims the mature generation in place, then promotes the nursery into it.
    ///
    /// `needed` and `refs` come from the [`scan_young`] the caller ran before choosing the
    /// collection, so the nursery is scanned exactly once per collection.
    ///
    /// When the heap-size policy asks for a different mature-region size
    /// ([`HeapPolicy::mature_after_fullsweep`]) the fullsweep **relocates** instead — see
    /// [`relocate_major`](Self::relocate_major). When the policy *keeps* the capacity but
    /// the in-place reclaim still cannot hold the promotion, the fullsweep falls back to
    /// growing the region — see [`grow_major`](Self::grow_major).
    fn run_major(
        &mut self,
        roots: &mut [Eterm],
        needed: usize,
        refs: Vec<(*mut Eterm, Eterm)>,
    ) -> Result<GcStats> {
        // The size this fullsweep should leave behind, from the **exact live graph** it
        // evacuates (nursery *and* mature, reachable-from-roots only) plus the **young heap's
        // occupancy** — the C `adjust_after_fullsweep`'s
        // `need_after = (HEAP_TOP(p) - HEAP_START(p)) + need + …` (`erl_gc.c:2072`). The
        // region must be able to receive a whole nursery's worth of promotion, or the very
        // next collection cannot be a minor and the cadence degenerates into a fullsweep
        // every time. It is still **never** the *mature* occupancy: that is garbage-inflated
        // (the graphs the minors promoted and the next round's roots replaced), so using it
        // would make the policy's `should_grow` fire on every fullsweep and creep the region
        // up without bound (see [`crate::live`]). The young occupancy is bounded by the
        // nursery capacity, which `young_after_collection` keeps bounded.
        let live = crate::live::live_full(self.nursery(), &self.mature, roots);
        let capacity = self.mature.capacity_words();
        let young_occupancy = self.nursery.used_words();
        let target = self
            .policy
            .mature_after_fullsweep(capacity, live.words, young_occupancy);
        if target != capacity {
            return self.relocate_major(roots, target, live.lines);
        }

        // The nursery's mature references, as additional roots: a mature object reachable
        // only through the live nursery must survive the in-place reclaim.
        let mut promoted: Vec<Eterm> = refs.iter().map(|(_, term)| *term).collect();
        let immix = self.mature.collect(roots, &mut promoted)?;
        self.last_immix = Some(immix);

        // `ImmixSpace::collect` may have evacuated these objects, so rewrite each nursery
        // slot to the final address its (order-aligned) `promoted` entry now holds. The
        // nursery itself was untouched by the mature reclaim, so the slots still address
        // live reference words.
        for ((slot, _), new) in refs.iter().zip(promoted.iter()) {
            // SAFETY: `slot` addresses a live reference word of a reachable nursery object.
            unsafe { **slot = *new };
        }

        if self.mature.free_lines() < needed {
            // Even after reclaiming the mature generation the *in-place* region cannot hold
            // the promotion: grow the region instead of failing — see [`Self::grow_major`].
            return self.grow_major(roots, needed, live.lines);
        }

        // Attempt the promotion atomically. A line-granular destination can refuse it even
        // with room in total (`free_lines()` counts the whole region, `find_run` needs one
        // block), and the refusal leaves both spaces exactly as they are — the in-place
        // reclaim's work is intact and the nursery is un-forwarded — so the same fallback
        // answers it.
        let used = self.nursery.used_words();
        match promote(&PromoteFrom::new(&self.nursery, None), &mut self.mature, roots)? {
            PromoteOutcome::Done(mut stats) => {
                let nursery_garbage = used.saturating_sub(stats.words_copied);
                // Total words reclaimed: the mature generation's in-place reclaim plus the
                // nursery words that died (they were never promoted).
                stats.words_reclaimed = immix.words_reclaimed + nursery_garbage;
                self.minor_since_major = 0;
                // The fullsweep promoted the nursery's live words into the mature region, so
                // the nursery's surviving occupancy is zero and the resize can only shrink it.
                self.nursery.collapse(&self.policy, 0)?;
                Ok(stats)
            }
            PromoteOutcome::WouldNotFit => self.grow_major(roots, needed, live.lines),
        }
    }
    /// The **growth fallback** for a fullsweep whose **in-place** reclaim could not take the
    /// nursery's promotion (`needed` lines) — either for want of total room
    /// (`free_lines() < needed`) or for want of a contiguous run inside one block.
    ///
    /// The in-place reclaim ([`ImmixSpace::collect`]) has already run when this is reached,
    /// and the nursery's mature-reference slots have already been rewritten to their final
    /// in-place addresses, so the ordering the major prescribes (reclaim mature in place →
    /// resolve the nursery's references → promote the nursery) is preserved: this only
    /// replaces the *promotion* target with a freshly-built mark-region space. The whole live
    /// set — the nursery's graph *and* the just-reclaimed mature generation — is copied into
    /// it by the ordinary promotion protocol of [`relocate_major`](Self::relocate_major), so
    /// the old region's garbage (including whatever the in-place reclaim could not return) is
    /// abandoned wholesale.
    ///
    /// Two targets answer two different shortfalls:
    ///
    /// * a **larger** region (the policy's `next_words` target) when the ceiling permits
    ///   growing; and
    /// * a **fresh region of the current capacity** when it does not — a fresh region packs
    ///   monotonically from its first line, so the run the fragmented in-place region could
    ///   not produce is exactly what it has. The live set simply has to fit the region's line
    ///   count (`live_lines <= line_count`, which is what the capacity being a whole number of
    ///   blocks makes exact).
    ///
    /// So a growable heap never turns a working program into a failing one, and a capped one
    /// still collects whenever *any* layout of the current region could hold the live set.
    /// Only a [`HeapPolicy::max_heap_words`] ceiling that forbids growth **and** a live set
    /// no fresh region of the current capacity can hold reports the typed, retryable heap
    /// exhaustion — and always **before any word is promoted**. The nursery is left
    /// un-forwarded throughout (nothing here writes to it), and on that error path the current
    /// mature region is left in place, so the already-reclaimed mature generation is still
    /// fully addressable from every reference.
    ///
    /// (Under the current selection rule the larger-region arm is defensive: a policy that
    /// *keeps* the mature capacity only does so when it already has room for the promotion
    /// plus the reserve. The fresh-region arm is the one a fragmented region reaches.)
    fn grow_major(
        &mut self,
        roots: &mut [Eterm],
        needed: usize,
        live_lines: usize,
    ) -> Result<GcStats> {
        let capacity = self.mature.capacity_words();
        // The target must cover both the promotion the reclaim could not fit (`needed`) and
        // the line need of the whole live set being relocated (`live_lines`), since a
        // line-granular space charges one line per object even for a two-word cell.
        let want = capacity
            .saturating_add(needed.saturating_mul(LINE_WORDS))
            .max(live_lines.saturating_mul(LINE_WORDS));
        // `next_words` applies the policy's floor and its `max_heap_words` ceiling, so a
        // target that is not strictly larger than the current capacity means the ceiling
        // (or the growth table's own ceiling) forbids growing at all.
        let target = self.policy.next_words(want, 0);
        if target > capacity {
            return self.relocate_major(roots, target, live_lines);
        }
        if live_lines <= self.mature.line_count() {
            // Growth is forbidden, but the *in-place* region's fragmentation is not the last
            // word: a fresh region of the same capacity is empty and packs monotonically, so
            // the promotion that found no run gets one. The capacity is a whole number of
            // blocks, so `line_count() * LINE_WORDS == capacity` and the target needs no
            // raising (and would be clamped back to the ceiling if it were raised).
            return self.relocate_major(roots, capacity, live_lines);
        }
        Err(heap_exhausted(needed.saturating_mul(LINE_WORDS)))
    }

    /// A fullsweep that also **relocates** the mature generation into a differently-sized
    /// mark-region space — the growth/shrink half of
    /// [`HeapPolicy::mature_after_fullsweep`].
    ///
    /// Every object reachable from `roots` is copied out of the nursery *and* the old
    /// mature region into `fresh` by the ordinary promotion protocol, so the resize is a
    /// plain copying collection and never touches the in-place reclaim (that is why
    /// [`last_immix_stats`](Self::last_immix_stats) is cleared). The old mature region is
    /// abandoned wholesale, so its garbage is reclaimed for free.
    ///
    /// `live_lines` is the **exact** line need of the graph being relocated
    /// ([`crate::live::Live::lines`]: the per-object `lines_for` sum, not
    /// `lines_for(total_words)`, which would round the total once and under-count the many
    /// one-line objects a mark-region space actually charges for). The destination is both
    /// raised to it and checked against it *before* any word is copied, so the promotion can
    /// never run out of lines halfway — an explicit
    /// [`max_heap_words`](HeapPolicy::max_heap_words) ceiling that forbids the raise is the
    /// typed heap exhaustion instead, and the from-space is never left half-forwarded.
    ///
    /// The promotion itself is [atomic](\`promote\`), so a fresh region whose *run* layout
    /// cannot place the graph is refused with the same typed exposure and **nothing
    /// installed**: the trial region is dropped, the from-space keeps every live word at its
    /// original address, and `mature()` is still the region the caller had.
    fn relocate_major(
        &mut self,
        roots: &mut [Eterm],
        target: usize,
        live_lines: usize,
    ) -> Result<GcStats> {
        let used = self.nursery.used_words();
        let before = self.mature.used_words().saturating_add(used);

        let alloc = self.mature.allocator().clone();
        // Raise the policy's word target to the copy's real line need when the former alone
        // does not cover it (a space of two-word cells needs one line each). A ceiling that
        // forbids the raise leaves the target as it is and the check below reports it.
        let want = target.max(live_lines.saturating_mul(LINE_WORDS));
        let target = if want > target {
            self.policy.next_words(want, 0)
        } else {
            target
        };
        let mut fresh = ImmixSpace::with_allocator(alloc, target)?;
        if fresh.free_lines() < live_lines {
            return Err(heap_exhausted(live_lines.saturating_mul(LINE_WORDS)));
        }
        // Copy the whole live set out of the nursery and the old mature region. The two
        // borrows (`root` via `from`, `fresh` via `to`) are disjoint, and the immutable
        // borrow of `self.mature` ends with the block below, before the field is replaced.
        let mut stats = {
            let from = PromoteFrom::new(&self.nursery, Some(&self.mature));
            match promote(&from, &mut fresh, roots)? {
                PromoteOutcome::Done(stats) => {
                    drop(from);
                    self.mature = fresh;
                    stats
                }
                // The fresh region has the lines but not a contiguous run for one of the
                // objects (first-fit packing can waste a block's tail): the trial region is
                // dropped — never installed — and the from-space and the current region are
                // exactly as they were, so this is the same typed exhaustion the pre-check
                // above raises.
                PromoteOutcome::WouldNotFit => {
                    return Err(heap_exhausted(live_lines.saturating_mul(LINE_WORDS)));
                }
            }
        };
        stats.words_reclaimed = before.saturating_sub(stats.words_copied);
        self.minor_since_major = 0;
        // No in-place reclaim ran, so there is nothing to report for it.
        self.last_immix = None;
        // The relocation copied every live word — the nursery's included — into the fresh
        // mature region, so the nursery's surviving occupancy is zero.
        self.nursery.collapse(&self.policy, 0)?;
        Ok(stats)
    }
}

impl HeapObservable for GenImmixCollector {
    /// Minors since the last fullsweep.
    fn minor_gcs(&self) -> u32 {
        self.minor_since_major
    }

    /// The nursery (the "heap" in the C sense) capacity, in words — the **summed** capacity of
    /// every nursery space, so a chained nursery reports its full size.
    fn heap_size_words(&self) -> usize {
        self.nursery.capacity_words()
    }

    /// The nursery plus the mark-region mature generation (the C's `heap_sz + old heap`),
    /// in words.
    fn total_heap_size_words(&self) -> usize {
        self.nursery.capacity_words() + self.mature.capacity_words()
    }

    fn heap_policy(&self) -> &HeapPolicy {
        &self.policy
    }
}
#[cfg(test)]
mod tests;

#[cfg(test)]
mod grow_tests;