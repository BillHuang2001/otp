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
//! Two properties keep the chain from becoming a cost:
//!
//! * **Containment is indexed, not scanned.** [`contains`](Nursery::contains) — the per-word
//!   range test a host's *unscanned-root* valve runs at every heap admission — answers from
//!   an ordered index of the retired spaces' live regions plus one direct test of the current
//!   space, so it is `O(log #spaces)` rather than a walk over every space.
//! * **The aggregate extent is cached.** [`bounds`](Nursery::bounds) hands back the covering
//!   interval of **every** space's reserved extent (`[base, limit)`, not the moving live fill)
//!   plus a geometry epoch, maintained in `O(1)` by [`grow`](Nursery::grow) and
//!   [`collapse`](Nursery::collapse) — the whole-collector half a host builds
//!   [`ScannedBounds`](crate::ScannedBounds) from, so its unscanned-root valve can reject an
//!   address outside the chain with one comparison instead of the indexed test.
//! * **The chain is bounded.** [`grow`](Nursery::grow) takes the plain one-step next size
//!   whenever that already enlarges the space, and only falls back to at least a **doubling**
//!   when the one-step target would install a space no larger than the one it replaces, so a
//!   growth loop that keeps enlarging an equally-full space climbs the growth table
//!   geometrically: the chain is `O(log(heap))` long instead of gaining one ≈same-size space
//!   per call.
//!
//! Every superseded space is **retained until a collection evacuates it**, never freed by the
//! growth itself: it may hold live words (they were handed to the mutator) and only a
//! [collection](Nursery::collapse) proves them dead. Nothing in this module ever frees a
//! retired space outside [`collapse`](Nursery::collapse).
//! [`retains_old_spaces`](Nursery::retains_old_spaces) reports such a retained chain — the
//! "collection debt" a growth accumulates and only a collection can clear, exported to hosts
//! as [`Collector::collection_debt`](crate::Collector::collection_debt).
//!
//! Once a collection has evacuated **every** space the chain is no longer needed, so
//! [`Nursery::collapse`] drops the retired spaces and re-sizes the single remaining current
//! space — the chain is exactly one space before any mutator allocates again. While a chain
//! is live, every space is part of the nursery: [`spaces`](Nursery::spaces) enumerates them
//! all, [`contains`](Nursery::contains) covers every one — the current space directly and the
//! retired ones through its ordered index — and
//! [`add_to_from_set`](Nursery::add_to_from_set) adds every live region to a collection's
//! from-set, so a collection evacuates a chained nursery whole.
//!
//! This module is safe Rust — it only manipulates [`HeapSpace`] values; `unsafe` stays
//! confined to the word-level modules of the crate.

use std::collections::BTreeMap;

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::Eterm;

use crate::bounds::ScannedBounds;
use crate::error::heap_exhausted;
use crate::policy::HeapPolicy;
use crate::space::{FromSet, HeapSpace};

/// The reserved extent `[base, limit)` of `space`, as a generation-`0` [`ScannedBounds`].
///
/// The **reserved** extent, deliberately, not the live fill `[base, top)`: `top` moves on every
/// allocation (so a cached bound would go stale immediately) while `limit` is fixed for the
/// space's whole lifetime (a [`HeapSpace`] never reallocates). The aggregate therefore covers
/// every live region — the safe direction for a reject (see [`ScannedBounds`]).
fn space_extent(space: &HeapSpace) -> ScannedBounds {
    ScannedBounds::new(space.base() as usize, space.limit() as usize)
}

/// The young nursery: the space the mutator allocates in (`current`) plus every space a
/// growth has **retired** (the larger space a growth installed replaces it, never moves it).
pub(crate) struct Nursery {
    /// The space the mutator allocates in.
    current: HeapSpace,
    /// The spaces a growth retired, oldest first. Their live words are still addressable and
    /// are still part of the nursery (a collection evacuates them; a range test covers them).
    retired: Vec<HeapSpace>,
    /// The retired spaces' live regions `[base, top)`, keyed by **base byte address** — the
    /// containment index [`contains`](Nursery::contains) consults.
    ///
    /// *Why it is exact:* every space is a **distinct live allocation**, so the intervals
    /// `[base, top)` are pairwise **disjoint**; sorted by base they are therefore
    /// non-overlapping, and the greatest base `<= addr` is the only candidate — `addr < top`
    /// decides it exactly, with the same half-open semantics as
    /// [`HeapSpace::contains`] (`top` is exclusive).
    ///
    /// *Why an entry never goes stale:* nothing is ever allocated in a retired space, so its
    /// `top` is **frozen** from the moment it is retired until [`collapse`](Nursery::collapse)
    /// drops it (and clears this map with it). The current space is the only one whose `top`
    /// moves, and it is tested directly instead of being indexed.
    ///
    /// The map mirrors `retired` exactly (all live spaces default to a real allocation and
    /// therefore distinct bases); a same-`base` insert could only come from a zero-capacity
    /// space — an empty live region for which both entries answer `false` identically.
    retired_regions: BTreeMap<usize, usize>,
    /// The covering interval of **every** space's reserved extent (`[base, limit)`), maintained
    /// incrementally — the chain-wide [`ScannedBounds`] the whole-collector aggregate is built
    /// from, so a host's fast-reject filter costs `O(1)` per address instead of the indexed
    /// per-space test.
    ///
    /// *Why it cannot go stale:* a space's `limit` is fixed for its lifetime, so the interval
    /// only has to change when the **set** of spaces does — a space is installed
    /// ([`grow`](Nursery::grow), which only widens the covering: `min`/`max`) or the chain is
    /// recomputed down to one space ([`collapse`](Nursery::collapse)). Those are exactly the
    /// two edit points, and each advances the carried
    /// [generation](ScannedBounds::generation).
    ///
    /// *Why it is exact (not merely wide):* the covering of the spaces' extents **is**
    /// `min(bases) .. max(limits)`, so [`bounds`](Nursery::bounds) answers precisely the union's
    /// bounding interval — no accumulated slack from earlier, since-dropped spaces.
    extent: ScannedBounds,
}

impl Nursery {
    /// A nursery with a single empty `capacity_words`-word current space and no retired
    /// spaces, backed by `alloc`.
    pub(crate) fn new(alloc: AllocRef, capacity_words: usize) -> Result<Nursery> {
        let current = HeapSpace::with_allocator(alloc, capacity_words)?;
        let extent = space_extent(&current);
        Ok(Nursery {
            current,
            retired: Vec::new(),
            retired_regions: BTreeMap::new(),
            extent,
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
    /// This is the per-collector basis of the hosts' **opt-in** debt query
    /// ([`Collector::collection_debt`](crate::Collector::collection_debt)): a `true` answer
    /// means the heap-admission door should treat the chain like a full nursery and collect.
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
    ///
    /// This is the range test a host's unscanned-root valve evaluates **per reference word
    /// and per heap admission**, so it is answered in `O(log #spaces)`: the current space
    /// directly (it is the only one whose `top` moves) and the retired spaces from the
    /// ordered [`retired_regions`](Nursery::retired_regions) index. It is **exactly** the
    /// predicate `spaces().any(|space| space.contains(ptr))` for every address — the retired
    /// regions are disjoint, so the greatest base at or below the address is the only
    /// candidate ([`retired_regions`](Nursery::retired_regions)).
    pub(crate) fn contains(&self, ptr: *const Eterm) -> bool {
        if self.current.contains(ptr) {
            return true;
        }
        let addr = ptr as usize;
        self.retired_regions
            .range(..=addr)
            .next_back()
            .is_some_and(|(&_base, &top)| addr < top)
    }

    /// Adds the live region of **every** nursery space to `from`, so a collection evacuates the
    /// whole chain.
    pub(crate) fn add_to_from_set(&self, from: &mut FromSet) {
        for space in self.spaces() {
            from.add(space);
        }
    }

    /// The chain-wide aggregate extent: the covering interval `[base, limit)` of **every** space
    /// of the chain (current and retired), plus the chain's geometry
    /// [generation](ScannedBounds::generation).
    ///
    /// `O(1)` and allocation-free — the interval is maintained by [`grow`](Nursery::grow) and
    /// [`collapse`](Nursery::collapse) rather than computed here — and built from each space's
    /// **reserved** extent, so it is a superset of every live region: an address it rejects is
    /// in no nursery space, while an address it accepts may be a hole between two spaces or the
    /// unallocated tail of the current space and still needs [`contains`](Nursery::contains).
    pub(crate) fn bounds(&self) -> ScannedBounds {
        self.extent
    }

    /// Enlarges the nursery so a `need_words`-word allocation can proceed — the
    /// **NO-RELOCATION** growth seam.
    ///
    /// It **never collects and never moves a single live term**: the old space is retained in
    /// `retired` (so every live word stays addressable at its original address) and the
    /// allocator is pointed at a fresh, larger space. A caller that holds no root set may
    /// therefore use it safely.
    ///
    /// The superseded space is **retained** (never freed here), whatever its occupancy: a
    /// space that handed the mutator words may hold live terms and only a
    /// [collection](Nursery::collapse) proves them dead, so the runtime never frees a retired
    /// space outside `collapse`. Dropping a provably empty (`used_words() == 0`) superseded
    /// space would be sound, but it changes `space_count`, so it is deliberately not done.
    ///
    /// The fresh space is the **one-step** size the policy allows —
    /// `policy.next_words(need_words, 0)` floored by the current capacity — whenever that
    /// already enlarges the space (a genuinely larger request: unchanged from the plain
    /// next-size rule). Only when the one-step target would install a space **no larger than
    /// the one it replaces** does the rule take at least a **doubling**
    /// (`policy.next_words(2 * capacity, 0)`), so a growth loop that keeps enlarging an
    /// equally-full space climbs the growth table geometrically and the chain of retired
    /// spaces stays `O(log(heap))` long, instead of gaining one ≈same-size space — and one
    /// whole retained capacity — per call. Every size is bounded by
    /// [`min_heap_words`](HeapPolicy::min_heap_words) and
    /// [`max_heap_words`](HeapPolicy::max_heap_words) through
    /// [`next_words`](HeapPolicy::next_words) and by the growth table's ceiling.
    ///
    /// When the current space already has room the call is a no-op. A
    /// [`max_heap_words`](HeapPolicy::max_heap_words) (or growth-table) ceiling that cannot
    /// reach the request yields the typed [`BeamError::Alloc`](beam_common::BeamError::Alloc)
    /// and leaves the nursery untouched — the current space is not replaced and nothing is
    /// retired. (A ceiling that can cover the request but forbids a larger step installs the
    /// largest space it allows, which is the only size left to it.)
    pub(crate) fn grow(&mut self, policy: &HeapPolicy, need_words: usize) -> Result<()> {
        if self.current.available_words() >= need_words {
            return Ok(());
        }
        let capacity = self.current.capacity_words();
        // The plain next-size rule, floored by the capacity it must not undercut.
        let one_step = policy.next_words(need_words, 0).max(capacity);
        // Only a request that would hand back a space no larger than the one it replaces
        // needs the doubling: it is what bounds the chain logarithmically.
        let target = if one_step > capacity {
            one_step
        } else {
            policy
                .next_words(capacity.saturating_mul(2), 0)
                .max(one_step)
        };
        if target < need_words {
            return Err(heap_exhausted(need_words));
        }
        let fresh = HeapSpace::with_allocator(self.current.allocator().clone(), target)?;
        let old = core::mem::replace(&mut self.current, fresh);
        self.retired_regions
            .insert(old.base() as usize, old.top() as usize);
        self.retired.push(old);
        // The chain gained the freshly installed space; the retired one was already covered (it
        // was the current space when it entered the aggregate). Widening by `min`/`max` makes
        // the update `O(1)` regardless of how long the chain is, and the geometry epoch
        // advances because the set of spaces did.
        self.extent = self
            .extent
            .covering(space_extent(&self.current))
            .with_generation(self.extent.generation().saturating_add(1));
        Ok(())
    }
    /// Collapses the chain back to **exactly one** space, after a collection has evacuated
    /// every space.
    ///
    /// It drops the retired spaces (and their containment index) and re-sizes the single
    /// remaining current space from the heap-size policy
    /// ([`HeapPolicy::young_after_collection`]) over the capacity the chain held when it was
    /// collected, where `live_words` is what the collection left in the nursery (the promoted
    /// words for a minor, `0` after a fullsweep). The space is only replaced when the target
    /// differs; otherwise its bump pointer is merely rewound.
    pub(crate) fn collapse(&mut self, policy: &HeapPolicy, live_words: usize) -> Result<()> {
        let collected_capacity = self.capacity_words();
        let target = policy.young_after_collection(collected_capacity, live_words);
        self.retired.clear();
        self.retired_regions.clear();
        if self.current.capacity_words() == target {
            self.current.reset();
        } else {
            let alloc = self.current.allocator().clone();
            self.current = HeapSpace::with_allocator(alloc, target)?;
        }
        // Down to exactly one space, so the aggregate is re-seeded from it rather than widened:
        // the retired spaces' extents are gone and a stale widening would keep rejecting
        // addresses far outside the collapsed chain's neighbourhood. The epoch advances — the
        // set of spaces *and* the surviving space's live region both changed.
        self.extent = space_extent(&self.current)
            .with_generation(self.extent.generation().saturating_add(1));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the nursery chain's invariants: the containment index mirrors the
    //! retired spaces **exactly**, the range test it answers is exactly
    //! `spaces().any(|space| space.contains(ptr))`, a growth neither collects, moves nor
    //! frees anything, and the growth rule takes the one-step size unless that would not
    //! enlarge the space. The index is what keeps the range test independent of the chain's
    //! length, so it is pinned over a chain far longer than any hand-written case would build.

    use beam_alloc::registry::default_allocator;
    use beam_term::prelude::{make_small, ptr_val};

    use super::*;

    /// The capacity of **every** space of the test chain, in term words — and the policy's
    /// `max_heap_words` ceiling, so a growth is clamped to a space of exactly the size it
    /// replaces: the one shape a caller can use to make the chain arbitrarily long through the
    /// growth seam alone (a real heap's growth ladder tops out at the growth table's
    /// ceiling).
    const CAP: usize = 90;

    /// Retired spaces the test builds before it samples the range test.
    const RETIRED: usize = 1024;

    /// A policy whose growth ceiling pins every space to [`CAP`] words.
    fn capped() -> HeapPolicy {
        HeapPolicy {
            max_heap_words: CAP,
            initial_young_words: CAP,
            ..HeapPolicy::default()
        }
    }

    /// Grows the chain by [`RETIRED`] retired spaces, reporting each retired space's live region
    /// `[base, top)` as it stood immediately before the growth that retired it.
    fn build_chain(nursery: &mut Nursery) -> Vec<(usize, usize)> {
        let policy = capped();
        let mut regions = Vec::with_capacity(RETIRED);
        for i in 0..RETIRED {
            // Hand the mutator `1 + i % 4` words, so the retired regions' live extents (and
            // therefore their `top`s) differ — the range test must read each one's own `top`.
            let elems: Vec<Eterm> = (0..(i % 4)).map(|k| make_small(k as i64)).collect();
            nursery
                .current_mut()
                .heap_mut()
                .tuple(&elems)
                .unwrap_or_else(|e| panic!("round {i}: the tuple fits the space: {e:?}"));
            regions.push((
                nursery.current().base() as usize,
                nursery.current().top() as usize,
            ));
            nursery
                .grow(&policy, CAP)
                .unwrap_or_else(|e| panic!("round {i}: the capped growth: {e:?}"));
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
    fn a_capped_growth_retires_but_never_collects_moves_or_frees() {
        let mut nursery = Nursery::new(default_allocator().clone(), CAP).expect("nursery");
        let policy = capped();

        // A live term in the space the growth will retire, at a known address.
        let live = nursery
            .current_mut()
            .heap_mut()
            .tuple(&[make_small(11), make_small(12)])
            .expect("tuple");
        let base = nursery.current().base();
        let used = nursery.current().used_words();

        nursery.grow(&policy, CAP).expect("the capped growth");

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

        // The growth is bounded by the ceiling: every space it installs is `CAP` words, so the
        // chain's capacity is the chain's length times `CAP` (the capped shape of the growth
        // rule: a ceiling that forbids a larger step installs the largest space it allows, the
        // only size left to it).
        assert_eq!(nursery.capacity_words(), 2 * CAP);

        // A request the ceiling cannot reach is the typed refusal, and the nursery is as it was.
        let spaces_before = nursery.space_count();
        let err = nursery.grow(&policy, CAP + 1).expect_err("capped");
        assert_eq!(err.as_atom(), Some("out_of_memory"));
        assert_eq!(
            nursery.space_count(),
            spaces_before,
            "a refused growth retires nothing"
        );
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
        // `O(log(heap))` long. The pathological shape this replaces (`grow` handing back a space
        // of the size it replaces) is only reachable through a `max_heap_words` ceiling, which
        // no step can enlarge past (the long-chain tests build their chain that way).
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
        let policy = capped();
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
        // exactly one, and leaves the interval the covering of **every** space's extent.
        for i in 0..RETIRED {
            nursery
                .current_mut()
                .heap_mut()
                .tuple(&[])
                .expect("an empty tuple fits every space");
            nursery
                .grow(&policy, CAP)
                .unwrap_or_else(|e| panic!("round {i}: the capped growth: {e:?}"));
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
        }

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
}
