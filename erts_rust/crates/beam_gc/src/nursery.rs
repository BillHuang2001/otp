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
//! * **The chain is bounded.** A growth always installs a space **strictly larger** than the one
//!   it retires ([`HeapPolicy::next_words_above`], the C's "never the same size when *growing*"
//!   rule `erl_gc.c:310-338`), so a growth event can never stack an equally sized space onto the
//!   chain — one whole retained capacity that grants no room. Once the nursery has **collected**
//!   it also remembers the live-set target that collection derived and narrows every fresh space
//!   from above at `max(one_step, live_target)` (`live_target_words`), so the growth seam cannot
//!   install a capacity the next collection would have to give back. And a chain that already
//!   **retains** a space may only grow while its summed capacity stays inside the live set's keep
//!   band ([`HeapPolicy::chain_cap_words`], `YOUNG_SHRINK_FACTOR` times the live target) — the C's
//!   one-young-block-plus-one-bounded-fragment shape (`delay_garbage_collection`,
//!   `erl_gc.c:500-599`) — so `capacity_words()` is bounded by the **live set** and is *flat in
//!   the number of growth events* rather than one capacity per event.
//! * **The chain's *summed* capacity obeys the ceiling.** [`capacity_words`](Nursery::capacity_words)
//!   is what `HeapPolicy::max_heap_words` bounds — a fresh space may only claim the budget the
//!   chain has left, so `capacity_words() <= max_heap_words` after every successful growth. A
//!   request the remaining budget cannot reach is the typed heap exhaustion, returned before
//!   anything is allocated or retired.
//! * **A provably empty retired space is re-adopted, not replaced.** A space a growth retired
//!   without ever handing out a word can serve a later request itself: the swap allocates
//!   nothing and consumes none of the chain's capacity budget, which is the second reason the
//!   chain cannot be driven past the ceiling by a loop of small requests. The empty spaces are
//!   **indexed by capacity** ([`empty_retired`](Nursery::empty_retired)), so selecting one is
//!   `O(log #spaces)` and no growth event scans the chain.
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

use std::collections::{BTreeMap, BTreeSet};

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
    /// The subset of `retired` that never handed the mutator a word — the spaces
    /// [`grow`](Nursery::grow) may **re-adopt** instead of allocating — keyed by
    /// `(capacity_words, retirement serial, base address)`.
    ///
    /// Ordered by capacity first, so the smallest adequate empty space is one `range` probe from
    /// the request size; the serial breaks ties towards the **oldest** space, and the base names
    /// the space inside `retired`. This is what keeps a growth from scanning the chain: a growth
    /// event that has to allocate (or refuse) touches no entry at all.
    ///
    /// *Why it cannot go stale:* a retired space's occupancy is **frozen** — nothing allocates in
    /// a retired space — so a space that was empty when it was retired is empty for as long as it
    /// is retired, and one that held words never becomes empty. Membership is therefore decided
    /// once, at retirement ([`install`](Nursery::install)), removed when the space is re-adopted
    /// ([`readopt_retired`](Nursery::readopt_retired)) and cleared with the chain
    /// ([`install_collapsed`](Nursery::install_collapsed)).
    empty_retired: BTreeSet<(usize, u64, usize)>,
    /// Monotone retirement counter — the "oldest among equals" tiebreak of
    /// [`empty_retired`](Nursery::empty_retired).
    retired_serial: u64,    /// The covering interval of **every** space's reserved extent (`[base, limit)`), maintained
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
    /// The nursery capacity the policy derived for the live set the **last collection** of this
    /// chain measured: the young live set's target after a minor
    /// ([`HeapPolicy::young_minor_target_words`]) or the whole live set's after a fullsweep
    /// ([`HeapPolicy::young_target_words`]).
    ///
    /// `None` until the first collection: a fresh nursery has no measured live set, so its
    /// growths keep the plain curated rule. The growth seam has no live set of its own —
    /// [`grow`](Nursery::grow) only knows a request — so without this bound it could install a
    /// space the very next collection gives back; the remembered target caps every fresh space at
    /// `max(one_step, live_target)`, which bounds the installed capacity by the same rule the
    /// collection applies while still granting a genuinely large request its own `one_step` size.
    live_target_words: Option<usize>,
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
            empty_retired: BTreeSet::new(),
            retired_serial: 0,
            extent,
            live_target_words: None,
        })    }

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
    ///    Otherwise the candidate is at least a **doubling** (`policy.next_words(2 * capacity,
    ///    0)`). A nursery that has **collected** then narrows the candidate from **above** at
    ///    `max(one_step, live_target)` (its remembered live-set target, `live_target_words`): the
    ///    growth seam has no live set of its own, so it must not install a space the next
    ///    collection would have to give back. Finally the size is raised to
    ///    [`HeapPolicy::next_words_above`] of the current capacity — the "never the same size when
    ///    *growing*" rule (`erl_gc.c:310-338`) — so a growth **always** installs a space strictly
    ///    larger than the one it retires. That last step is what the live-set bound may never
    ///    defeat: the bound narrows the choice, it cannot shrink the space below the current one
    ///    and it cannot install a same-size space, so the chain can never stack equally sized
    ///    spaces (each event would otherwise gain a whole capacity while granting no room).
    ///    The bound also cannot refuse a request that fits (`one_step >= need_words`), and it still
    ///    grants a genuinely large request its own `one_step` size. Every size is bounded by
    ///    [`min_heap_words`](HeapPolicy::min_heap_words) through
    ///    [`next_words`](HeapPolicy::next_words) and by the growth table's ceiling.
    /// 4. **Refuse at the chain cap.** Once the chain already **retains** a space, a growth that
    ///    would take the summed capacity past
    ///    [`HeapPolicy::chain_cap_words`] yields the typed
    ///    [`BeamError::Alloc`](beam_common::BeamError::Alloc) — before anything is allocated or
    ///    retired. The cap is the same `YOUNG_SHRINK_FACTOR` (8) multiple the heap-size policy
    ///    treats as the acceptable keep band around a live-set target, so a growth can strand no
    ///    more capacity than a collection would have kept in place.
    /// 5. **Refuse at the ceiling.** A [`max_heap_words`](HeapPolicy::max_heap_words) (or
    ///    growth-table) ceiling that cannot reach the request — or that can reach it only by
    ///    installing a space no larger than the current one — yields the typed
    ///    [`BeamError::Alloc`](beam_common::BeamError::Alloc) and leaves the nursery untouched:
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
    /// the only size left to it — and when that is no larger than the space it would replace the
    /// growth is refused rather than installing a same-size space (a growth always makes room).
    /// `max_heap_words == 0` is "no upper bound", the crate's encoding
    /// for unbounded growth, and leaves the size of step 3 alone.
    ///
    /// # The chain cap bounds the RETAINED capacity
    ///
    /// A growth never moves a term, so every space it supersedes stays addressable and stays
    /// charged to [`capacity_words`](Nursery::capacity_words) until a collection collapses the
    /// chain. The C never has more than **one young block plus one bounded fragment**
    /// (`delay_garbage_collection`, `erl_gc.c:500-599`: one fragment sized to the actual need,
    /// accounted separately, with `heap_sz` unchanged and `F_FORCE_GC` set so the next collection
    /// folds it away). Step 4 is that bound, in capacity terms:
    ///
    /// * a chain holding **nothing** retained always grants the request — that is the C's one
    ///   fragment, and refusing it would make a legitimate large allocation impossible;
    /// * a chain that **does** retain something may only grow while the summed capacity stays
    ///   within [`HeapPolicy::chain_cap_words`]`(live_target_or_fresh_size)` — so after the
    ///   fragment, a further growth has to collect first, exactly as the C's `F_FORCE_GC` asks.
    ///
    /// The consequence is the property the seam was missing: `capacity_words()` is bounded by the
    /// **live set** (times the keep band, plus the one space the first growth had to install) and
    /// is **flat in the number of growth events** instead of one whole capacity per event.
    ///
    /// # What a refusal means
    ///
    /// Every refusal is decided **before** any space is allocated or retired, so a refused growth
    /// leaves the nursery byte-for-byte as it was: the same current space (same base, same bump
    /// pointer), the same chain length, the same summed capacity — and therefore the same
    /// [`retains_old_spaces`](Nursery::retains_old_spaces) answer, so the
    /// [`collection_debt`](crate::Collector::collection_debt) the host polls stays `true` and the
    /// caller is expected to **collect** (`Collector::collect`/`ensure_capacity` collapse the
    /// chain, after which the request fits again). A refusal is never a wrong answer, and the
    /// runtime host absorbs it: the allocation it was making simply stays in its own pool.
    ///
    /// A superseded space is never *freed* here, whatever its occupancy: a space that handed the
    /// mutator words may hold live terms and only a [collection](Nursery::collapse) proves them
    /// dead, so the runtime never frees a retired space outside `collapse` — even when its words
    /// are provably garbage, the host may hold machine roots that are unpublished at this point
    /// (which is also why a collection is not legal here). A space that never handed out a word is
    /// not freed either — it is [re-adopted](Nursery::readopt_retired)
    /// instead, which keeps `space_count` and the containment index meaningful.
    pub(crate) fn grow(&mut self, policy: &HeapPolicy, need_words: usize) -> Result<()> {
        if self.current.available_words() >= need_words {
            return Ok(());
        }
        // A retired space nothing was ever allocated in can serve this request itself: it
        // strands no live word, allocates nothing and consumes none of the capacity budget.
        if let Some(key) = self.empty_retired_for(need_words) {
            self.readopt_retired(key);
            return Ok(());
        }
        let capacity = self.current.capacity_words();
        // The plain next-size rule, floored by the capacity it must not undercut.
        let one_step = policy.next_words(need_words, 0).max(capacity);
        // Only a request that would hand back a space no larger than the one it replaces
        // needs the doubling.
        let mut target = if one_step > capacity {
            one_step
        } else {
            policy
                .next_words(capacity.saturating_mul(2), 0)
                .max(one_step)
        };
        // A nursery that has collected knows the live set's target, so its growths are bounded by
        // it: the seam has no live set of its own, and a space larger than that target is one the
        // next collection would immediately give back. `one_step` (>= the request, >= the current
        // capacity) keeps the bound from ever refusing a fitting request or shrinking the space.
        if let Some(live_target) = self.live_target_words {
            target = target.min(one_step.max(live_target));
        }
        // A growth must make room: never install a space the size of the one it retires. This is
        // the C's `erts_next_heap_size(size, 1)` (the "never the same size when growing" rule), and
        // it is applied **after** the live-set bound so that bound can only narrow the choice — it
        // can no longer discard the progress (which is what used to make every post-collection
        // growth stack one whole same-size space onto the chain).
        target = target.max(policy.next_words_above(capacity));
        if target < need_words {
            return Err(heap_exhausted(need_words));
        }
        // The ceiling bounds the chain's **summed** capacity, so the fresh space may only claim
        // the budget the chain has left. Refuse — before allocating or retiring anything —
        // when that budget cannot reach the request, or when it would leave the fresh space no
        // larger than the one it replaces.
        if policy.max_heap_words != 0 {
            let budget = policy.max_heap_words.saturating_sub(self.capacity_words());
            if budget < need_words {
                return Err(heap_exhausted(need_words));
            }
            target = target.min(budget);
            if target <= capacity {
                return Err(heap_exhausted(need_words));
            }
        }
        // The chain cap: the capacity this growth would leave **retained** must stay within the
        // keep band's upper edge. A chain holding nothing retained always passes (the C's one
        // bounded fragment); the cap is floored by `target` so a single large request is still
        // granted its own space.
        let cap = policy
            .chain_cap_words(self.live_target_words.unwrap_or(target))
            .max(target);
        if self.retains_old_spaces() && self.capacity_words().saturating_add(target) > cap {
            return Err(heap_exhausted(need_words));
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

    /// The **smallest** retired space that never handed the mutator a word
    /// (`used_words() == 0`) and is still large enough for a `need_words` allocation — the space
    /// [`grow`](Nursery::grow) can re-adopt instead of allocating a fresh one — as the
    /// [`empty_retired`](Nursery::empty_retired) key that names it.
    ///
    /// The smallest adequate space wins, and the set's ordering (capacity, then retirement serial)
    /// keeps the oldest among equals: the selection is deterministic, and it leaves the largest
    /// retired spaces available for a later, larger request. `None` when no retired space is both
    /// empty and large enough — the only case in which a growth has to allocate.
    ///
    /// **`O(log #spaces)`**, and it touches no entry on any other growth path: the chain is
    /// indexed by capacity at retirement rather than scanned per growth, so a growth loop is never
    /// quadratic in the number of retired spaces.
    fn empty_retired_for(&self, need_words: usize) -> Option<(usize, u64, usize)> {
        self.empty_retired
            .range((need_words, 0, 0)..)
            .next()
            .copied()
    }
    /// Re-adopts the retired space named by `key` as the current space, retiring the old current
    /// in its place — the step [`grow`](Nursery::grow) takes instead of allocating when a retired
    /// space is provably empty.
    ///
    /// Nothing is stranded (the re-adopted space never handed the mutator a word), nothing is
    /// allocated, and the chain's summed capacity and length are both unchanged. The chain's
    /// **set** of spaces is unchanged too — the same allocations, a different one of them
    /// designated current — so the aggregate covering cannot move and is left alone; only the
    /// geometry epoch advances, because the partition (and with it the role of every address:
    /// which space's `top` may still move) changed.
    ///
    /// The space is located by its base address (the stable identity the
    /// [`empty_retired`](Nursery::empty_retired) key carries), so the index needs no renumbering
    /// when one entry leaves the `retired` vector; this path is the only one that scans it, and
    /// it is at most one scan per productive re-adoption, never per growth event.
    fn readopt_retired(&mut self, key: (usize, u64, usize)) {
        let base = key.2;
        let index = self
            .retired
            .iter()
            .position(|space| space.base() as usize == base)
            .expect("the empty-retired index names a retired space");
        self.empty_retired.remove(&key);
        let space = self.retired.remove(index);
        self.retired_regions.remove(&(space.base() as usize));
        self.install(space);
        self.extent = self
            .extent
            .with_generation(self.extent.generation().saturating_add(1));
    }

    /// Installs `space` as the current space and retires the one it replaces, keeping the
    /// containment index ([`retired_regions`](Nursery::retired_regions)), the `retired` chain and
    /// the empty-space index ([`empty_retired`](Nursery::empty_retired)) in step. The allocator is
    /// untouched, and nothing is freed.
    ///
    /// This is the one place a space's emptiness is decided, and the decision is final: a retired
    /// space's occupancy is frozen (nothing allocates in it), so the entry the index gains here is
    /// exact for as long as the space is retired.
    fn install(&mut self, space: HeapSpace) {
        let old = core::mem::replace(&mut self.current, space);
        let base = old.base() as usize;
        self.retired_regions.insert(base, old.top() as usize);
        if old.used_words() == 0 {
            let key = (old.capacity_words(), self.retired_serial, base);
            self.retired_serial = self.retired_serial.saturating_add(1);
            self.empty_retired.insert(key);
        }
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
    /// collected, where `live_words` is what the round measured: the words it **promoted**
    /// (which is what it left alive in the young generation). The space is only replaced when
    /// the target differs; otherwise its bump pointer is merely rewound.
    ///
    /// A **fullsweep** passes the post-copy live set as well and therefore calls
    /// [`collapse_after_fullsweep`](Nursery::collapse_after_fullsweep) — the C applies a
    /// second, post-copy sizing rule to a fullsweep (`erl_gc.c:2062-2098`) that a minor never
    /// reaches.
    ///
    /// # The minor seam
    ///
    /// [`HeapPolicy::young_after_collection`] installs the live set's target whenever the capacity
    /// is below it or far above it, so `collapse` both grows a nursery that was too small for
    /// what the round promoted and shrinks one a dead burst over-sized. Both branches land on a
    /// capacity derived from the **live set**, so a fixed live set converges to a constant
    /// capacity instead of climbing one growth-table step per collection. The target is
    /// remembered ([`live_target_words`](Nursery::live_target_words)) so the growth seam
    /// ([`grow`](Nursery::grow)) is bounded by it too.
    pub(crate) fn collapse(&mut self, policy: &HeapPolicy, live_words: usize) -> Result<()> {
        let collected_capacity = self.capacity_words();
        let target = policy.young_after_collection(collected_capacity, live_words);
        let live_target = policy.young_minor_target_words(live_words);
        self.install_collapsed(target, live_target)
    }

    /// Collapses the chain after a **fullsweep**, sizing the nursery from the **whole-heap live
    /// set** the round kept ([`HeapPolicy::young_after_fullsweep`]).
    ///
    /// `live_words` must be the whole-heap post-copy live word count (young *and* mature): a
    /// fullsweep leaves no live young set of its own, so a nursery-only count would collapse the
    /// nursery to the reserve floor every round. A fullsweep therefore **may enlarge** the
    /// nursery — it is the one collection that measures the live set exactly, so it is where the
    /// growth decision belongs — and it shrinks a nursery the live set has outgrown back
    /// towards the target. That whole-heap target is also what the growth seam is bounded by
    /// afterwards ([`live_target_words`](Nursery::live_target_words)).
    pub(crate) fn collapse_after_fullsweep(
        &mut self,
        policy: &HeapPolicy,
        live_words: usize,
    ) -> Result<()> {
        let collected_capacity = self.capacity_words();
        let target = policy.young_after_fullsweep(collected_capacity, live_words);
        let live_target = policy.young_target_words(live_words);
        self.install_collapsed(target, live_target)
    }

    /// The shared body of the two collapse seams: install (or rewind to) a single space of
    /// `target` words, drop the retired chain, re-seed the aggregate extent and remember the
    /// `live_target` this round derived for the growth seam.
    fn install_collapsed(&mut self, target: usize, live_target: usize) -> Result<()> {
        self.retired.clear();
        self.retired_regions.clear();
        self.empty_retired.clear();
        if self.current.capacity_words() == target {
            self.current.reset();
        } else {
            let alloc = self.current.allocator().clone();
            self.current = HeapSpace::with_allocator(alloc, target)?;
        }
        self.live_target_words = Some(live_target);
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
