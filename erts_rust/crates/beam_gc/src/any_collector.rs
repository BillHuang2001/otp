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

//! [`AnyCollector`] — the strategy-dispatching collector seam.
//!
//! A thin wrapper that lets a caller (and a test) drive the **same** corpus through either
//! mature-generation strategy behind one API: [`crate::Collector`] for
//! [`MatureKind::Copying`](crate::config::MatureKind::Copying) and [`GenImmixCollector`] for
//! [`MatureKind::Immix`](crate::config::MatureKind::Immix). [`AnyCollector::from_config`] is
//! the dispatcher, and it dispatches on the configuration's **resolved** choice: an
//! explicit pin, or the measurement-derived automatic rule of [`crate::selection`] (the
//! default). The remaining methods forward to whichever strategy was selected.
//!
//! This module owns no collection logic — it exists so the two strategies stay
//! interchangeable without either depending on the other.

use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::bounds::ScannedBounds;
use crate::collector::{Collection, CollectionKind, Collector};
use crate::config::{GcConfig, MatureKind};
use crate::heap_info::HeapObservable;
use crate::immix_collector::GenImmixCollector;
use crate::policy::HeapPolicy;
use crate::reachable::ReachableObject;
use crate::selection::MatureChoice;
use crate::space::HeapSpace;
use crate::stats::GcStats;
/// A collector whose mature-generation strategy is chosen at construction time.
///
/// Both variants share the [`Collection`]/[`crate::CollectionKind`] result type and the
/// young-nursery accessors, so a caller that only needs "collect and inspect the young
/// space" is strategy-agnostic.
pub enum AnyCollector {
    /// The copying mature space ([`crate::Collector`]).
    Copying(Collector),
    /// The mark-region mature space ([`GenImmixCollector`]).
    Immix(GenImmixCollector),
}

impl AnyCollector {
    /// Builds the collector `config` **resolves to**: [`GcConfig::mature`] picks
    /// [`MatureKind::Copying`] → [`Collector`], [`MatureKind::Immix`] →
    /// [`GenImmixCollector`]. That resolution is the caller's explicit pin when one was set
    /// and otherwise the automatic rule's verdict on the configured mature generation
    /// ([`crate::selection`]).
    pub fn from_config(config: &GcConfig) -> Result<AnyCollector> {
        match config.mature() {
            MatureKind::Copying => Ok(AnyCollector::Copying(Collector::new(
                config.policy().clone(),
            )?)),
            MatureKind::Immix => Ok(AnyCollector::Immix(GenImmixCollector::with_config(config)?)),
        }
    }

    /// Creates a collector for `policy` with the **automatic rule** deciding the mature
    /// strategy — the strategy-dispatching counterpart of [`Collector::new`] for a caller that
    /// owns only a heap policy (the host `beam_process::Process` does exactly that).
    ///
    /// Equivalent to `AnyCollector::from_config(&GcConfig::new(policy))`, so it resolves to
    /// the same strategy [`GcConfig::new`] does: the copying [`Collector`] for a policy below
    /// the break-even (the default policy included) and the [`GenImmixCollector`] at/above it
    /// ([`crate::selection`]).
    pub fn new(policy: HeapPolicy) -> Result<AnyCollector> {
        AnyCollector::from_config(&GcConfig::new(policy))
    }

    /// The `MatureKind` this collector was built for.
    pub fn kind(&self) -> MatureKind {
        match self {
            AnyCollector::Copying(_) => MatureKind::Copying,
            AnyCollector::Immix(_) => MatureKind::Immix,
        }
    }

    /// The resolved mature-generation choice this collector runs, for inspection: the
    /// strategy, whether it came from the automatic rule or an explicit override, and the
    /// sizes the decision used.
    ///
    /// The `source` is recovered by comparing the resolved strategy with the rule's verdict
    /// on the mature generation — see [`crate::selection::choice_for`]. Use
    /// [`GcConfig::mature_choice`] when the exact source matters.
    pub fn mature_choice(&self) -> MatureChoice {
        crate::selection::choice_for(self.kind(), self.policy().initial_mature_words)
    }
    /// The collector's heap-size policy.
    pub fn policy(&self) -> &HeapPolicy {
        match self {
            AnyCollector::Copying(c) => c.policy(),
            AnyCollector::Immix(c) => c.policy(),
        }
    }

    /// The young (nursery) space.
    pub fn young(&self) -> &HeapSpace {
        match self {
            AnyCollector::Copying(c) => c.young(),
            AnyCollector::Immix(c) => c.young(),
        }
    }

    /// The young (nursery) space, mutably — where the mutator allocates.
    pub fn young_mut(&mut self) -> &mut HeapSpace {
        match self {
            AnyCollector::Copying(c) => c.young_mut(),
            AnyCollector::Immix(c) => c.young_mut(),
        }
    }

    /// The mature generation's **capacity** in words — the strategy-agnostic form of
    /// `mature().capacity_words()`.
    ///
    /// The two mature generations are differently-typed ([`HeapSpace`] for the copying
    /// [`Collector`], [`ImmixSpace`](crate::immix::ImmixSpace) for the
    /// [`GenImmixCollector`]), so there is no common `&` reference to hand back; this method
    /// and its two siblings expose the word-count observables both types agree on, in the same
    /// unit (words). The mark-region space accounts in whole lines, so its figures are
    /// line-rounded (`used_lines * LINE_WORDS`): a caller reads them as "at least this many
    /// words", never as an exact per-object count.
    pub fn mature_capacity_words(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.mature().capacity_words(),
            AnyCollector::Immix(c) => c.mature().capacity_words(),
        }
    }

    /// The words the mature generation currently holds (whole lines in the mark-region
    /// strategy — see [`mature_capacity_words`](AnyCollector::mature_capacity_words)).
    pub fn mature_used_words(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.mature().used_words(),
            AnyCollector::Immix(c) => c.mature().used_words(),
        }
    }

    /// The words the mature generation can still hand out (whole lines in the mark-region
    /// strategy — see [`mature_capacity_words`](AnyCollector::mature_capacity_words)). Always
    /// `mature_capacity_words() - mature_used_words()`.
    pub fn mature_available_words(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.mature().available_words(),
            AnyCollector::Immix(c) => c.mature().available_words(),
        }
    }

    /// The mature generation's **base** address — the strategy-agnostic form of
    /// `mature().base()`.
    ///
    /// Both mature generations are one contiguous region, so this and
    /// [`mature_capacity_words`](AnyCollector::mature_capacity_words) name the whole of it:
    /// `base .. base + capacity_words` in term words.
    pub fn mature_base(&self) -> *mut Eterm {
        match self {
            AnyCollector::Copying(c) => c.mature().base(),
            AnyCollector::Immix(c) => c.mature().base(),
        }
    }

    /// Whether `ptr` addresses a word the mature generation owns — the strategy-agnostic
    /// form of `mature().contains(ptr)`, and the predicate a host answers its
    /// "does this term live in a region the collector scans?" question with (a term is only
    /// safe to collect over when it is an immediate, a literal, or a pointer into the
    /// process's own young or mature heap — the runtime's `root_is_unscanned` valve).
    ///
    /// Reuses each concrete space's own range test (their `contains`); **the two differ,
    /// deliberately**:
    ///
    /// * the copying [`Collector`]'s [`HeapSpace::contains`] covers the **live region**
    ///   `[base, top)` — the words the mutator or a promotion has handed out. The
    ///   not-yet-allocated tail of the space is *not* included.
    /// * the [`GenImmixCollector`]'s [`ImmixSpace`](crate::immix::ImmixSpace) test covers the
    ///   **whole region** `[base, base + capacity)`, **free lines included** — the mark-region
    ///   space is one contiguous block-aligned area whose holes are still addressable. It is
    ///   therefore **over-inclusive** relative to the copying one.
    ///
    /// The over-inclusion is harmless for the membership question this answers: a word the
    /// space will never hand out cannot be the target of a live reference (a live term only
    /// ever points at an *allocated* address), and a genuinely foreign container — a pooled
    /// BIF heap, the runtime's pointer-stable arena, a message fragment — lies outside every
    /// mature space's range entirely, so an address on a free line is never mistaken for one.
    /// A caller that needs the exact live fill of a copying space that precisely should read
    /// [`mature_used_words`](AnyCollector::mature_used_words) instead.
    pub fn mature_contains(&self, ptr: *const Eterm) -> bool {
        match self {
            AnyCollector::Copying(c) => c.mature().contains(ptr),
            AnyCollector::Immix(c) => c.mature().contains(ptr),
        }
    }

    /// The collector's accumulated statistics.
    pub fn stats(&self) -> GcStats {
        match self {
            AnyCollector::Copying(c) => c.stats(),
            AnyCollector::Immix(c) => c.stats(),
        }
    }

    /// The number of minor collections run since the last fullsweep (the C `GEN_GCS`),
    /// forwarded from whichever strategy was selected.
    pub fn minor_gcs(&self) -> u32 {
        match self {
            AnyCollector::Copying(c) => c.minor_gcs(),
            AnyCollector::Immix(c) => c.minor_gcs(),
        }
    }

    /// The `garbage_collection` observables of the selected strategy.
    pub fn heap_info(&self) -> crate::heap_info::HeapInfo {
        crate::heap_info::HeapInfo::of(self)
    }

    /// Whether the nursery can no longer satisfy `need_words` more words — the query a runtime
    /// makes at its heap-admission point before it decides to collect. Forwarded to whichever
    /// strategy is selected ([`Collector::should_collect`] /
    /// [`GenImmixCollector::should_collect`]).
    ///
    /// Both strategies answer this from the **shared** young nursery (an exact-fit allocation
    /// still fits), so the answer is strategy-independent — a caller that switches the mature
    /// generation gets the same admission decisions. The chain's
    /// [collection debt](Self::collection_debt) is deliberately **not** folded in; see that
    /// method for the opt-in seam a host consults alongside this one.
    pub fn should_collect(&self, need_words: usize) -> bool {
        match self {
            AnyCollector::Copying(c) => c.should_collect(need_words),
            AnyCollector::Immix(c) => c.should_collect(need_words),
        }
    }

    /// Whether the nursery chain carries a space a [growth](Self::grow_nursery) retired — the
    /// chain's **collection debt** — forwarded to whichever strategy is selected
    /// ([`Collector::collection_debt`] / [`GenImmixCollector::collection_debt`]), the same way
    /// [`should_collect`](Self::should_collect) is.
    ///
    /// A growth never collects and never moves a term, so only a
    /// [collection](Self::collect) reclaims a retired space (it collapses the chain). A `true`
    /// answer therefore means **a collection is required to reclaim the chain: a
    /// heap-admission door should treat it like a full nursery** and collect, even when the
    /// current space still has room for the request.
    ///
    /// This is an **opt-in** seam, not part of [`should_collect`](Self::should_collect): a
    /// caller that ignores it pays nothing for it. `beam_rt`'s heap-admission doors
    /// (`src/context/gc.rs::ensure_heap` / `ensure_heap_full`) must OR it into their
    /// `should_collect` decision once they unconditionally admit the collection — see the
    /// handoff in the crate `CONTEXT.md`.
    pub fn collection_debt(&self) -> bool {
        match self {
            AnyCollector::Copying(c) => c.collection_debt(),
            AnyCollector::Immix(c) => c.collection_debt(),
        }
    }
    /// Runs a collection on the selected strategy.
    pub fn collect(&mut self, roots: &mut [Eterm]) -> Result<Collection> {
        match self {
            AnyCollector::Copying(c) => c.collect(roots),
            AnyCollector::Immix(c) => c.collect(roots),
        }
    }

    /// Runs a collection of **exactly** `kind` on the selected strategy — the kind-pinned
    /// counterpart of [`collect`](Self::collect), forwarded to
    /// [`Collector::collect_kind`](crate::Collector::collect_kind) /
    /// [`GenImmixCollector::collect_kind`](crate::GenImmixCollector::collect_kind) the same way
    /// the two `collect_*` forcing methods below are.
    ///
    /// [`collect`](Self::collect) picks the kind by policy and **escalates** a would-not-fit
    /// minor to a major; this entry point never does. [`CollectionKind::Minor`] runs
    /// `collect_minor` and [`CollectionKind::Major`] runs `collect_major`, so on success the
    /// returned [`Collection::kind`] is the requested `kind`, and a minor whose promotion cannot
    /// fit is the typed heap-exhaustion error with nothing copied and no counter advanced.
    ///
    /// This is the seam `beam_process::Process::collect_kind` drives: a caller that asks for a
    /// young-only collection can never be handed a fullsweep, whichever mature strategy the
    /// automatic rule selected.
    ///
    /// # Errors
    ///
    /// The selected collection's own `BeamError`; a `Minor` failure is never masked by a major.
    pub fn collect_kind(
        &mut self,
        kind: CollectionKind,
        roots: &mut [Eterm],
    ) -> Result<Collection> {
        match self {
            AnyCollector::Copying(c) => c.collect_kind(kind, roots),
            AnyCollector::Immix(c) => c.collect_kind(kind, roots),
        }
    }

    /// Forces a **minor** collection on the selected strategy (escalation is bypassed).
    pub fn collect_minor(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        match self {
            AnyCollector::Copying(c) => c.collect_minor(roots),
            AnyCollector::Immix(c) => c.collect_minor(roots),
        }
    }

    /// Forces a **major** (fullsweep) collection on the selected strategy.
    pub fn collect_major(&mut self, roots: &mut [Eterm]) -> Result<GcStats> {
        match self {
            AnyCollector::Copying(c) => c.collect_major(roots),
            AnyCollector::Immix(c) => c.collect_major(roots),
        }
    }

    /// Grows the nursery **on demand** so that a `need_words`-word allocation can proceed,
    /// forwarded to whichever strategy is selected (both expose the identical seam):
    /// [`Collector::ensure_capacity`] / [`GenImmixCollector::ensure_capacity`].
    ///
    /// After it returns, `young().available_words() >= need_words`; if the policy's
    /// [`max_heap_words`](HeapPolicy::max_heap_words) makes the request unreachable it
    /// returns the typed heap-exhaustion error instead. This is the seam a runtime calls on
    /// heap exhaustion, and it is strategy-agnostic — the caller does not need to know which
    /// mature generation is in use.
    pub fn ensure_capacity(
        &mut self,
        need_words: usize,
        roots: &mut [Eterm],
    ) -> Result<Collection> {
        match self {
            AnyCollector::Copying(c) => c.ensure_capacity(need_words, roots),
            AnyCollector::Immix(c) => c.ensure_capacity(need_words, roots),
        }
    }

    /// Enlarges the young nursery so a `need_words`-word allocation can proceed, **without
    /// collecting and without moving any live term** — forwarded to whichever strategy is
    /// selected (both expose the identical seam): [`Collector::grow_nursery`] /
    /// [`GenImmixCollector::grow_nursery`].
    ///
    /// It is safe for a caller that holds no root set: the space it replaces is retained (the
    /// nursery becomes a chain of spaces), so every live word stays addressable at its original
    /// address. When the policy's [`max_heap_words`](HeapPolicy::max_heap_words) (or the growth
    /// table's ceiling) cannot reach the request the typed heap-exhaustion error is returned
    /// instead. This is strategy-agnostic — the caller does not need to know which mature
    /// generation is in use.
    ///
    /// [`max_heap_words`](HeapPolicy::max_heap_words) bounds the nursery chain's **summed**
    /// capacity ([`heap_size_words`](Self::heap_size_words)), not each individual space, so the
    /// fresh space may only claim the budget the chain has left:
    /// `heap_size_words() <= max_heap_words` after every successful growth, on both strategies.
    /// A retired space a previous growth left provably **empty** (`used_words() == 0`) is
    /// re-adopted instead of replaced, which consumes no budget at all.
    pub fn grow_nursery(&mut self, need_words: usize) -> Result<()> {
        match self {
            AnyCollector::Copying(c) => c.grow_nursery(need_words),
            AnyCollector::Immix(c) => c.grow_nursery(need_words),
        }
    }

    /// Whether `ptr` addresses a word of **any** nursery space (current or one a
    /// [growth](AnyCollector::grow_nursery) retained) — forwarded to whichever strategy is
    /// selected, so the answer is strategy-agnostic.
    pub fn young_contains(&self, ptr: *const Eterm) -> bool {
        match self {
            AnyCollector::Copying(c) => c.young_contains(ptr),
            AnyCollector::Immix(c) => c.young_contains(ptr),
        }
    }

    /// How many spaces the nursery chain holds (`1`, or more after a
    /// [growth](AnyCollector::grow_nursery) retained a space) — forwarded to whichever
    /// strategy is selected, so the answer is strategy-agnostic.
    pub fn young_space_count(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.young_space_count(),
            AnyCollector::Immix(c) => c.young_space_count(),
        }
    }

    /// The `O(1)` aggregate extent over **every** region the selected strategy scans — the whole
    /// nursery chain plus its mature generation — forwarded to whichever strategy is selected
    /// ([`Collector::scanned_bounds`] / [`GenImmixCollector::scanned_bounds`]), the same way
    /// [`collection_debt`](Self::collection_debt) is.
    ///
    /// It is the **fast-reject** filter a host's unscanned-root valve evaluates per reference
    /// word: [`may_contain`](ScannedBounds::may_contain)` == false` proves the address is in no
    /// scanned region, so [`young_contains`](Self::young_contains) and
    /// [`mature_contains`](Self::mature_contains) need not run. A `true` answer is **not** a
    /// containment claim (the interval spans the holes between spaces, each space's unallocated
    /// tail, and the mark-region free lines) and must fall through to them; see
    /// [`ScannedBounds`].
    ///
    /// `O(1)` and allocation-free on both strategies. The interval is built from each space's
    /// **reserved** extent, so it cannot go stale between allocations, and its
    /// [generation](ScannedBounds::generation) advances on every event that can change the
    /// geometry — a nursery space installed, retired or collapsed, or the mature generation
    /// replaced (a copying fullsweep, a relocating mark-region fullsweep). The two strategies
    /// report different *intervals* (their mature regions differ in size and place) but the same
    /// contract; each is a superset of the regions its own exact tests accept, so the reject
    /// direction is always sound for the selected strategy.
    pub fn scanned_bounds(&self) -> ScannedBounds {
        match self {
            AnyCollector::Copying(c) => c.scanned_bounds(),
            AnyCollector::Immix(c) => c.scanned_bounds(),
        }
    }
    /// Visits every **boxed object** reachable from `roots` across the **whole heap** — every
    /// space of the nursery chain plus the mature generation — forwarded to whichever strategy
    /// is selected, so the enumeration is strategy-agnostic:
    /// [`Collector::for_each_reachable_object`] /
    /// [`GenImmixCollector::for_each_reachable_object`].
    ///
    /// Both strategies answer over the same nursery chain and their own mature region, so the
    /// reachable-object set is identical — the enumeration reports *which* objects are
    /// reachable, never where in the mature generation they live. See
    /// [`Collector::for_each_reachable_object`] for the full contract (read-only, whole-heap,
    /// layout-agnostic, iterative and cycle-safe, each object once, cons cells traversed but not
    /// reported).
    pub fn for_each_reachable_object(
        &self,
        roots: &[Eterm],
        visit: impl FnMut(Eterm, *const Eterm),
    ) -> Result<()> {
        match self {
            AnyCollector::Copying(c) => c.for_each_reachable_object(roots, visit),
            AnyCollector::Immix(c) => c.for_each_reachable_object(roots, visit),
        }
    }

    /// The **safe** form of [`for_each_reachable_object`](Self::for_each_reachable_object):
    /// visits every **boxed object** reachable from `roots` across the **whole heap**, each
    /// object **exactly once**, passing a read-only [`ReachableObject`] view of it — forwarded
    /// to whichever strategy is selected, so the view enumeration is strategy-agnostic:
    /// [`Collector::for_each_reachable_object_view`] /
    /// [`GenImmixCollector::for_each_reachable_object_view`].
    ///
    /// Both strategies answer over the same nursery chain and their own mature region, so the
    /// reachable-object set is identical, and the caller needs **no `unsafe`**. See
    /// [`Collector::for_each_reachable_object`] for the full contract (read-only, whole-heap,
    /// layout-agnostic, iterative and cycle-safe, each object once, cons cells traversed but not
    /// reported).
    pub fn for_each_reachable_object_view(
        &self,
        roots: &[Eterm],
        visit: impl FnMut(ReachableObject<'_>),
    ) -> Result<()> {
        match self {
            AnyCollector::Copying(c) => c.for_each_reachable_object_view(roots, visit),
            AnyCollector::Immix(c) => c.for_each_reachable_object_view(roots, visit),
        }
    }
}

impl HeapObservable for AnyCollector {
    fn minor_gcs(&self) -> u32 {
        match self {
            AnyCollector::Copying(c) => c.minor_gcs(),
            AnyCollector::Immix(c) => c.minor_gcs(),
        }
    }

    fn heap_size_words(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.heap_size_words(),
            AnyCollector::Immix(c) => c.heap_size_words(),
        }
    }

    fn total_heap_size_words(&self) -> usize {
        match self {
            AnyCollector::Copying(c) => c.total_heap_size_words(),
            AnyCollector::Immix(c) => c.total_heap_size_words(),
        }
    }

    fn heap_policy(&self) -> &HeapPolicy {
        self.policy()
    }
}
#[cfg(test)]
mod tests {
    use beam_common::BeamError;
    use beam_term::prelude::{make_small, ptr_val, tuple_element, tuple_size, NIL};

    use super::*;

    #[test]
    fn default_config_selects_the_copying_path() {
        let any = AnyCollector::from_config(&GcConfig::default()).expect("collector");
        assert_eq!(any.kind(), MatureKind::Copying);
        assert!(matches!(any, AnyCollector::Copying(_)));
        assert_eq!(any.policy(), &HeapPolicy::default());
        // The choice is observable on the built collector, and it names the rule.
        let choice = any.mature_choice();
        assert_eq!(choice.kind, MatureKind::Copying);
        assert_eq!(choice.source, crate::selection::MatureSource::Automatic);
        assert_eq!(choice.mature_words, 233);
    }

    #[test]
    fn immix_config_selects_the_immix_path() {
        let config = GcConfig::immix(HeapPolicy::from_min_max(12, 0));
        let any = AnyCollector::from_config(&config).expect("collector");
        assert_eq!(any.kind(), MatureKind::Immix);
        assert!(matches!(any, AnyCollector::Immix(_)));
        assert_eq!(any.policy(), config.policy());
        // An explicit Immix pin under the break-even is reported as an override.
        assert!(any.mature_choice().is_override());
    }

    #[test]
    fn the_automatic_rule_selects_the_strategy_from_the_mature_size() {
        let big = HeapPolicy {
            initial_mature_words: crate::selection::IMMIX_BREAK_EVEN_WORDS,
            ..HeapPolicy::default()
        };
        let any = AnyCollector::from_config(&GcConfig::auto(big)).expect("collector");
        assert_eq!(any.kind(), MatureKind::Immix);
        assert!(matches!(any, AnyCollector::Immix(_)));
        assert_eq!(
            any.mature_choice().source,
            crate::selection::MatureSource::Automatic
        );
    }

    /// The two mature strategies over one policy — the dispatch seam `beam_process` holds.
    fn strategies(policy: HeapPolicy) -> [AnyCollector; 2] {
        [
            AnyCollector::from_config(&GcConfig::copying(policy.clone()))
                .expect("copying collector"),
            AnyCollector::from_config(&GcConfig::immix(policy)).expect("immix collector"),
        ]
    }

    /// [`AnyCollector::collect_kind`] pins the kind on **both** strategies: a
    /// `fullsweep_after: 0` policy makes [`AnyCollector::collect`] pick a major, while
    /// `collect_kind(Minor)` still runs the young-only collection and relocates the live term.
    #[test]
    fn collect_kind_pins_the_kind_on_both_strategies() {
        let policy = HeapPolicy {
            fullsweep_after: 0, // the cadence fires on every collection
            ..HeapPolicy::default()
        };
        for mut any in strategies(policy) {
            let strategy = any.kind();
            let root = any
                .young_mut()
                .heap_mut()
                .tuple(&[make_small(1), make_small(2)])
                .expect("the tuple fits the nursery");

            // `collect` (unchanged) selects the major the policy asks for.
            let mut chosen_roots = [root];
            let chosen = any.collect(&mut chosen_roots).expect("collect");
            assert_eq!(
                chosen.kind,
                CollectionKind::Major,
                "{strategy:?}: fullsweep_after = 0 makes the policy pick a major"
            );

            // `collect_kind(Minor)` pins the minor.
            let root = any
                .young_mut()
                .heap_mut()
                .tuple(&[make_small(1), make_small(2)])
                .expect("the tuple fits the nursery");
            let mut roots = [root];
            let pinned = any
                .collect_kind(CollectionKind::Minor, &mut roots)
                .expect("the pinned minor fits");

            assert_eq!(pinned.kind, CollectionKind::Minor, "{strategy:?}");
            assert_eq!(pinned.stats.words_copied, 3, "{strategy:?}");
            assert_eq!(any.minor_gcs(), 1, "{strategy:?}");
            assert_eq!(
                any.stats().minor_collections,
                1,
                "{strategy:?}: only the pinned collection was a minor"
            );
            assert_eq!(
                any.stats().major_collections,
                1,
                "{strategy:?}: the chosen major only — the pinned minor added none"
            );

            // The relocated root is a live, intact 2-tuple in the mature generation.
            let ptr = ptr_val(roots[0]);
            assert!(any.mature_contains(ptr), "{strategy:?}");
            assert!(!any.young_contains(ptr), "{strategy:?}");
            // SAFETY: the assertions above proved `roots[0]` addresses a live object in the
            // mature generation and it is the 2-tuple allocated above, so slots 0..2 are in
            // bounds.
            unsafe {
                assert_eq!(tuple_size(roots[0]), 2);
                assert_eq!(tuple_element(roots[0], 0), make_small(1));
                assert_eq!(tuple_element(roots[0], 1), make_small(2));
            }
        }
    }

    /// [`AnyCollector::collect_kind`]`(Minor)` on **either** strategy reports the typed heap
    /// exhaustion for a promotion the mature generation cannot receive — never the escalated
    /// fullsweep [`AnyCollector::collect`] would run — and changes **no** counter and **no**
    /// space geometry.
    #[test]
    fn collect_kind_minor_never_escalates_on_either_strategy() {
        // A 1 024-word nursery against a minimal mature generation: a whole nursery's promotion
        // cannot fit, so only a major's reclaim/growth can make room for it.
        let policy = HeapPolicy {
            initial_young_words: 1024,
            initial_mature_words: 1,
            ..HeapPolicy::default()
        };
        for mut any in strategies(policy) {
            let strategy = any.kind();
            let heap = any.young_mut().heap_mut();
            let mut live: Vec<Eterm> = Vec::new();
            for i in 0..500i64 {
                live.push(heap.cons(make_small(i), NIL).expect("cell"));
            }
            let mut roots = live;
            assert!(
                any.young().used_words() > any.mature_available_words(),
                "{strategy:?}: the promotion really cannot fit"
            );

            let stats_before = any.stats();
            let young_capacity = any.young().capacity_words();
            let mature_capacity = any.mature_capacity_words();
            let mature_used = any.mature_used_words();
            let spaces = any.young_space_count();

            let err = any
                .collect_kind(CollectionKind::Minor, &mut roots)
                .expect_err("the pinned minor must not fit");
            assert!(
                matches!(err, BeamError::Alloc(_)),
                "{strategy:?}: got {err:?}"
            );

            // No escalation, no copy, no counter and no geometry change.
            assert_eq!(
                any.stats(),
                stats_before,
                "{strategy:?}: no counter advanced"
            );
            assert_eq!(any.minor_gcs(), 0, "{strategy:?}");
            assert_eq!(any.mature_used_words(), mature_used, "{strategy:?}");
            assert_eq!(any.mature_capacity_words(), mature_capacity, "{strategy:?}");
            assert_eq!(any.young().capacity_words(), young_capacity, "{strategy:?}");
            assert_eq!(any.young_space_count(), spaces, "{strategy:?}");
            assert!(
                roots.iter().all(|root| any.young_contains(ptr_val(*root))),
                "{strategy:?}: every root is still in the nursery"
            );

            // The same collector DOES make room when the caller asks for the major.
            let escalated = any
                .collect_kind(CollectionKind::Major, &mut roots)
                .expect("the major makes room");
            assert_eq!(escalated.kind, CollectionKind::Major, "{strategy:?}");
            assert_eq!(any.stats().major_collections, 1, "{strategy:?}");
            assert!(
                roots.iter().all(|root| any.mature_contains(ptr_val(*root))),
                "{strategy:?}: the live set was relocated into the mature generation"
            );
        }
    }
}
