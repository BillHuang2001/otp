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

//! **The aggregate scanned-region bounds** — the `O(1)` fast-reject filter a host's
//! *unscanned-root* valve evaluates **per reference word and per heap admission**.
//!
//! A collector's scanned regions are a *set*: every space of the nursery chain (the current
//! space and every space a growth retired) plus the mature generation. Answering "is this
//! address in one of them?" exactly costs the chain's `O(log #spaces)` ordered index plus the
//! mature space's range test. [`ScannedBounds`] additionally reports their **covering
//! interval** — maintained in `O(1)` — so a caller can reject an address outside every region
//! with two comparisons.
//!
//! This file pins the contract of that filter, in the only direction it is allowed to be
//! load-bearing:
//!
//! 1. **The safety property.** For **every** probe address: if
//!    [`may_contain`](ScannedBounds::may_contain) is `false`, then both exact tests
//!    ([`young_contains`](beam_gc::Collector::young_contains) and
//!    [`mature_contains`](beam_gc::AnyCollector::mature_contains)) are `false` too. A false
//!    reject of a contained address would let a host treat a live reference as foreign (or a
//!    foreign pointer as live) — a memory-safety bug, so it fails the test. The converse is
//!    **never** asserted: the interval spans the holes between spaces, every space's
//!    unallocated tail and the mark-region free lines, so a `true` answer is not a containment
//!    claim. How many `true` answers were actually contained is *printed* instead (the
//!    over-approximation the geometry forces), so the filter's tightness is visible without
//!    being asserted.
//! 2. **The aggregate is exactly the covering of the reserved extents.** Every space's
//!    `[base, limit)` (the *reserved* extent, not the moving live fill) — the interval is
//!    `min(bases) .. max(limits)` over the whole chain and the mature generation, with no
//!    accumulated slack from earlier, since-dropped spaces.
//! 3. **The geometry epoch moves exactly when the geometry does.** A growth retires and
//!    installs a space (one event, one epoch); a growth that finds room moves nothing; a
//!    collection collapses the chain; a mature relocation replaces the region. The interval and
//!    the epoch are checked after each of those, for both mature strategies and for the
//!    strategy-dispatching [`AnyCollector`].
//!
//! Everything here is deterministic — geometry, counts and word arithmetic only. No timing is
//! ever asserted.

use core::ptr::addr_of;
use std::sync::LazyLock;

use beam_common::BeamError;
use beam_gc::prelude::{
    AnyCollector, Collector, GcConfig, GenImmixCollector, HeapPolicy, HeapSpace, MatureKind,
    ScannedBounds,
};
use beam_term::prelude::{make_small, ptr_val, Eterm};

/// The capacity of the **first** space of the budgeted test chain, in term words.
const CAP: usize = 90;

/// The chain's **summed-capacity budget**, in term words — the policy's `max_heap_words`.
///
/// A chain can no longer be made arbitrarily long by pinning the ceiling to one space's size:
/// the ceiling bounds `capacity_words()`, the *sum* over the chain, so a growth may only claim
/// the budget the chain has left and an allocate-and-abandon loop ends in the typed refusal. The
/// longest reachable chain is therefore `O(log(budget))` links — see [`LONGEST`].
const BUDGET: usize = 200_000;

/// One term word, in bytes — address arithmetic below is word-granular.
const WORD: usize = core::mem::size_of::<Eterm>();

/// A half-open address interval `[lo, hi)` in byte addresses.
type Region = (usize, usize);

/// The budgeted policy every chain is built under: a [`CAP`]-word first space under a
/// [`BUDGET`]-word ceiling over the chain's summed capacity.
fn budgeted() -> HeapPolicy {
    HeapPolicy {
        max_heap_words: BUDGET,
        initial_young_words: CAP,
        initial_mature_words: CAP,
        ..HeapPolicy::default()
    }
}

/// An **uncapped** policy: a growth climbs the curated growth table, so the chain's spaces have
/// differing capacities (and, because each round hands the mutator a different number of words,
/// differing occupancies).
fn uncapped() -> HeapPolicy {
    HeapPolicy::default()
}

/// An [`AnyCollector`] for `kind`.
fn any_for(kind: MatureKind) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(budgeted()).with_mature(kind)).expect("collector")
}

/// How the driver asks for the next growth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grow {
    /// One word more than the current space can ever hold: the plain one-step rule fires (never
    /// the doubling) and the chain climbs one curated step per call — the longest chain a
    /// budgeted policy can build, which is what [`LONGEST`] measures.
    Outgrowing,
    /// One word more than the current space has left, so an uncapped policy climbs its growth
    /// table and the chain mixes capacities.
    Filling,
}

/// The longest chain [`BUDGET`] allows through the growth seam, discovered rather than assumed:
/// the ladder is the policy's growth table, so a table change moves this number instead of
/// breaking the corpus.
static LONGEST: LazyLock<usize> = LazyLock::new(|| {
    let mut c = Collector::new(budgeted()).expect("collector");
    let mut count = 0usize;
    loop {
        match grow_once(&mut c, count, Grow::Outgrowing) {
            Ok(()) => count += 1,
            Err(err) => {
                assert_eq!(
                    err.as_atom(),
                    Some("out_of_memory"),
                    "the chain must end in the typed refusal, not another error"
                );
                break;
            }
        }
        assert!(count < 4096, "the growth ladder must terminate");
    }
    assert!(
        count >= 8,
        "the budget must yield a chain long enough to be adversarial, got {count}"
    );
    count
});

/// The reserved extent `[base, base + capacity)` of a space, in byte addresses — what the
/// aggregate is built from.
fn reserved_extent(space: &HeapSpace) -> Region {
    (space.base() as usize, space.limit() as usize)
}

/// The three ways to reach a collector's scanned regions.
///
/// [`AnyCollector`] and the two concrete collectors expose the same seams through different
/// types, so the checkers below are written once against this door (the inherent methods have
/// the same names, so every forwarder is fully qualified).
trait BoundsDoor {
    /// The no-relocation growth seam.
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError>;
    /// The exact nursery-chain range test.
    fn young_contains(&self, ptr: *const Eterm) -> bool;
    /// The exact mature-generation range test.
    fn mature_contains(&self, ptr: *const Eterm) -> bool;
    /// The space the mutator allocates in.
    fn young(&self) -> &HeapSpace;
    /// The space the mutator allocates in, mutably.
    fn young_mut(&mut self) -> &mut HeapSpace;
    /// How many spaces the chain holds.
    fn young_space_count(&self) -> usize;
    /// The mature generation's **reserved extent** `[base, base + capacity)`.
    fn mature_extent(&self) -> Region;
    /// The aggregate filter under test.
    fn scanned_bounds(&self) -> ScannedBounds;
}

impl BoundsDoor for AnyCollector {
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError> {
        AnyCollector::grow_nursery(self, need_words)
    }
    fn young_contains(&self, ptr: *const Eterm) -> bool {
        AnyCollector::young_contains(self, ptr)
    }
    fn mature_contains(&self, ptr: *const Eterm) -> bool {
        AnyCollector::mature_contains(self, ptr)
    }
    fn young(&self) -> &HeapSpace {
        AnyCollector::young(self)
    }
    fn young_mut(&mut self) -> &mut HeapSpace {
        AnyCollector::young_mut(self)
    }
    fn young_space_count(&self) -> usize {
        AnyCollector::young_space_count(self)
    }
    fn mature_extent(&self) -> Region {
        let base = AnyCollector::mature_base(self) as usize;
        (base, base + self.mature_capacity_words() * WORD)
    }
    fn scanned_bounds(&self) -> ScannedBounds {
        AnyCollector::scanned_bounds(self)
    }
}

impl BoundsDoor for Collector {
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError> {
        Collector::grow_nursery(self, need_words)
    }
    fn young_contains(&self, ptr: *const Eterm) -> bool {
        Collector::young_contains(self, ptr)
    }
    fn mature_contains(&self, ptr: *const Eterm) -> bool {
        self.mature().contains(ptr)
    }
    fn young(&self) -> &HeapSpace {
        Collector::young(self)
    }
    fn young_mut(&mut self) -> &mut HeapSpace {
        Collector::young_mut(self)
    }
    fn young_space_count(&self) -> usize {
        Collector::young_space_count(self)
    }
    fn mature_extent(&self) -> Region {
        reserved_extent(self.mature())
    }
    fn scanned_bounds(&self) -> ScannedBounds {
        Collector::scanned_bounds(self)
    }
}

impl BoundsDoor for GenImmixCollector {
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError> {
        GenImmixCollector::grow_nursery(self, need_words)
    }
    fn young_contains(&self, ptr: *const Eterm) -> bool {
        GenImmixCollector::young_contains(self, ptr)
    }
    fn mature_contains(&self, ptr: *const Eterm) -> bool {
        self.mature().contains(ptr)
    }
    fn young(&self) -> &HeapSpace {
        GenImmixCollector::young(self)
    }
    fn young_mut(&mut self) -> &mut HeapSpace {
        GenImmixCollector::young_mut(self)
    }
    fn young_space_count(&self) -> usize {
        GenImmixCollector::young_space_count(self)
    }
    fn mature_extent(&self) -> Region {
        let base = self.mature().base() as usize;
        (base, base + self.mature().capacity_words() * WORD)
    }
    fn scanned_bounds(&self) -> ScannedBounds {
        GenImmixCollector::scanned_bounds(self)
    }
}

/// The growth request `grow` implies for the current state of `c`.
fn grow_request<C: BoundsDoor>(c: &C, grow: Grow) -> usize {
    match grow {
        Grow::Outgrowing => c.young().capacity_words().saturating_add(1),
        Grow::Filling => c.young().available_words().saturating_add(1),
    }
}

/// Grows `c` once, after handing the mutator `0..=3` words — the shape a growth is called in (the
/// mutator has outgrown the space).
fn grow_once<C: BoundsDoor>(c: &mut C, i: usize, grow: Grow) -> Result<(), BeamError> {
    // Hand the mutator `0..=3` words, so the spaces' live occupancies differ — the aggregate is
    // built from the reserved extent, so this must not change it.
    let elems: Vec<Eterm> = (0..(i % 4)).map(|k| make_small(k as i64)).collect();
    c.young_mut()
        .heap_mut()
        .tuple(&elems)
        .unwrap_or_else(|e| panic!("round {i}: the tuple fits the fresh space: {e:?}"));
    let need = grow_request(c, grow);
    assert!(
        c.young().available_words() < need,
        "round {i}: the request really outgrows the space"
    );
    c.grow_nursery(need)
}

/// Builds `count` retired spaces, reporting the **reserved extent** of every space of the chain
/// (each as it stood while it was the current space — the only moment it can be observed from
/// outside the crate).
fn build_chain<C: BoundsDoor>(c: &mut C, count: usize, grow: Grow) -> Vec<Region> {
    let mut extents = vec![reserved_extent(c.young())];
    for i in 0..count {
        grow_once(c, i, grow).unwrap_or_else(|e| panic!("round {i}: the growth: {e:?}"));
        extents.push(reserved_extent(c.young()));
    }
    extents
}

/// The covering interval of `regions` — what the aggregate of exactly those regions is.
fn covering(regions: &[Region]) -> Region {
    (
        regions.iter().map(|&(lo, _)| lo).min().expect("a region"),
        regions.iter().map(|&(_, hi)| hi).max().expect("a region"),
    )
}

/// The addresses a region is probed at: every word of it, the words straddling both of its
/// boundaries, and the words filling every hole between two regions.
fn region_probes(regions: &[Region]) -> Vec<usize> {
    let mut probes = Vec::new();
    for &(lo, hi) in regions {
        probes.push(lo.saturating_sub(WORD));
        probes.push(lo.saturating_sub(1));
        let mut addr = lo;
        while addr < hi {
            probes.push(addr);
            addr += WORD;
        }
        probes.push(hi.saturating_sub(1));
        probes.push(hi);
        probes.push(hi.saturating_add(WORD));
    }
    // Every hole between two (lo-sorted) regions is inside the covering interval but in no
    // region — the corner where a "true" answer must **not** be read as containment.
    let mut sorted = regions.to_vec();
    sorted.sort_unstable();
    for pair in sorted.windows(2) {
        let (prev_hi, next_lo) = (pair[0].1, pair[1].0);
        if next_lo > prev_hi {
            let step = ((next_lo - prev_hi) / 5).max(WORD);
            let mut addr = prev_hi;
            while addr < next_lo {
                probes.push(addr);
                addr += step;
            }
            probes.push(next_lo - 1);
        }
    }
    probes
}

/// Builds a `count`-space chain on `c` and checks the aggregate against the exact predicate at
/// every probe of every region's neighbourhood, in every hole, and at a set of foreign
/// addresses.
///
/// The safety property is **asserted**; the over-approximation is only **measured** (printed) —
/// never asserted, because it is a property of the geometry (holes, unallocated tails, free
/// lines), not of the contract.
fn check<C: BoundsDoor>(label: &str, mut c: C, count: usize, grow: Grow) {
    // Two foreign addresses kept live for the whole comparison so their addresses cannot be
    // recycled: a stack word and a term arena outside every collector.
    let local: Eterm = make_small(3);
    let mut arena = beam_term::heap::Heap::new(256);
    let arena_term = arena
        .tuple(&[make_small(7), make_small(8)])
        .expect("the foreign arena's tuple");

    let extents = build_chain(&mut c, count, grow);
    assert_eq!(
        c.young_space_count(),
        count + 1,
        "{label}: every growth retired (never freed) a space"
    );

    // (2) The aggregate is **exactly** the covering of every space's reserved extent plus the
    // mature generation's — no slack from a space that has already been dropped.
    let mature = c.mature_extent();
    let mut all = extents.clone();
    all.push(mature);
    let expected = covering(&all);
    let bounds = c.scanned_bounds();
    assert_eq!(
        (bounds.base(), bounds.limit()),
        expected,
        "{label}: the aggregate must be exactly the covering of the reserved extents"
    );
    assert!(!bounds.is_empty(), "{label}: a live collector scans something");
    assert_eq!(
        bounds.span_words(),
        (expected.1 - expected.0) / WORD,
        "{label}: the span is the covering's"
    );

    // (1) The safety property, over the whole neighbourhood of every region and a set of
    // foreign addresses. A rejected address must be in **no** region the exact tests accept.
    let mut probes = region_probes(&extents);
    probes.extend(region_probes(&[mature]));
    probes.extend([
        0,
        WORD,
        addr_of!(local) as usize,
        ptr_val(arena_term) as usize,
        usize::MAX & !(WORD - 1),
    ]);

    let mut accepted = 0usize;
    let mut accepted_and_contained = 0usize;
    let mut contained = 0usize;
    let total_probes = probes.len();
    for addr in probes {
        let ptr = addr as *const Eterm;
        let exact = c.young_contains(ptr) || c.mature_contains(ptr);
        if exact {
            contained += 1;
        }
        if bounds.may_contain(addr) {
            accepted += 1;
            if exact {
                accepted_and_contained += 1;
            }
        } else {
            assert!(
                !exact,
                "{label}: {addr:#x} was rejected by the aggregate but the exact tests contain \
                 it ({} spaces) — a false reject is a memory-safety bug",
                count + 1
            );
        }
    }
    assert!(
        contained > 0,
        "{label}: the probe corpus must land inside the scanned regions"
    );
    // The over-approximation is *measured*, never asserted: it is a property of the geometry
    // (the holes between spaces, each space's unallocated tail, the mark-region free lines),
    // not of the contract.
    println!(
        "{label}: chain {} spaces, {total_probes} probes, {contained} contained; the aggregate \
         accepted {accepted} ({accepted_and_contained} contained, so {} over-approximated), \
         span {} words",
        count + 1,
        accepted - accepted_and_contained,
        bounds.span_words()
    );
}

// ---------------------------------------------------------------------------
// 1. The safety property at every chain length, both strategies, both policies
// ---------------------------------------------------------------------------

/// The long chain: the longest a budgeted policy allows — a growth that climbs one curated step
/// per call until the chain's summed capacity can no longer grow. The aggregate must stay exact
/// and must stay safe at that length, and the page also pins that the chain ends in the typed
/// refusal rather than an unbounded append.
#[test]
fn the_aggregate_is_safe_and_exact_for_the_longest_chain_the_budget_allows() {
    let count = *LONGEST;
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        check(
            &format!("AnyCollector/{kind:?}/budgeted"),
            any_for(kind),
            count,
            Grow::Outgrowing,
        );
    }
    check(
        "Collector/budgeted",
        Collector::new(budgeted()).expect("collector"),
        count,
        Grow::Outgrowing,
    );
    check(
        "GenImmixCollector/budgeted",
        GenImmixCollector::from_policy(budgeted()).expect("collector"),
        count,
        Grow::Outgrowing,
    );

    // The refusal that ends the chain is the typed heap exhaustion, and the chain it leaves
    // behind is still inside the ceiling.
    let mut c = Collector::new(budgeted()).expect("collector");
    build_chain(&mut c, count, Grow::Outgrowing);
    let err = grow_once(&mut c, count, Grow::Outgrowing).expect_err("the budget is spent");
    assert_eq!(err.as_atom(), Some("out_of_memory"));
    assert!(
        c.young_space_count() == count + 1,
        "a refused growth retires nothing"
    );
}

/// The same checker at every chain length: the expectations are derived from the observed
/// geometry alone, so a length-dependent answer cannot satisfy all of them.
#[test]
fn the_aggregate_is_the_same_predicate_at_every_chain_length() {
    let mut lengths = vec![1usize, 2, 3, 8];
    if *LONGEST > 8 {
        lengths.push(*LONGEST);
    }
    for count in lengths {
        for kind in [MatureKind::Copying, MatureKind::Immix] {
            check(
                &format!("AnyCollector/{kind:?}/budgeted/{count}"),
                any_for(kind),
                count,
                Grow::Outgrowing,
            );
        }
    }
}

/// Differing capacities and occupancies: an uncapped policy climbs the growth table, so every
/// space of the chain is a different size — and each space's extent must enter the aggregate.
#[test]
fn the_aggregate_covers_a_chain_of_differing_capacities() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = AnyCollector::from_config(&GcConfig::new(uncapped()).with_mature(kind))
            .expect("collector");
        let extents = build_chain(&mut c, 8, Grow::Filling);
        let capacities: Vec<usize> = extents.iter().map(|&(lo, hi)| (hi - lo) / WORD).collect();
        assert!(
            capacities.windows(2).all(|w| w[0] != w[1]),
            "{kind:?}: the chain really mixes capacities: {capacities:?}"
        );
        let mut all = extents.clone();
        all.push(c.mature_extent());
        let bounds = c.scanned_bounds();
        assert_eq!(
            (bounds.base(), bounds.limit()),
            covering(&all),
            "{kind:?}: the aggregate covers every differently-sized space"
        );
        // Safety at every probe.
        for addr in region_probes(&extents) {
            let ptr = addr as *const Eterm;
            if !bounds.may_contain(addr) {
                assert!(
                    !c.young_contains(ptr) && !c.mature_contains(ptr),
                    "{kind:?}: {addr:#x} rejected but contained"
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 2. The geometry epoch: it moves exactly when the geometry does
// ---------------------------------------------------------------------------

/// Across **growth / retire**, a **no-op growth**, and the **collapse** a collection performs:
/// the interval and the epoch move together, and only when the geometry does.
#[test]
fn growth_retire_and_collapse_move_the_bounds_and_the_epoch_together() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any_for(kind);

        // A fresh collector is epoch 0 with one nursery space.
        let fresh = c.scanned_bounds();
        assert_eq!(fresh.generation(), 0, "{kind:?}: a fresh collector is epoch 0");
        assert_eq!(
            (fresh.base(), fresh.limit()),
            covering(&[reserved_extent(c.young()), c.mature_extent()]),
            "{kind:?}: the fresh aggregate is the two spaces' covering"
        );

        // A growth that finds room retires nothing: neither the interval nor the epoch moves.
        let room = c.young().available_words();
        c.grow_nursery(room).expect("an exact fit is a no-op");
        assert_eq!(
            c.scanned_bounds(),
            fresh,
            "{kind:?}: a no-op growth must change nothing"
        );

        // Every real growth retires one space and installs another: the epoch advances by
        // exactly one and the interval widens to cover the new space while still covering the
        // old one.
        let mut prev = fresh;
        for i in 0..8usize {
            c.young_mut().heap_mut().tuple(&[]).expect("a tuple");
            let need = grow_request(&c, Grow::Outgrowing);
            c.grow_nursery(need)
                .unwrap_or_else(|e| panic!("{kind:?} round {i}: the growth: {e:?}"));
            let bounds = c.scanned_bounds();
            assert_eq!(
                bounds.generation(),
                prev.generation() + 1,
                "{kind:?} round {i}: one geometry event per growth"
            );
            assert!(
                bounds.base() <= prev.base() && bounds.limit() >= prev.limit(),
                "{kind:?} round {i}: a growth only widens the aggregate"
            );
            let young = reserved_extent(c.young());
            assert!(
                bounds.base() <= young.0 && bounds.limit() >= young.1,
                "{kind:?} round {i}: the installed space is covered"
            );
            prev = bounds;
        }
        assert_eq!(
            c.young_space_count(),
            9,
            "{kind:?}: eight growths retired eight spaces"
        );

        // A collection collapses the chain back to one space: the epoch advances and the
        // interval is re-seeded from what is left — the surviving nursery space and the (maybe
        // replaced) mature generation, never the retired neighbourhood.
        let before = c.scanned_bounds();
        let mut no_roots: [Eterm; 0] = [];
        c.collect(&mut no_roots).expect("the collection");
        assert_eq!(
            c.young_space_count(),
            1,
            "{kind:?}: the collection collapsed the chain"
        );
        let after = c.scanned_bounds();
        assert!(
            after.generation() > before.generation(),
            "{kind:?}: the collapse is a geometry event ({} -> {})",
            before.generation(),
            after.generation()
        );
        assert_eq!(
            (after.base(), after.limit()),
            covering(&[reserved_extent(c.young()), c.mature_extent()]),
            "{kind:?}: after the collapse the aggregate is exactly the live geometry"
        );
        // And the safety property survives the collapse, over the retired neighbourhood too.
        for addr in region_probes(&[(before.base(), before.limit())]) {
            let ptr = addr as *const Eterm;
            if !after.may_contain(addr) {
                assert!(
                    !c.young_contains(ptr) && !c.mature_contains(ptr),
                    "{kind:?}: {addr:#x} rejected after the collapse but contained"
                );
            }
        }
    }
}/// A mature **replacement** (a copying fullsweep's fresh space / a relocating mark-region
/// fullsweep's fresh region) must be visible: the aggregate follows the new region exactly and
/// the epoch advances.
#[test]
fn a_mature_replacement_moves_the_aggregate_and_the_epoch() {
    // The copying collector replaces its mature space on every fullsweep.
    let mut copying = Collector::new(budgeted()).expect("collector");
    build_chain(&mut copying, 3, Grow::Outgrowing);
    let before = copying.scanned_bounds();
    let old_mature = copying.mature_extent();
    copying.collect_major(&mut []).expect("the fullsweep");
    let after = copying.scanned_bounds();
    assert_ne!(
        copying.mature_extent(),
        old_mature,
        "a fullsweep installs a *different* mature space"
    );
    assert!(
        after.generation() > before.generation(),
        "the replacement advanced the epoch"
    );
    assert_eq!(
        (after.base(), after.limit()),
        covering(&[reserved_extent(copying.young()), copying.mature_extent()]),
        "the aggregate follows the replaced space"
    );
    for addr in region_probes(&[old_mature]) {
        let ptr = addr as *const Eterm;
        if !after.may_contain(addr) {
            assert!(
                !copying.young_contains(ptr) && !copying.mature_contains(ptr),
                "{addr:#x} rejected after the replacement but contained"
            );
        }
    }

    // The mark-region collector replaces its region on a *relocating* fullsweep. A policy that
    // shrinks the region after a fullsweep that kept almost nothing alive forces that path
    // (`HeapPolicy::mature_after_fullsweep`).
    let shrinking = HeapPolicy {
        initial_mature_words: 8 * 256,
        min_heap_words: 0,
        max_heap_words: 0,
        ..HeapPolicy::default()
    };
    let mut immix = GenImmixCollector::from_policy(shrinking).expect("collector");
    let before = immix.scanned_bounds();
    let old_mature = immix.mature_extent();
    immix.collect_major(&mut []).expect("the relocating fullsweep");
    let after = immix.scanned_bounds();
    assert_ne!(
        immix.mature_extent(),
        old_mature,
        "the fullsweep relocated the region into a smaller one"
    );
    assert!(
        after.generation() > before.generation(),
        "the relocation advanced the epoch"
    );
    assert_eq!(
        (after.base(), after.limit()),
        covering(&[reserved_extent(immix.young()), immix.mature_extent()]),
        "the aggregate follows the relocated region"
    );
    for addr in region_probes(&[old_mature]) {
        let ptr = addr as *const Eterm;
        if !after.may_contain(addr) {
            assert!(
                !immix.young_contains(ptr) && !immix.mature_contains(ptr),
                "{addr:#x} rejected after the relocation but contained"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 3. Dispatch, and the addresses outside every region
// ---------------------------------------------------------------------------

/// The strategy-dispatching [`AnyCollector`] reports exactly what its selected strategy
/// reports — before and after geometry changes, epoch included.
#[test]
fn any_collector_forwards_the_selected_strategys_bounds() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut any = any_for(kind);
        let check_forwarding = |any: &AnyCollector| match any {
            AnyCollector::Copying(c) => assert_eq!(
                AnyCollector::scanned_bounds(any),
                Collector::scanned_bounds(c),
                "{kind:?}: forwarding to the copying collector"
            ),
            AnyCollector::Immix(c) => assert_eq!(
                AnyCollector::scanned_bounds(any),
                GenImmixCollector::scanned_bounds(c),
                "{kind:?}: forwarding to the mark-region collector"
            ),
        };
        check_forwarding(&any);
        build_chain(&mut any, 4, Grow::Outgrowing);
        check_forwarding(&any);
        any.collect(&mut []).expect("the collection");
        check_forwarding(&any);
    }
}

/// The interval's neighbourhood, from outside: one byte below `base`, `limit` itself and
/// everything above are rejected — and the exact tests reject them too, so the rejection is
/// truthful rather than merely conservative about the two spaces' own interiors.
#[test]
fn addresses_outside_the_aggregate_are_rejected_by_the_exact_tests_too() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        let mut c = any_for(kind);
        build_chain(&mut c, 3, Grow::Outgrowing);
        let bounds = c.scanned_bounds();

        for addr in [
            bounds.base() - 1,
            bounds.base() - WORD,
            bounds.limit(),
            bounds.limit() + 1,
            bounds.limit() + WORD,
            usize::MAX,
        ] {
            let ptr = addr as *const Eterm;
            assert!(
                !bounds.may_contain(addr),
                "{kind:?}: {addr:#x} is outside the aggregate"
            );
            assert!(
                bounds.definitely_unscanned(ptr),
                "{kind:?}: the unscanned form agrees"
            );
            assert!(
                !c.young_contains(ptr) && !c.mature_contains(ptr),
                "{kind:?}: {addr:#x} is outside every scanned region"
            );
        }

        // A zero-length address and the lowest word are outside as well (no heap allocation can
        // sit there).
        for addr in [0usize, WORD] {
            assert!(!bounds.may_contain(addr), "{kind:?}: {addr:#x} is outside");
        }
    }
}
