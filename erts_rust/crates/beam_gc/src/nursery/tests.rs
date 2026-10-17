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

//! Unit tests for the nursery chain's invariants: the containment index mirrors the
//! retired spaces **exactly**, the range test it answers is exactly
//! `spaces().any(|space| space.contains(ptr))`, a growth neither collects, moves nor
//! frees anything, the growth rule takes the one-step size unless that would not enlarge
//! the space, the chain's **summed** capacity obeys `HeapPolicy::max_heap_words`, and a
//! provably empty retired space is re-adopted rather than replaced. The index is what keeps
//! the range test independent of the chain's length, so it is pinned over a chain far longer
//! than any hand-written case would build — installed directly by [`install_space`], because
//! the growth seam can no longer build one that long (see its doc).

use beam_alloc::registry::default_allocator;
use beam_common::BeamError;
use beam_term::prelude::{make_small, ptr_val};

use super::*;

/// The capacity of the **first** space of the test chain, in term words.
const CAP: usize = 90;

/// The chain's **summed-capacity budget**, in term words — `max_heap_words` for every policy
/// that has to grow a chain. A coupling of minutes-of-CPU is not needed: the growth ladder
/// climbs geometrically, so a few hundred thousand words already yields a chain of ≈10-12
/// spaces and then the typed refusal, which is what the growth seam may legitimately build.
const BUDGET: usize = 200_000;

/// Retired spaces the **directly installed** long chain holds. It is deliberately far longer
/// than any reachable growth produces (see [`install_space`]).
const RETIRED: usize = 1024;

/// A policy whose summed-capacity ceiling is [`BUDGET`] and whose first space is [`CAP`]
/// words — the bounded shape a growth chain is built under.
fn budgeted() -> HeapPolicy {
    HeapPolicy {
        max_heap_words: BUDGET,
        initial_young_words: CAP,
        ..HeapPolicy::default()
    }
}

/// The request that makes a growth install a **genuinely larger** space: one word more than
/// the current space can ever hold, so the plain one-step rule (never the doubling) fires and
/// the chain climbs the growth table one curated step per call. That is the slowest way to
/// spend the budget, so it is what gives a bounded chain the most links.
fn outgrowing(nursery: &Nursery) -> usize {
    nursery.current().capacity_words().saturating_add(1)
}

/// Grows `nursery` until the budget refuses, reporting every retired space's live region
/// `[base, top)` **as it stood immediately before the growth that retired it**, and the
/// refusal's error. Every growth allocates `1 + i % 4` words into the space it is about to
/// retire, so the regions' `top`s differ (the range test must read each one's own `top`).
fn grow_chain(nursery: &mut Nursery, policy: &HeapPolicy) -> (Vec<(usize, usize)>, BeamError) {
    let mut regions = Vec::new();
    loop {
        let i = regions.len();
        let elems: Vec<Eterm> = (0..(i % 4)).map(|k| make_small(k as i64)).collect();
        nursery
            .current_mut()
            .heap_mut()
            .tuple(&elems)
            .unwrap_or_else(|e| panic!("round {i}: the tuple fits the space: {e:?}"));
        let need = outgrowing(nursery);
        match nursery.grow(policy, need) {
            Ok(()) => regions.push((
                nursery.retired.last().expect("a retired space").base() as usize,
                nursery.retired.last().expect("a retired space").top() as usize,
            )),
            Err(err) => return (regions, err),
        }
    }
}

/// Installs a fresh `capacity`-word space as the current space and retires the old one,
/// reproducing exactly what [`Nursery::grow`]'s fresh-allocation path does to the chain
/// (retire into the index, widen the aggregate, advance the geometry epoch) — and hands the
/// mutator `elems`, so the retired space's live region is as varied as a real chain's.
///
/// The growth seam can no longer build a chain this long: the chain's **summed** capacity is
/// capped by `HeapPolicy::max_heap_words` and the ladder climbs geometrically, so the longest
/// chain a real growth produces is `O(log(budget))`. This helper exists to pin the *query*
/// paths ([`Nursery::contains`], [`Nursery::bounds`]) at a length no reachable growth
/// produces, which is what makes a per-space **scan** — or a length-dependent index —
/// unmistakable.
fn install_space(nursery: &mut Nursery, capacity: usize, elems: &[Eterm]) -> (usize, usize) {
    nursery
        .current_mut()
        .heap_mut()
        .tuple(elems)
        .expect("the tuple fits the space");
    let region = (
        nursery.current().base() as usize,
        nursery.current().top() as usize,
    );
    let fresh = HeapSpace::with_allocator(nursery.current().allocator().clone(), capacity)
        .expect("the fresh space");
    nursery.install(fresh);
    nursery.widen_extent();
    region
}

/// The directly installed long chain: [`RETIRED`] retired spaces of [`CAP`] words each,
/// reporting every retired space's live region.
fn build_chain(nursery: &mut Nursery) -> Vec<(usize, usize)> {
    let mut regions = Vec::with_capacity(RETIRED);
    for i in 0..RETIRED {
        let elems: Vec<Eterm> = (0..(i % 4)).map(|k| make_small(k as i64)).collect();
        regions.push(install_space(nursery, CAP, &elems));
    }
    regions
}
/// Probes the whole neighbourhood of every region of `regions`: every word of its live
/// region plus the words straddling both of its boundaries.
fn region_probes(regions: &[(usize, usize)]) -> Vec<usize> {
    let word = core::mem::size_of::<Eterm>();
    let mut probes = Vec::new();
    for &(lo, hi) in regions {
        probes.push(lo);
        let mut addr = lo;
        while addr < hi {
            probes.push(addr);
            addr += word;
        }
        probes.push(hi.saturating_sub(1));
        probes.push(hi);
        probes.push(lo.saturating_sub(1));
        probes.push(lo.saturating_sub(word));
        probes.push(hi.saturating_add(word));
    }
    probes
}

#[test]
fn the_containment_index_mirrors_the_chain_and_answers_exactly() {
    let mut nursery = Nursery::new(default_allocator().clone(), CAP).expect("nursery");
    let retired = build_chain(&mut nursery);

    // A growth retires a space, never frees it: the chain holds every superseded space.
    assert_eq!(nursery.space_count(), RETIRED + 1);
    assert_eq!(nursery.retired.len(), RETIRED);

    // The index mirrors the chain **exactly** — one entry per retired space, naming that
    // space's own frozen live region. This is the invariant the range test's exactness
    // rests on: with the bases distinct (every space is its own allocation) the indexed
    // intervals are disjoint, so the greatest base at or below an address is its only
    // candidate, and a missing or stale entry would be a wrong answer.
    assert_eq!(nursery.retired_regions.len(), nursery.retired.len());
    for space in &nursery.retired {
        assert_eq!(
            nursery.retired_regions.get(&(space.base() as usize)),
            Some(&(space.top() as usize)),
            "the index names every retired space's live region"
        );
    }

    // The range test is exactly the reference predicate over `spaces()` — for the retired
    // regions *and* for the current space, whose live region the index deliberately does not
    // hold (it is the only one whose `top` moves).
    let mut regions = retired;
    regions.push((
        nursery.current().base() as usize,
        nursery.current().top() as usize,
    ));
    let probes = region_probes(&regions);
    let mut inside = 0usize;
    for &addr in &probes {
        let want = nursery
            .spaces()
            .any(|space| space.contains(addr as *const Eterm));
        assert_eq!(
            nursery.contains(addr as *const Eterm),
            want,
            "contains({addr:#x}) must equal the scan over `spaces()`"
        );
        inside += usize::from(want);
    }
    // The corpus is not vacuous: it really probes inside the chain (≥ one word per region).
    assert!(
        inside >= RETIRED,
        "the probes must reach inside the chain, got {inside}"
    );

    // ... and a below-the-first and an above-the-last address are outside every space.
    let lowest = nursery
        .spaces()
        .map(|s| s.base() as usize)
        .min()
        .expect("a space");
    let highest = nursery
        .spaces()
        .map(|s| s.top() as usize)
        .max()
        .expect("a space");
    assert!(!nursery.contains(lowest.saturating_sub(8) as *const Eterm));
    assert!(!nursery.contains(highest.saturating_add(8) as *const Eterm));
}

#[test]
fn a_growth_retires_but_never_collects_moves_or_frees() {
    let mut nursery = Nursery::new(default_allocator().clone(), CAP).expect("nursery");
    let policy = budgeted();

    // A live term in the space the growth will retire, at a known address.
    let live = nursery
        .current_mut()
        .heap_mut()
        .tuple(&[make_small(11), make_small(12)])
        .expect("tuple");
    let base = nursery.current().base();
    let used = nursery.current().used_words();

    let need = CAP + 1;
    nursery.grow(&policy, need).expect("the growth");

    // Nothing was freed (the superseded allocation is still the chain's, untouched), nothing
    // moved (the term is at its original address and reads back inside the chain), and the
    // chain's summed occupancy is unchanged.
    assert_eq!(nursery.space_count(), 2);
    assert!(nursery.contains(ptr_val(live) as *const Eterm));
    assert_eq!(nursery.retired.len(), 1);
    assert_eq!(nursery.retired[0].base(), base);
    assert_eq!(nursery.retired[0].used_words(), used);
    assert_eq!(nursery.used_words(), used, "the live words are still there");
    assert!(
        nursery.retains_old_spaces(),
        "the retention is the chain's collection debt"
    );

    // The request outgrew the space, so the growth installed the smallest curated space that
    // covers it — and the chain's summed capacity is the two spaces', still within the
    // ceiling ([`growth_stops_at_the_summed_capacity_ceiling`] pins the other side of that).
    assert_eq!(nursery.current().capacity_words(), policy.next_words(need, 0));
    assert!(nursery.current().capacity_words() > CAP);
    assert_eq!(nursery.capacity_words(), CAP + policy.next_words(need, 0));
    assert!(nursery.capacity_words() <= policy.max_heap_words);
}

/// Fills a space so that at most a few words are still available — the shape a growth is
/// called in (the mutator has outgrown the space). Leaves a margin, so the tuple always fits.
fn fill(space: &mut HeapSpace) {
    let elems: Vec<Eterm> = (0..space.available_words().saturating_sub(4))
        .map(|i| make_small(i as i64))
        .collect();
    space
        .heap_mut()
        .tuple(&elems)
        .expect("the filler fits the space");
}

#[test]
fn a_growth_takes_the_one_step_size_unless_it_would_not_enlarge_the_space() {
    let policy = HeapPolicy::default();

    // (a) A 233-word space with a 234-word request: the plain one-step rule already hands
    // back a **larger** space (`next_words(234, 0) = 376`), so the doubling does not fire —
    // the request gets exactly the smallest curated size that covers it. This is the shape a
    // request-driven growth has, and the geometry the sibling crates pin
    // (`beam_process`'s `next_heap_size(payload_words, 0)` expectations).
    let mut nursery = Nursery::new(default_allocator().clone(), 233).expect("nursery");
    assert_eq!(nursery.current().capacity_words(), 233);
    assert!(
        nursery.current().available_words() < 234,
        "the 234-word request outgrows the space"
    );
    nursery.grow(&policy, 234).expect("the one-step growth");
    assert_eq!(nursery.current().capacity_words(), 376);
    assert_eq!(nursery.current().capacity_words(), policy.next_words(234, 0));

    // (b) The same nursery, now 376 words, and a request the space **already covers**: the
    // one-step rule would hand back the very space it replaces, so the rule takes at least a
    // doubling — `next_words(2 * 376, 0) = 987`.
    fill(nursery.current_mut());
    assert!(
        nursery.current().available_words() < 376,
        "the space is nearly full again"
    );
    nursery.grow(&policy, 376).expect("the doubling growth");
    assert_eq!(nursery.current().capacity_words(), 987);
    assert_eq!(nursery.current().capacity_words(), policy.next_words(752, 0));
    assert!(
        nursery.current().capacity_words() >= 2 * 376,
        "the doubling branch really doubles"
    );

    // (c) A growth loop repeating a **constant** request can therefore never retire one
    // ≈same-size space per call: each call either installs a genuinely larger request size
    // or doubles, so the capacity at least doubles every **two** calls and the chain stays
    // `O(log(heap))` long. A `max_heap_words` ceiling no longer produces the pathological
    // same-size chain at all — it bounds the chain's **summed** capacity, so once the budget
    // cannot cover one more space the growth is refused
    // (`growth_stops_at_the_summed_capacity_ceiling`) and the long-chain corpus is installed
    // directly (`install_space`).
    const REQUEST: usize = 234;
    const THRESHOLD: usize = 1 << 16;
    let mut looped = Nursery::new(default_allocator().clone(), 233).expect("nursery");
    let mut caps = vec![looped.current().capacity_words()];
    while *caps.last().expect("a capacity") < THRESHOLD {
        fill(looped.current_mut());
        looped.grow(&policy, REQUEST).expect("the loop's growth");
        caps.push(looped.current().capacity_words());
    }
    // Every space is strictly larger than the one it replaced ...
    assert!(
        caps.windows(2).all(|w| w[0] < w[1]),
        "every growth enlarged the space: {caps:?}"
    );
    // ... and the ladder doubles at least every two calls, so its length is logarithmic in
    // the threshold (from 233, ~2 * log2(65536 / 233) ≈ 18 calls).
    for i in 2..caps.len() {
        assert!(
            caps[i] >= 2 * caps[i - 2],
            "the capacity must at least double every two calls: {caps:?}"
        );
    }
    assert!(
        caps.len() <= 24,
        "the chain must stay logarithmic, got {} spaces: {caps:?}",
        caps.len()
    );

    // (d) A request the current space already has room for is still a **no-op**: nothing is
    // retired and the capacity is untouched.
    let spaces_before = looped.space_count();
    let cap_before = looped.current().capacity_words();
    let fit = looped.current().available_words();
    looped.grow(&policy, fit).expect("an exact fit is a no-op");
    assert_eq!(looped.space_count(), spaces_before, "nothing retired");
    assert_eq!(looped.current().capacity_words(), cap_before);
}

/// One term word, in bytes.
const WORD: usize = core::mem::size_of::<Eterm>();

/// The **reserved** extent `[base, base + capacity)` of `space`, in byte addresses — what
/// the aggregate cache is built from, as opposed to the moving live fill `[base, top)`.
fn reserved_extent(space: &HeapSpace) -> (usize, usize) {
    (
        space.base() as usize,
        space.base() as usize + space.capacity_words() * WORD,
    )
}

/// The covering interval of `regions` — what the aggregate of exactly those spaces is.
fn covering(regions: &[(usize, usize)]) -> (usize, usize) {
    (
        regions.iter().map(|&(lo, _)| lo).min().expect("a region"),
        regions.iter().map(|&(_, hi)| hi).max().expect("a region"),
    )
}

#[test]
fn the_aggregate_extent_covers_every_space_and_tracks_the_geometry_epoch() {
    let policy = budgeted();
    let mut nursery = Nursery::new(default_allocator().clone(), CAP).expect("nursery");

    // A fresh nursery is one space: the aggregate is exactly its reserved extent, at epoch 0.
    let mut extents = vec![reserved_extent(nursery.current())];
    let fresh = nursery.bounds();
    assert_eq!((fresh.base(), fresh.limit()), covering(&extents));
    assert_eq!(fresh.generation(), 0, "a fresh chain is epoch 0");
    assert!(!fresh.is_empty(), "a live space has a non-empty extent");

    // A growth that finds room retires nothing, so the geometry does not change: neither the
    // interval nor the epoch may move.
    let room = nursery.current().available_words();
    nursery.grow(&policy, room).expect("an exact fit is a no-op");
    assert_eq!(
        nursery.bounds(),
        fresh,
        "a no-op growth must not change the aggregate or its epoch"
    );

    // Every real growth widens the aggregate to cover the space it installs (the retired one
    // was already covered — it was the current space when it entered), advances the epoch by
    // exactly one, and leaves the interval the covering of **every** space's extent. The
    // chain climbs one curated step per call, so the budget yields ≈10-14 spaces before it
    // refuses — a real, growth-built chain, which is what pins the *insertion* path of the
    // aggregate (the much longer chain below pins the query path).
    let mut growths = 0usize;
    loop {
        nursery
            .current_mut()
            .heap_mut()
            .tuple(&[])
            .expect("an empty tuple fits every space");
        let need = outgrowing(&nursery);
        let i = growths;
        let before = nursery.bounds();
        match nursery.grow(&policy, need) {
            Ok(()) => {
                growths += 1;
                extents.push(reserved_extent(nursery.current()));
                let bounds = nursery.bounds();
                assert_eq!(
                    (bounds.base(), bounds.limit()),
                    covering(&extents),
                    "round {i}: the aggregate is the covering of every space's extent"
                );
                assert_eq!(
                    bounds.generation(),
                    i as u64 + 1,
                    "round {i}: one geometry event per growth"
                );
                assert!(
                    nursery.capacity_words() <= policy.max_heap_words,
                    "round {i}: the summed capacity stays within the ceiling"
                );
            }
            Err(err) => {
                // The chain has spent the budget: the refusal is the typed error, and a
                // refused growth moves neither the interval nor the epoch nor the chain.
                assert_eq!(err.as_atom(), Some("out_of_memory"));
                assert_eq!(
                    nursery.bounds(),
                    before,
                    "a refused growth is not a geometry event"
                );
                break;
            }
        }
    }
    assert!(
        growths >= 8,
        "the budget must yield a real chain, got {growths} growths"
    );

    // The safety property, over the whole neighbourhood of every space's extent: a rejected
    // address is in **no** space of the chain. (The converse is deliberately not asserted —
    // the aggregate spans the holes between spaces and each space's unallocated tail.)
    let mut probes = region_probes(&extents);
    probes.extend([0, WORD, usize::MAX & !(WORD - 1)]);
    let long = nursery.bounds();
    let mut accepted = 0usize;
    for addr in probes {
        if long.may_contain(addr) {
            accepted += 1;
        } else {
            assert!(
                !nursery.contains(addr as *const Eterm),
                "{addr:#x} was rejected by the aggregate but the chain contains it \
                 ({} spaces)",
                nursery.space_count()
            );
        }
    }
    assert!(
        accepted > nursery.space_count(),
        "the probe corpus really reaches inside the chain, got {accepted}"
    );

    // The collapse re-seeds the aggregate from the single remaining space — a stale
    // widening would keep claiming the retired spaces' neighbourhood — and advances the
    // epoch. The safety property holds again over the same neighbourhood.
    nursery.collapse(&policy, 0).expect("the collapse");
    assert_eq!(nursery.space_count(), 1, "the chain is one space again");
    let collapsed = nursery.bounds();
    assert_eq!(
        (collapsed.base(), collapsed.limit()),
        reserved_extent(nursery.current()),
        "the aggregate is exactly the surviving space's extent"
    );
    assert_eq!(
        collapsed.generation(),
        long.generation() + 1,
        "the collapse is one geometry event"
    );
    for addr in region_probes(&extents) {
        if !collapsed.may_contain(addr) {
            assert!(
                !nursery.contains(addr as *const Eterm),
                "{addr:#x} was rejected after the collapse but is still contained"
            );
        }
    }
}

/// The aggregate and the range test over a chain far longer than the growth seam can now
/// build: [`RETIRED`] directly installed spaces. The interval must stay **exactly** the
/// covering of every space's reserved extent (no per-space slack, no accumulated widening),
/// the epoch must count every geometry event, and a rejected address must be in no space of
/// the chain — over the whole neighbourhood of every space, before and after the collapse.
#[test]
fn the_aggregate_extent_stays_exact_over_a_chain_no_reachable_growth_produces() {
    let policy = budgeted();
    let mut nursery = Nursery::new(default_allocator().clone(), CAP).expect("nursery");
    assert_eq!(nursery.bounds().generation(), 0, "a fresh chain is epoch 0");

    let retired = build_chain(&mut nursery);
    assert_eq!(nursery.space_count(), RETIRED + 1);
    assert_eq!(
        nursery.bounds().generation(),
        RETIRED as u64,
        "one geometry event per installed space"
    );

    // Every space of the chain — retired first, then the current one — contributes its own
    // reserved extent to the covering.
    let extents: Vec<(usize, usize)> = nursery.spaces().map(reserved_extent).collect();
    assert_eq!(extents.len(), RETIRED + 1);

    let bounds = nursery.bounds();
    assert_eq!(
        (bounds.base(), bounds.limit()),
        covering(&extents),
        "the aggregate is exactly the covering of every space's extent"
    );

    // The safety property over the whole neighbourhood, plus far-outside addresses.
    let mut probes = region_probes(&retired);
    probes.extend([0, WORD, usize::MAX & !(WORD - 1)]);
    let mut accepted = 0usize;
    for addr in probes {
        if bounds.may_contain(addr) {
            accepted += 1;
        } else {
            assert!(
                !nursery.contains(addr as *const Eterm),
                "{addr:#x} was rejected by the aggregate but the chain contains it \
                 ({} spaces)",
                nursery.space_count()
            );
        }
    }
    assert!(
        accepted > nursery.space_count(),
        "the probe corpus really reaches inside the chain, got {accepted}"
    );

    // The collapse re-seeds the aggregate from the surviving space and keeps the safety
    // property over the retired neighbourhood.
    nursery.collapse(&policy, 0).expect("the collapse");
    assert_eq!(nursery.space_count(), 1, "the chain is one space again");
    let collapsed = nursery.bounds();
    assert_eq!(
        (collapsed.base(), collapsed.limit()),
        reserved_extent(nursery.current()),
        "the aggregate is exactly the surviving space's extent"
    );
    assert_eq!(
        collapsed.generation(),
        bounds.generation() + 1,
        "the collapse is one geometry event"
    );
    for addr in region_probes(&retired) {
        if !collapsed.may_contain(addr) {
            assert!(
                !nursery.contains(addr as *const Eterm),
                "{addr:#x} was rejected after the collapse but is still contained"
            );
        }
    }
}

/// The **insertion** path of both caches, over a chain the real growth seam builds: after
/// every growth the containment index mirrors the retired spaces exactly (one entry per
/// space, naming its own frozen live region) and the range test answers exactly the scan
/// over `spaces()` at every probe. The chain here is the longest [`BUDGET`] allows —
/// `≈10-14` spaces, since the ladder climbs a curated step per call.
#[test]
fn a_chain_the_growth_seam_builds_keeps_the_index_and_the_aggregate_exact() {
    let policy = budgeted();
    let mut nursery = Nursery::new(default_allocator().clone(), CAP).expect("nursery");
    let (retired, err) = grow_chain(&mut nursery, &policy);

    // The chain really grew, and stopped on the typed refusal rather than an unlucky shape.
    assert!(
        retired.len() >= 8,
        "the budget must yield a real chain, got {} growths",
        retired.len()
    );
    assert_eq!(err.as_atom(), Some("out_of_memory"));
    assert_eq!(nursery.space_count(), retired.len() + 1);
    assert!(nursery.capacity_words() <= policy.max_heap_words);

    // The index mirrors the chain exactly.
    assert_eq!(nursery.retired_regions.len(), nursery.retired.len());
    for space in &nursery.retired {
        assert_eq!(
            nursery.retired_regions.get(&(space.base() as usize)),
            Some(&(space.top() as usize)),
            "the index names every retired space's live region"
        );
    }

    // The range test is exactly the reference predicate over `spaces()`.
    let mut regions = retired;
    regions.push((
        nursery.current().base() as usize,
        nursery.current().top() as usize,
    ));
    let mut inside = 0usize;
    for &addr in &region_probes(&regions) {
        let want = nursery
            .spaces()
            .any(|space| space.contains(addr as *const Eterm));
        assert_eq!(
            nursery.contains(addr as *const Eterm),
            want,
            "contains({addr:#x}) must equal the scan over `spaces()`"
        );
        inside += usize::from(want);
    }
    assert!(
        inside >= regions.len() - 1,
        "the probes must reach inside the chain, got {inside}"
    );

    // The aggregate covers every space of the grown chain.
    let extents: Vec<(usize, usize)> = nursery
        .spaces()
        .map(reserved_extent)
        .collect();
    assert_eq!(
        (nursery.bounds().base(), nursery.bounds().limit()),
        covering(&extents),
        "the aggregate is the covering of the grown chain"
    );
}

/// **(a) The chain's summed capacity stops at the ceiling.** Under a bounded policy every
/// successful growth keeps `capacity_words() <= max_heap_words`, and the loop must end in the
/// typed heap exhaustion — the budget, not an allocation accident — with the nursery
/// byte-for-byte untouched by the refused call.
#[test]
fn growth_stops_at_the_summed_capacity_ceiling() {
    let policy = budgeted();
    let mut nursery = Nursery::new(default_allocator().clone(), CAP).expect("nursery");
    let mut growths = 0usize;
    let err = loop {
        // Fill what is left of the current space, so the growth is never a no-op.
        fill(nursery.current_mut());
        let need = nursery.current().available_words().saturating_add(1);
        assert!(
            nursery.current().available_words() < need,
            "the request outgrows the current space"
        );
        match nursery.grow(&policy, need) {
            Ok(()) => {
                growths += 1;
                assert!(
                    nursery.capacity_words() <= policy.max_heap_words,
                    "round {growths}: the summed capacity is capped, got {} > {}",
                    nursery.capacity_words(),
                    policy.max_heap_words
                );
                assert!(growths < 4096, "the growth ladder must terminate");
            }
            Err(err) => break err,
        }
    };

    // The loop ended because the remaining budget could not cover the request — the typed
    // refusal, never a panic and never a silently smaller space.
    assert_eq!(err.as_atom(), Some("out_of_memory"));
    assert!(
        growths >= 3,
        "the corpus must really climb the ladder, got {growths} growths"
    );
    assert!(
        policy.max_heap_words - nursery.capacity_words() < nursery.current().available_words() + 1,
        "the refusal is the exhausted budget: {} of {} words left",
        policy.max_heap_words - nursery.capacity_words(),
        policy.max_heap_words
    );

    // The refused call left the nursery exactly as it was — same current space, same chain
    // length, same summed capacity — so a caller may retry it after collecting.
    let (spaces, capacity, base) = (
        nursery.space_count(),
        nursery.capacity_words(),
        nursery.current().base(),
    );
    let again = nursery.grow(&policy, nursery.current().available_words() + 1);
    assert_eq!(again.expect_err("still capped").as_atom(), Some("out_of_memory"));
    assert_eq!(nursery.space_count(), spaces, "a refused growth retires nothing");
    assert_eq!(nursery.capacity_words(), capacity, "and consumes no capacity");
    assert_eq!(nursery.current().base(), base, "and replaces no space");
}

/// **(b) A provably empty retired space is re-adopted, not replaced.** The swap allocates
/// nothing, leaves the chain's summed capacity and length unchanged, keeps every live word
/// addressable, and returns the empty space to service. The control chain — identical but
/// with a retired space that handed the mutator a word — cannot reuse it and does grow,
/// which is what makes the assertion discriminating.
#[test]
fn a_provably_empty_retired_space_is_reused_instead_of_reallocated() {
    let policy = HeapPolicy::unbounded();

    let mut nursery = Nursery::new(default_allocator().clone(), 500).expect("nursery");
    // The first growth retires the 500-word space **provably empty**: nothing was allocated
    // in it before the growth.
    nursery.grow(&policy, 501).expect("the first growth");
    assert_eq!(nursery.space_count(), 2);
    assert_eq!(
        nursery.retired[0].used_words(),
        0,
        "the retired space never handed out a word"
    );
    let reused_base = nursery.retired[0].base();
    let capacity = nursery.capacity_words();
    let current_base = nursery.current().base();

    // Fill the current space, put a live term in it, and ask for room it cannot supply.
    fill(nursery.current_mut());
    let live = nursery
        .current_mut()
        .heap_mut()
        .tuple(&[make_small(7)])
        .expect("the live term fits");
    let need = nursery.current().available_words() + 1;
    nursery.grow(&policy, need).expect("the re-adoption");

    assert_eq!(
        nursery.current().base(),
        reused_base,
        "the provably empty retired space is the current space again"
    );
    assert_eq!(
        nursery.capacity_words(),
        capacity,
        "a re-adoption consumes no capacity"
    );
    assert_eq!(nursery.space_count(), 2, "a re-adoption retires the old current");
    assert_eq!(
        nursery.retired[0].base(),
        current_base,
        "the space it replaced is retired in its place"
    );
    assert!(
        nursery.contains(ptr_val(live) as *const Eterm),
        "the term in the newly retired space is still part of the nursery"
    );
    assert!(
        nursery.retired[0].used_words() > 0,
        "the space it replaced holds the words the mutator was given"
    );
    assert!(
        nursery.contains(current_base as *const Eterm),
        "and the range test covers its live region"
    );

    // The contrast: the same chain, but the retired space is **not** provably empty, so the
    // request has to install a fresh (larger) space and the summed capacity grows by it.
    let mut control = Nursery::new(default_allocator().clone(), 500).expect("nursery");
    control
        .current_mut()
        .heap_mut()
        .tuple(&[make_small(1)])
        .expect("a live word");
    control.grow(&policy, 501).expect("the first growth");
    assert!(
        control.retired[0].used_words() > 0,
        "the control's retired space is not empty"
    );
    let control_capacity = control.capacity_words();
    fill(control.current_mut());
    let need = control.current().available_words() + 1;
    control.grow(&policy, need).expect("a fresh space");
    assert!(
        control.capacity_words() > control_capacity,
        "without an empty space to re-adopt the chain grows: {} -> {}",
        control_capacity,
        control.capacity_words()
    );
    assert_eq!(control.space_count(), 3);
    assert!(
        control.current().capacity_words() > 610,
        "the fresh space is the doubling the sizing rule prescribes"
    );
    }
