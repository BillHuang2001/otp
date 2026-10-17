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

//! **Containment-index exactness** for the nursery chain — the predicate a host's
//! *unscanned-root* valve evaluates **per reference word and per heap admission**.
//!
//! A growth-retained nursery is a chain of spaces, and the nursery-range test
//! ([`young_contains`](beam_gc::Collector::young_contains)) must answer for **every** address
//! of it. This file pins the answer itself, not its cost: whatever the chain's length, the
//! predicate must be **exactly** `spaces().any(|space| space.contains(ptr))` — the live region
//! `[base, top)` of any space of the chain, `top` exclusive, and nothing else. A spoofed
//! membership is a wrong result, not a slow one, so the reference predicate is recomputed here
//! from the regions the driver observed while it built the chain and compared over the whole
//! neighbourhood of every region (every word inside it, the words straddling both boundaries,
//! every hole between two spaces, and a set of foreign stack/arena addresses).
//!
//! Three properties are pinned:
//!
//! 1. **Exactness at the longest chain the heap policy allows.** Every growth is an *outgrowing*
//!    one (one word more than the current space can ever hold) under a policy whose
//!    `max_heap_words` is a **budget over the chain's summed capacity**, so a chain of the
//!    maximum reachable length is built **through the public growth seam alone** — the shape a
//!    runtime's un-rooted allocation path hits — and the chain ends in the typed
//!    heap-exhaustion refusal, not in an unbounded append.
//! 2. **Independence of the chain length.** The identical checker runs at every length from 1 up
//!    to that maximum, and it derives its expectations from the observed regions alone — a
//!    length-dependent answer cannot pass at any of them.
//! 3. **All three doors agree.** The strategy-dispatching
//!    [`AnyCollector`](beam_gc::AnyCollector) (both mature strategies) and the two concrete
//!    collectors [`Collector`](beam_gc::Collector) /
//!    [`GenImmixCollector`](beam_gc::GenImmixCollector) answer the same predicate — and all three
//!    obey the **summed-capacity ceiling**, refusing the request that would exceed it.
//!
//! The corpus is deterministic (no randomness) and no timing is ever asserted.

use core::ptr::addr_of;
use std::sync::LazyLock;

use beam_common::BeamError;
use beam_gc::prelude::{
    AnyCollector, Collector, GcConfig, GenImmixCollector, HeapObservable, HeapPolicy, HeapSpace,
    MatureKind,
};
use beam_term::prelude::{make_small, ptr_val, Eterm};

/// The capacity of the **first** space of the test chain, in term words.
const CAP: usize = 90;

/// The chain's **summed-capacity budget**, in term words — the policy's `max_heap_words`.
///
/// The chain can no longer be made arbitrarily long by pinning the ceiling to one space's size:
/// the ceiling bounds `capacity_words()`, the *sum* over the chain, so a growth may only claim the
/// budget the chain has left and an allocate-and-abandon loop ends in the typed refusal. The
/// longest reachable chain is therefore `O(log(budget))` links, which the growth ladder turns
/// into ≈10-14 spaces (see [`LONGEST`]).
const BUDGET: usize = 200_000;

/// One term word, in bytes — address arithmetic below is word-granular.
const WORD: usize = core::mem::size_of::<Eterm>();

/// A half-open live region `[lo, hi)` in byte addresses.
type Region = (usize, usize);

/// The budgeted policy every chain is built under: a [`CAP`]-word first space under a
/// [`BUDGET`]-word ceiling on the chain's summed capacity.
fn policy() -> HeapPolicy {
    HeapPolicy {
        max_heap_words: BUDGET,
        initial_young_words: CAP,
        initial_mature_words: CAP,
        ..HeapPolicy::default()
    }
}

/// The longest chain [`BUDGET`] allows **through the public growth seam**, discovered rather
/// than assumed: the ladder is the policy's growth table, so a table change moves this number
/// instead of breaking the corpus.
static LONGEST: LazyLock<usize> = LazyLock::new(|| {
    let mut c = Collector::new(policy()).expect("collector");
    let mut count = 0usize;
    loop {
        match grow_once(&mut c, count) {
            Ok(_) => count += 1,
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

/// An [`AnyCollector`] for `kind` under the budgeted policy.
fn any_for(kind: MatureKind) -> AnyCollector {
    AnyCollector::from_config(&GcConfig::new(policy()).with_mature(kind)).expect("collector")
}

/// The three ways to reach a collector's nursery.
///
/// [`AnyCollector`] and the two concrete collectors expose the same seams through different
/// types, so the driver below is written once against this door (the inherent methods have the
/// same names, so every forwarder is fully qualified).
trait NurseryDoor {
    /// The no-relocation growth seam.
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError>;
    /// The nursery-range test under test.
    fn young_contains(&self, ptr: *const Eterm) -> bool;
    /// The space the mutator allocates in.
    fn young(&self) -> &HeapSpace;
    /// The space the mutator allocates in, mutably.
    fn young_mut(&mut self) -> &mut HeapSpace;
    /// How many spaces the chain holds.
    fn young_space_count(&self) -> usize;
    /// The **summed** nursery capacity, in words.
    fn nursery_capacity_words(&self) -> usize;
}

impl NurseryDoor for AnyCollector {
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError> {
        AnyCollector::grow_nursery(self, need_words)
    }
    fn young_contains(&self, ptr: *const Eterm) -> bool {
        AnyCollector::young_contains(self, ptr)
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
    fn nursery_capacity_words(&self) -> usize {
        <AnyCollector as HeapObservable>::heap_size_words(self)
    }
}

impl NurseryDoor for Collector {
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError> {
        Collector::grow_nursery(self, need_words)
    }
    fn young_contains(&self, ptr: *const Eterm) -> bool {
        Collector::young_contains(self, ptr)
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
    fn nursery_capacity_words(&self) -> usize {
        <Collector as HeapObservable>::heap_size_words(self)
    }
}

impl NurseryDoor for GenImmixCollector {
    fn grow_nursery(&mut self, need_words: usize) -> Result<(), BeamError> {
        GenImmixCollector::grow_nursery(self, need_words)
    }
    fn young_contains(&self, ptr: *const Eterm) -> bool {
        GenImmixCollector::young_contains(self, ptr)
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
    fn nursery_capacity_words(&self) -> usize {
        <GenImmixCollector as HeapObservable>::heap_size_words(self)
    }
}

/// Grows `c` once, after handing the mutator `0..=3` words, reporting the retired space's live
/// region `[base, top)` **as it stood immediately before the growth that retired it** (the only
/// moment it can be observed from outside the crate: a retired space's `top` is frozen, but
/// nothing hands out its handle).
///
/// The request is one word more than the current space can ever hold, so the plain one-step rule
/// fires (no doubling) and the chain climbs the growth table one curated step per call — the
/// slowest way to spend the budget, hence the longest reachable chain.
fn grow_once<C: NurseryDoor>(c: &mut C, i: usize) -> Result<Region, BeamError> {
    let elems: Vec<Eterm> = (0..(i % 4)).map(|k| make_small(k as i64)).collect();
    c.young_mut()
        .heap_mut()
        .tuple(&elems)
        .unwrap_or_else(|e| panic!("round {i}: the tuple fits the fresh space: {e:?}"));
    let region = (c.young().base() as usize, c.young().top() as usize);
    assert!(
        region.1 > region.0,
        "round {i}: the mutator really handed out a word"
    );
    let need = c.young().capacity_words().saturating_add(1);
    assert!(
        c.young().available_words() < need,
        "round {i}: the request outgrows the space"
    );
    c.grow_nursery(need)?;
    Ok(region)
}

/// Builds `count` retired spaces through the public growth seam.
fn build_chain<C: NurseryDoor>(c: &mut C, count: usize) -> Vec<Region> {
    (0..count)
        .map(|i| grow_once(c, i).unwrap_or_else(|e| panic!("round {i}: the growth: {e:?}")))
        .collect()
}

/// The **reference predicate**: the live region `[base, top)` of any space of the chain, `top`
/// exclusive — recomputed here from the observed regions, exactly as
/// `spaces().any(|space| space.contains(ptr))` would answer.
fn reference(regions: &[Region], addr: usize) -> bool {
    regions.iter().any(|&(lo, hi)| lo <= addr && addr < hi)
}

/// The addresses a chain's regions are probed at: every word of every live region, the words
/// straddling both of its boundaries, and the words filling every hole between two spaces.
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
    // Every hole between two (base-sorted) spaces is outside both.
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

/// Builds a `count`-space chain on `c` and asserts the range test equals the reference predicate
/// at every probe — inside every region, on both of its boundaries, in every hole, and at a set
/// of foreign addresses a nursery must never claim.
fn check<C: NurseryDoor>(label: &str, mut c: C, count: usize) {
    // Two foreign regions, kept live for the whole comparison so their addresses cannot be
    // recycled by a later allocation: the real stack and a term arena outside every collector.
    let local: Eterm = make_small(3);
    let mut arena = beam_term::heap::Heap::new(256);
    let arena_term = arena
        .tuple(&[make_small(7), make_small(8)])
        .expect("the foreign arena's tuple");

    let retired = build_chain(&mut c, count);

    // A growth retires a space and never frees it, so the chain really is `count + 1` long — no
    // space was merged, dropped or resized — and its summed capacity stays inside the ceiling the
    // policy's budget sets over the whole chain.
    assert_eq!(
        c.young_space_count(),
        count + 1,
        "{label}: every growth retired (never freed) a space"
    );
    assert!(
        c.nursery_capacity_words() <= BUDGET,
        "{label}: the summed nursery capacity {} exceeds the {BUDGET}-word ceiling",
        c.nursery_capacity_words()
    );
    assert!(
        c.nursery_capacity_words() > count,
        "{label}: the summed nursery covers the whole chain"
    );

    let mut regions = retired;
    regions.push((c.young().base() as usize, c.young().top() as usize));

    let mut probes = region_probes(&regions);
    probes.push(0);
    probes.push(WORD);
    probes.push(addr_of!(local) as usize);
    probes.push(ptr_val(arena_term) as usize);
    probes.push(usize::MAX & !(WORD - 1));

    let mut inside = 0usize;
    for addr in probes {
        let want = reference(&regions, addr);
        assert_eq!(
            c.young_contains(addr as *const Eterm),
            want,
            "{label}: young_contains({addr:#x}) must equal the reference predicate \
             ({} spaces)",
            count + 1
        );
        inside += usize::from(want);
    }
    // The corpus is not vacuous: it lands inside the chain at least once per region.
    assert!(
        inside >= count,
        "{label}: the probes must reach inside the chain, got {inside}"
    );
}

// ---------------------------------------------------------------------------
// 1. Exactness at the longest chain the policy allows
// ---------------------------------------------------------------------------

#[test]
fn the_nursery_range_test_is_exact_for_the_longest_chain_the_budget_allows() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        check(&format!("AnyCollector/{kind:?}"), any_for(kind), *LONGEST);
    }
}

#[test]
fn the_concrete_collectors_answer_the_same_predicate_for_a_long_chain() {
    check(
        "Collector",
        Collector::new(policy()).expect("collector"),
        *LONGEST,
    );
    check(
        "GenImmixCollector",
        GenImmixCollector::from_policy(policy()).expect("collector"),
        *LONGEST,
    );
}

// ---------------------------------------------------------------------------
// 2. The answer does not depend on the chain's length
// ---------------------------------------------------------------------------

/// The same checker at every chain length — the expectations are derived from the observed
/// regions alone, so a length-dependent membership (a scan that stops early, an index that
/// misses a link) cannot satisfy all of them.
#[test]
fn the_predicate_is_the_same_one_at_every_chain_length() {
    let mut lengths = vec![1usize, 2, 3, 8];
    if *LONGEST > 8 {
        lengths.push(*LONGEST);
    }
    for count in lengths {
        for kind in [MatureKind::Copying, MatureKind::Immix] {
            check(&format!("AnyCollector/{kind:?}"), any_for(kind), count);
        }
    }
}

// ---------------------------------------------------------------------------
// 3. A foreign address is never claimed, however long the chain
// ---------------------------------------------------------------------------

/// A long chain must not start claiming addresses it does not own: the range test is
/// half-open on both sides (`base` inclusive, `top` exclusive), and the foreign stack/arena
/// addresses stay outside.
#[test]
fn a_long_chain_claims_only_its_own_live_regions() {
    let count = *LONGEST;
    let mut c = any_for(MatureKind::Copying);
    let retired = build_chain(&mut c, count);
    assert_eq!(c.young_space_count(), count + 1, "the chain is long");

    // Every retired region's first word is claimed, its exclusive end is not.
    for &(lo, hi) in &retired {
        assert!(c.young_contains(lo as *const Eterm), "{lo:#x} is the base");
        assert!(
            c.young_contains((hi - 1) as *const Eterm),
            "{hi:#x} is exclusive"
        );
        assert!(!c.young_contains(hi as *const Eterm));
        assert!(!c.young_contains((lo - 1) as *const Eterm));
    }

    // The current space is empty (nothing was allocated in it since the last growth), so even
    // its own base is outside — an empty live region claims nothing.
    let base = c.young().base();
    assert_eq!(c.young().top(), base, "the current space is empty");
    assert!(!c.young_contains(base as *const Eterm));

    // The whole run's lowest and highest addresses, one word outside, are outside too.
    let lowest = retired.iter().map(|&(lo, _)| lo).min().expect("a region");
    let highest = retired.iter().map(|&(_, hi)| hi).max().expect("a region");
    assert!(!c.young_contains((lowest - WORD) as *const Eterm));
    assert!(!c.young_contains(highest.saturating_add(WORD) as *const Eterm));
}

// ---------------------------------------------------------------------------
// 4. The summed-capacity ceiling, through every door
// ---------------------------------------------------------------------------

/// Grows `c` until the budget refuses, asserting the ceiling after **every** successful growth
/// and that the refusal is the typed heap exhaustion, with the chain untouched by it (a retry
/// refuses identically, retires nothing and consumes no capacity).
fn grow_until_capped<C: NurseryDoor>(label: &str, c: &mut C) {
    let mut growths = 0usize;
    loop {
        // Fill the current space and ask for one word more than it has left, so every iteration
        // is a real growth rather than a no-op.
        let elems: Vec<Eterm> = (0..(growths % 4)).map(|k| make_small(k as i64)).collect();
        c.young_mut()
            .heap_mut()
            .tuple(&elems)
            .unwrap_or_else(|e| panic!("{label} round {growths}: the tuple fits: {e:?}"));
        let need = c.young().available_words().saturating_add(1);
        match c.grow_nursery(need) {
            Ok(()) => {
                growths += 1;
                assert!(
                    c.nursery_capacity_words() <= BUDGET,
                    "{label} round {growths}: the summed nursery capacity {} exceeds the \
                     {BUDGET}-word ceiling",
                    c.nursery_capacity_words()
                );
                assert!(growths < 4096, "{label}: the growth ladder must terminate");
            }
            Err(err) => {
                assert_eq!(
                    err.as_atom(),
                    Some("out_of_memory"),
                    "{label}: the ceiling is the typed heap exhaustion"
                );
                let (spaces, capacity) = (c.young_space_count(), c.nursery_capacity_words());
                assert!(
                    capacity <= BUDGET,
                    "{label}: the chain never passed the ceiling"
                );
                let again = c.grow_nursery(c.young().available_words().saturating_add(1));
                assert_eq!(
                    again.expect_err("still capped").as_atom(),
                    Some("out_of_memory"),
                    "{label}: the retry is refused identically"
                );
                assert_eq!(
                    c.young_space_count(),
                    spaces,
                    "{label}: a refused growth retires nothing"
                );
                assert_eq!(
                    c.nursery_capacity_words(),
                    capacity,
                    "{label}: and consumes no capacity"
                );
                break;
            }
        }
    }
    assert!(
        growths >= 3,
        "{label}: the corpus must really climb the ladder, got {growths} growths"
    );
}

/// The ceiling is a property of the **chain**, so every door that grows one must obey it — the
/// strategy-dispatching [`AnyCollector`] (both mature strategies) and the two concrete
/// collectors alike. This is the seam-level counterpart of `beam_gc`'s
/// `Nursery::grow` unit test: the invariant is asserted through the public API a runtime uses.
#[test]
fn a_bounded_policy_caps_the_summed_nursery_capacity_through_every_door() {
    for kind in [MatureKind::Copying, MatureKind::Immix] {
        grow_until_capped(&format!("AnyCollector/{kind:?}"), &mut any_for(kind));
    }
    grow_until_capped(
        "Collector",
        &mut Collector::new(policy()).expect("collector"),
    );
    grow_until_capped(
        "GenImmixCollector",
        &mut GenImmixCollector::from_policy(policy()).expect("collector"),
    );
}