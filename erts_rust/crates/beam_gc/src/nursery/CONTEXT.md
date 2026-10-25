# `beam_gc` — Young Nursery Chain (`src/nursery/`)

## Intent
The young nursery as a **chain of spaces**: the no-relocation growth seam, the chain ceiling, the empty-space
re-adoption index, the indexed containment and the cached scanned-extent aggregate.
The implementation is the sibling module `../nursery.rs`; this node owns the chain's contracts plus its
in-module suite (`tests.rs`, the growth contract, re-adoption, the containment index and the aggregate extent
incl. a directly installed long chain).
`../../CONTEXT.md` (crate) owns the seam shape and the measured headlines, `../CONTEXT.md` (implementation node)
owns the design decisions and the forensics rules; this file owns what is specific to the chain.

## Chain rules
- **`grow_nursery` is a NO-RELOCATION seam.**
  `HeapSpace` never reallocates, so growing by replacement would strand every live word; `Nursery::grow` retains
  the old space and points the allocator at a fresh one, so every live word keeps its **original address**.
  The step is the plain `next_words(need_words, 0)` whenever that already enlarges the space, and **at least**
  `next_words(2 * capacity, 0)` otherwise — so a caller growing repeatedly at a constant request climbs the
  growth table geometrically and the chain stays `O(log(heap))` long.
- **A nursery that has collected narrows the fresh space from above.**
  `min(target, max(one_step, live_target_words))`, with `one_step = next_words(need_words).max(capacity)`,
  `target` the plain curated step (at least a doubling) and `live_target` the target the last collapse recorded.
  So the seam can neither refuse a request that fits (`one_step >= need_words`) nor install a capacity the next
  collection would immediately give back, while a genuinely large request still gets its own `one_step` size.
  A fresh nursery, with no measured live set yet, keeps the plain rule.
  The live target is recorded by **both** collapse seams (`HeapPolicy::young_after_collection` from the promoted
  words, `young_after_fullsweep` from the whole-heap live set), which is also what makes each collapse a fixed
  point of its own rule rather than a ratchet.
- **Strict progress is applied LAST, so nothing above can defeat it.**
  `target = max(target, policy.next_words_above(capacity))`: a growth **always installs a strictly larger space
  than the one it retires** — the C's "never the same size when *growing*" rule
  (`erts_next_heap_size(size, 1)`, `erl_gc.c:310-338`) — so the chain can never stack equally sized spaces,
  each of which would append a whole capacity while granting no room.
  `next_words_above` is `next_words(size, 1)` completed to a total function (`size + 1` past the growth table's
  top), and only the two legitimate ceilings — the table's own limit and `max_heap_words` — can still refuse,
  both before anything is allocated or retired.
- **The consequence for a pre-empted burst is a chain of strictly increasing spaces** — every growth one table
  step above the space it retires, held near the remembered live target while the burst is small — rather than
  one space grown in place: the retained words stay the burst's own allocation, the cost is the space count, and
  a collection collapses the chain back onto the live-set target (`../../MEASURED.md`, "The CLI flatness probe
  and the burst residual").
- **The chain's length is the bounded quantity and its retained bytes are the ladder's geometric sum**,
  `O(requirement)`, not a function of the call count; a collection collapses the chain to exactly ONE space, so
  `young()`/`young_mut()` keep returning the current allocatable space while `young_contains` covers every space.

## The ceiling and the empty-space index
- **The ceiling bounds the chain, not one space.** `max_heap_words` is the process's heap budget (the C `+hmax`)
  and the chain *is* the young heap, so it is enforced against `capacity_words()`: a fresh space may claim only
  the remaining budget, and a request that budget cannot cover is the typed `heap_exhausted`, decided before
  anything is allocated or retired.
  `max_heap_words == 0` (the default) is "no cap", bounded only by the growth table's last entry (just under
  `(1<<53)/8` words); a policy whose `initial_young_words` already exceeds the ceiling starts above it and every
  growth is then the same typed refusal.
- **A provably empty space is re-adopted rather than re-allocated.** A retired space with `used_words() == 0`
  holds no term, so it is re-adopted as the current space — the smallest adequate one — which allocates nothing,
  moves nothing and consumes no budget.
  The chain's *set* of spaces is unchanged, so `extent` cannot move, but the geometry epoch advances because
  *which* space is current is part of the cached geometry.
- **The selection is `O(log #spaces)`, not a chain scan.** Retirement indexes every empty space in
  `empty_retired`, a `BTreeSet<(capacity, serial, base)>`, so `empty_retired_for(need_words)` is one range query
  (the smallest adequate capacity, oldest among equals, via the retirement serial) and `readopt_retired` removes
  exactly that key.
- **Containment over the chain is indexed, not scanned.** `Nursery::contains` answers from an ordered index of
  the retired spaces' live regions (`retired_regions: BTreeMap<base, top>`) plus one direct test of the current
  space, so it is `O(log #spaces)`; the intervals are pairwise disjoint, an indexed `top` is frozen from
  retirement, and the only space whose `top` moves is tested directly.
  That keeps the seam sound for a caller holding **no root set** — the un-rooted `beam_rt` allocation path,
  which must not collect.

## The cached scanned-region aggregate
- `Nursery` keeps `extent: ScannedBounds` over the **reserved** extents of every space (not the moving `top()`,
  which moves on every allocation).
  `grow` only *widens* it (`O(1)` regardless of chain length) because the retired space was already covered
  while current, and a collapse re-seeds it from the surviving space; both advance the epoch.
  `Collector`/`GenImmixCollector` compose it with their mature extent and a `mature_generation` counter bumped
  where the mature space is actually replaced, the reported `generation` being the **sum** of the two monotone
  counters.
- Tightening the aggregate is not worth implementing: the covering equals the union of the scanned regions to
  within one block except after a collection, where a per-space interval list or a nursery-vs-mature split is a
  lateral trade — what falls through is mature- and retired-space pointers, which every interval shape must fall
  through on (`may_contain` is a reject filter by contract) — so since `beam_rt` re-reads a freshly built
  `ScannedBounds` per reference word, hoisting one read per walk (a parameter at this node) is the cheaper lever.

## Known gaps (this node)
- **A retired nursery space is freed only by a collection; a provably empty one is re-adopted instead.**
  `Nursery::grow` supersedes the space the mutator was allocating in without collecting and without moving a
  term, so a live term may still name into it: it stays addressable as a chain link and only a collapse frees it.
- **The chain's collection debt is an opt-in seam; `should_collect` stays occupancy-only.**
  `Nursery::retains_old_spaces` is exported as `collection_debt() -> bool` on all three collector types; `true`
  means *a collection is required to reclaim the chain*, and the retention is then **one space at most**,
  however many cycles run (measured: 400 grow+admit cycles → peak chain **2** spaces, peak *retired* **15 143**
  words on Copying / **8 980** on Immix, against a peak current nursery of **46 422** / **37 670** words; the
  converged tail is **1** space / **0** retained and the mature capacity settles at **17 731** / **4 352** words
  — `../../MEASURED.md`, "The debt signal and the collection").
  Folding it into `should_collect` would tax **every** admission, which is why it is a separate accessor.
- **A refused growth is inert.** The two ceilings are decided before anything is allocated or retired, and
  `tests/heap_growth.rs::ensure_capacity_reports_a_typed_error_when_growth_is_capped` pins the capped half at
  `initial_young_words: 51` with `max_heap_words: 233`: a 4 096-word request is the typed `out_of_memory` and
  leaves the 51-word space and the chain's summed capacity untouched, while a 64-word request collects a minor
  and installs the 90-word `next_words(64)` step.
  The refusal is honest — the ceiling bounds the chain's summed capacity and a 51-word space cannot host a
  4 096-word request — and a policy that wants the request to fit must give the chain the room (or
  `Nursery::grow` would have to free a provably empty current space instead of retiring it, which is not
  implemented).

## Observables (forensics)
- **Address-level membership is public; enumeration of the chain is NOT.** From outside the crate a caller
  reaches only the current space (`young()`), a space count (`young_space_count()`) and a membership test over
  every space (`young_contains(ptr)`); the chain itself is `pub(crate)`, while the mature side is fully
  addressable (`mature_base`/`mature_capacity_words`/`mature_used_words`/`mature_contains`).
- **`young()` is the CURRENT space; every "nursery" figure in `HeapInfo` is CHAIN-SUMMED** —
  `young().capacity_words()` is the current space only, while `heap_size_words()`/`HeapInfo::heap_size` is
  `Nursery::capacity_words()`; they agree only while `young_space_count() == 1`.
- **Address stability:** a `grow_nursery` always installs a new `HeapSpace` (so `young().base()` changes); a
  collapse keeps the base when the target capacity equals the current one and installs a new space only when it
  differs.
- **`should_collect` is occupancy-only and ignores the chain, so a retired space is visible only through
  `collection_debt()`.** `should_collect(need)` reads the current space alone, and a growth guarantees that
  room, so it cannot be true right after a growth that satisfied the same `need`.
  `collection_debt()` is `!retired.is_empty()`, set by every `Nursery::grow` that replaces a space and cleared
  only by a collapse, so the runtime's pairing (`collect_or_grow` / `Process::deliver`) makes
  `young_space_count` oscillate ≈`1`→`2`→`1`.
  `retired.clear()`/`retired_regions.clear()`/`empty_retired.clear()` exist in exactly one place,
  `Nursery::install_collapsed`, and `retired.push` in exactly one, `Nursery::install`.
- **Reading the counters for a leak-vs-recycle diagnosis.** `heap_size_words()` is the summed capacity of the
  chain, so a value constant across N iterations proves no uncollected retirement is outstanding without reading
  `collection_debt()`; no counter advances on the `grow_nursery` path except `young_space_count()`,
  `collection_debt()` and that summed `heap_size_words()`.
- **Per-event allocations of the growth seam:** a fresh space plus inserts into `retired_regions`/`empty_retired`
  (`BTreeMap`/`BTreeSet` node allocations), or one `O(log #spaces)` re-adoption that allocates nothing.

## Routing Table
- `tests.rs` → the chain's in-module suite: the growth contract (strict progress, geometric growth, the
  live-target narrowing), re-adoption, the indexed containment, and the cached extent incl. a directly installed
  long chain
- `../nursery.rs` → the implementation (`Nursery`: `grow`/`collapse`/`collapse_after_fullsweep`/`spaces`/
  `contains`/`add_to_from_set`/`used_words`/`capacity_words`, `retired`/`retired_regions`/`empty_retired`,
  `extent`/`live_target_words`, `retains_old_spaces`)
- `../../tests/heap_growth.rs` → the out-of-crate growth gate, incl.
  `ensure_capacity_reports_a_typed_error_when_growth_is_capped` (the capped half)
- `../policy.rs` → `HeapPolicy`'s sizing rules the chain reads (`next_words`, `next_words_above`,
  `young_after_collection`, `young_after_fullsweep`, `young_target_words`, `young_minor_target_words`,
  `nursery_floor_words`)
- `../bounds.rs` → `ScannedBounds`, the type the chain's cached aggregate is
