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
//! Five properties keep the chain from becoming a cost:
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
//! * **The chain's *summed* capacity obeys the ceiling.** [`capacity_words`](Nursery::capacity_words)
//!   is what `HeapPolicy::max_heap_words` bounds — a fresh space may only claim the budget the
//!   chain has left, so `capacity_words() <= max_heap_words` after every successful growth. A
//!   request the remaining budget cannot reach is the typed heap exhaustion, returned before
//!   anything is allocated or retired.
//! * **A provably empty retired space is re-adopted, not replaced.** A space a growth retired
//!   without ever handing out a word can serve a later request itself: the swap allocates
//!   nothing and consumes none of the chain's capacity budget, which is the second reason the
//!   chain cannot be driven past the ceiling by a loop of small requests.
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
    /// [generation](ScannedBounds::generation). A
    /// [re-adoption](Nursery::readopt_retired) changes no set at all (it installs a space the
    /// chain already held and retires another of them), so it cannot move the interval — but it
    /// does advance the epoch, because which space is current is part of the geometry a caller
    /// caches alongside it.
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
    ///
    /// This is the quantity `HeapPolicy::max_heap_words` bounds:
    /// [`grow`](Nursery::grow) only ever claims the budget the chain has left, so after every
    /// successful growth `capacity_words() <= max_heap_words` whenever the ceiling is non-zero.
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
    /// It **never collects and never moves a single live term**: a superseded space is handed to
    /// `retired` (so every live word stays addressable at its original address) and the allocator
    /// is pointed at a space with room. A caller that holds no root set may therefore use it
    /// safely.
    ///
    /// # Decision order
    ///
    /// 1. **Room already there.** `current.available_words() >= need_words` is a no-op: nothing
    ///    is installed, nothing is retired, no capacity is consumed.
    /// 2. **Re-adopt a provably empty retired space** ([`empty_retired_for`](Nursery::empty_retired_for),
    ///    [`readopt_retired`](Nursery::readopt_retired)). A space a previous growth retired
    ///    without ever handing the mutator a word (`used_words() == 0`) and still large enough for
    ///    the request can serve it as it stands: it holds no live word, so nothing is stranded,
    ///    and the swap allocates **nothing** and changes the chain's summed capacity by
    ///    **nothing** — it consumes none of the
    ///    [`max_heap_words`](HeapPolicy::max_heap_words) budget of step 3. The **smallest**
    ///    adequate space wins (the oldest among equals), so the selection is deterministic and
    ///    leaves the largest retired spaces free for a later, bigger request.
    /// 3. **Allocate a fresh space.** Its size is the **one-step** size the policy allows —
    ///    `policy.next_words(need_words, 0)` floored by the current capacity — whenever that
    ///    already enlarges the space (a genuinely larger request: the plain next-size rule).
    ///    Only when the one-step target would install a space **no larger than the one it
    ///    replaces** does the rule take at least a **doubling**
    ///    (`policy.next_words(2 * capacity, 0)`), so a growth loop that keeps enlarging an
    ///    equally-full space climbs the growth table geometrically and the chain of retired
    ///    spaces stays `O(log(heap))` long, instead of gaining one ≈same-size space — and one
    ///    whole retained capacity — per call. Every size is bounded by
    ///    [`min_heap_words`](HeapPolicy::min_heap_words) through
    ///    [`next_words`](HeapPolicy::next_words) and by the growth table's ceiling.
    /// 4. **Refuse.** A [`max_heap_words`](HeapPolicy::max_heap_words) (or growth-table) ceiling
    ///    that cannot reach the request yields the typed
    ///    [`BeamError::Alloc`](beam_common::BeamError::Alloc) and leaves the nursery untouched —
    ///    the current space is not replaced and nothing is retired.
    ///
    /// # The ceiling bounds the chain's SUMMED capacity
    ///
    /// [`capacity_words`](Nursery::capacity_words) is the chain's size, so that is what
    /// `max_heap_words` bounds: the fresh space of step 3 may only claim the **budget the chain
    /// has left** (`max_heap_words - capacity_words()`), and once that budget cannot reach
    /// `need_words` the request is refused for good — an allocate-and-abandon loop can no longer
    /// append one ceiling-sized space per call. After every successful growth
    /// `capacity_words() <= max_heap_words` whenever the ceiling is non-zero. A ceiling that *can*
    /// cover the request but forbids a larger step installs the largest space it allows, which is
    /// the only size left to it. `max_heap_words == 0` is "no upper bound", the crate's encoding
    /// for unbounded growth, and leaves the size of step 3 alone.
    ///
    /// Both refusals are decided **before** any space is allocated or retired, so a refused
    /// growth leaves the nursery byte-for-byte as it was (same current base, same chain length,
    /// same summed capacity) and a caller may retry it after collecting.
    ///
    /// A superseded space is never *freed* here, whatever its occupancy: a space that handed the
    /// mutator words may hold live terms and only a [collection](Nursery::collapse) proves them
    /// dead, so the runtime never frees a retired space outside `collapse`. A space that never
    /// handed out a word is not freed either — it is [re-adopted](Nursery::readopt_retired)
    /// instead, which keeps `space_count` and the containment index meaningful.
    pub(crate) fn grow(&mut self, policy: &HeapPolicy, need_words: usize) -> Result<()> {
        if self.current.available_words() >= need_words {
            return Ok(());
        }
        // A retired space nothing was ever allocated in can serve this request itself: it
        // strands no live word, allocates nothing and consumes none of the capacity budget.
        if let Some(index) = self.empty_retired_for(need_words) {
            self.readopt_retired(index);
            return Ok(());
        }
        let capacity = self.current.capacity_words();
        // The plain next-size rule, floored by the capacity it must not undercut.
        let one_step = policy.next_words(need_words, 0).max(capacity);
        // Only a request that would hand back a space no larger than the one it replaces
        // needs the doubling: it is what bounds the chain logarithmically.
        let mut target = if one_step > capacity {
            one_step
        } else {
            policy
                .next_words(capacity.saturating_mul(2), 0)
                .max(one_step)
        };
        if target < need_words {
            return Err(heap_exhausted(need_words));
        }
        // The ceiling bounds the chain's **summed** capacity, so the fresh space may only claim
        // the budget the chain has left. Refuse — before allocating or retiring anything —
        // when that budget cannot reach the request.
        if policy.max_heap_words != 0 {
            let budget = policy.max_heap_words.saturating_sub(self.capacity_words());
            if budget < need_words {
                return Err(heap_exhausted(need_words));
            }
            target = target.min(budget);
        }
        let fresh = HeapSpace::with_allocator(self.current.allocator().clone(), target)?;
        // The chain gained the freshly installed space; the retired one was already covered (it
        // was the current space when it entered the aggregate). Widening by `min`/`max` makes
        // the update `O(1)` regardless of how long the chain is, and the geometry epoch
        // advances because the set of spaces did.
        self.install(fresh);
        self.widen_extent();
        Ok(())
    }

    /// The index of the **smallest** retired space that never handed the mutator a word
    /// (`used_words() == 0`) and is still large enough for a `need_words` allocation — the space
    /// [`grow`](Nursery::grow) can re-adopt instead of allocating a fresh one.
    ///
    /// The smallest adequate space wins, and [`min_by_key`](Iterator::min_by_key) keeps the first
    /// minimum, so among equal capacities the **oldest** space is chosen: the selection is
    /// deterministic, and it leaves the largest retired spaces available for a later, larger
    /// request. `None` when no retired space is both empty and large enough — the only case in
    /// which a growth has to allocate.
    fn empty_retired_for(&self, need_words: usize) -> Option<usize> {
        self.retired
            .iter()
            .enumerate()
            .filter(|(_, space)| space.used_words() == 0 && space.capacity_words() >= need_words)
            .min_by_key(|(_, space)| space.capacity_words())
            .map(|(index, _)| index)
    }

    /// Re-adopts the retired space at `index` as the current space, retiring the old current in
    /// its place — the step [`grow`](Nursery::grow) takes instead of allocating when a retired
    /// space is provably empty.
    ///
    /// Nothing is stranded (the re-adopted space never handed the mutator a word), nothing is
    /// allocated, and the chain's summed capacity and length are both unchanged. The chain's
    /// **set** of spaces is unchanged too — the same allocations, a different one of them
    /// designated current — so the aggregate covering cannot move and is left alone; only the
    /// geometry epoch advances, because the partition (and with it the role of every address:
    /// which space's `top` may still move) changed.
    fn readopt_retired(&mut self, index: usize) {
        let space = self.retired.remove(index);
        self.retired_regions.remove(&(space.base() as usize));
        self.install(space);
        self.extent = self
            .extent
            .with_generation(self.extent.generation().saturating_add(1));
    }

    /// Installs `space` as the current space and retires the one it replaces, keeping the
    /// containment index ([`retired_regions`](Nursery::retired_regions)) and the `retired` chain
    /// in step. The allocator is untouched, and nothing is freed.
    fn install(&mut self, space: HeapSpace) {
        let old = core::mem::replace(&mut self.current, space);
        self.retired_regions
            .insert(old.base() as usize, old.top() as usize);
        self.retired.push(old);
    }

    /// Widens the cached aggregate extent to cover the space [`install`](Nursery::install) just
    /// made current, and advances the geometry epoch by one.
    fn widen_extent(&mut self) {
        self.extent = self
            .extent
            .covering(space_extent(&self.current))
            .with_generation(self.extent.generation().saturating_add(1));
    }
    /// Collapses the chain back to **exactly one** space, after a **minor** has evacuated
    /// every space.
    ///
    /// It drops the retired spaces (and their containment index) and re-sizes the single
    /// remaining current space from the heap-size policy
    /// ([`HeapPolicy::young_after_collection`]) over the capacity the chain held when it was
    /// collected, where `demand_words` is what the round measured: for a minor the words it
    /// **promoted** (which is what it left alive in the young generation). The space is only
    /// replaced when the target differs; otherwise its bump pointer is merely rewound.
    ///
    /// A **fullsweep** passes the post-copy live set as well and therefore calls
    /// [`collapse_after_fullsweep`](Nursery::collapse_after_fullsweep) — the C applies a
    /// second, post-copy sizing rule to a fullsweep (`erl_gc.c:2062-2098`) that a minor never
    /// reaches.
    ///
    /// # The sizing input is bounded by the collected capacity, so this can never creep
    ///
    /// `demand_words` is at most `capacity_words()` (the chain's own occupancy cannot exceed
    /// the chain's capacity), and every branch of [`HeapPolicy::young_after_collection`] that
    /// could enlarge the target beyond `collected_capacity` needs `demand_words >
    /// collected_capacity`, which that bound forbids. So **`collapse` never grows the
    /// nursery**: the post-collection capacity is at most the pre-collection capacity, and a
    /// collection that finds the nursery mostly empty shrinks it towards the demand. The only
    /// thing that enlarges a nursery is the explicit, request-driven
    /// [`grow`](Nursery::grow) seam — so a fixed demand converges to a constant capacity
    /// instead of climbing one growth-table step per collection.
    pub(crate) fn collapse(&mut self, policy: &HeapPolicy, demand_words: usize) -> Result<()> {
        let collected_capacity = self.capacity_words();
        let target = policy.young_after_collection(collected_capacity, demand_words);
        self.install_collapsed(target)
    }

    /// Collapses the chain after a **fullsweep**, applying the C's **two-step** nursery sizing
    /// ([`HeapPolicy::young_after_fullsweep`]): the pre-copy occupancy sizing
    /// (`erl_gc.c:1866`) followed by the post-copy live-set shrink (`erl_gc.c:2062-2098`).
    ///
    /// `observed_words` is the occupancy the round sampled **before** the copy and
    /// `live_words` the **whole-heap** post-copy live word count (young *and* mature), which is
    /// the input the second step needs: a fullsweep leaves no live young set of its own, so its
    /// own surviving set cannot size the nursery, and a nursery-only count would collapse it to
    /// the reserve floor every round (`MEASURED.md`).
    ///
    /// Both inputs only ever *shrink* the target relative to the collected capacity, so this
    /// keeps [`collapse`](Nursery::collapse)'s guarantee that a collection never enlarges a
    /// nursery.
    pub(crate) fn collapse_after_fullsweep(
        &mut self,
        policy: &HeapPolicy,
        observed_words: usize,
        live_words: usize,
    ) -> Result<()> {
        let collected_capacity = self.capacity_words();
        let target =
            policy.young_after_fullsweep(collected_capacity, observed_words, live_words);
        self.install_collapsed(target)
    }

    /// The shared body of the two collapse seams: install (or rewind to) a single space of
    /// `target` words, drop the retired chain, and re-seed the aggregate extent.
    fn install_collapsed(&mut self, target: usize) -> Result<()> {
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
mod tests;
