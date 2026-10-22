# `beam_engine` — the arena's reclaim tiers (`src/context/`)

## Intent
The two **additive**, **explicitly-invoked** tiers that give a `MinimalContext`'s region heap its words back:
Tier A — `MinimalContext::heap_rewind`, a proven-prefix rewind (`O(regions dropped)`, reads no word) — and
Tier B — `MinimalContext::reclaim_heap` / `reclaim_heap_with_capacity`, a copying collapse of the live graph
into one fresh survivor region.
Both are **HOST-driven**: nothing in this crate ever reclaims on its own — the allocation path never
truncates, resets or reallocates a region, `ensure_heap` stays a no-op, and an arena that never calls either
door behaves exactly as it did before these doors existed.
The motivation is a long-lived host arena (a tree-walking `-eval` evaluator's per-expression scratch graph)
that would otherwise retain every byte it ever handed out.
`MinimalContext`, `ContextHeap`, `HeapMark` and `ContextHeapStats` themselves live in `../context.rs`; this
node owns only the two tiers over them.

## API Surface (public; re-exported from the crate root and `prelude`)
- `MinimalContext::heap_rewind(&mut self, mark: HeapMark) -> Result<HeapRewind>` — Tier A.
- `MinimalContext::reclaim_heap(&mut self, forward: impl FnMut(&mut Relocator<'_>) -> Result<()>) -> Result<Option<HeapReclaim>>` — Tier B, self-sizing.
- `MinimalContext::reclaim_heap_with_capacity(&mut self, dest_capacity_words: usize, forward: impl FnMut(&mut Relocator<'_>) -> Result<()>) -> Result<Option<HeapReclaim>>` — Tier B with a caller-chosen destination capacity (the pre-copy refusal is a real path here).
- `HeapRewind { regions_dropped, words_released, live_words, reserved_words, committed_words }` — `regions_dropped` = regions strictly newer than the marked one (each carrier released by its `Drop`), `words_released` = their `used` figures plus the words the surviving frontier had handed out above the mark (exactly what is added to `ContextHeapStats::released_words`), then the arena's figures **after** the rewind.
- `HeapReclaim { regions_dropped, source_words, live_words, words_reclaimed, objects_copied, reserved_words, committed_words }` — `source_words` = `Σ used` of the old arena (the copy bound), `live_words` = what the forwarded roots reached (`= relocation.dest_words_used`), `words_reclaimed` = `source_words - live_words` (added to `ContextHeapStats::released_words`), `objects_copied` = boxed objects plus cons cells, and the survivor's own reservation/commit (both `0` when nothing was reachable).
- `ContextHeapStats::live_words` — `Σ regions[i].used()`: the words the regions hold **right now**, the one figure that falls on a reclaim; `used_words` stays **cumulative demand** and equals `live_words + released_words`.
- `ContextHeapStats::released_words` — the words the two tiers have taken back, summed over the arena's whole life; `0` until the first successful reclaim, and it never falls.
- `HeapMark::region() -> Option<usize>` / `HeapMark::base() -> usize` — the marked region's index (`None` for `HeapMark::EMPTY`) and its base **address**; with `used()` they are the whole public view of a mark (its `epoch` stays private).
- `HeapMark::EMPTY` is also the **arena-reset door**: an epoch-matching empty mark makes `heap_rewind` drop every region the arena holds.
- Driver-facing companions (unchanged by this node): `MinimalContext::for_each_heap_region` (byte-level census of every written prefix), `Machine::for_each_root_mut` (the writable root-set hand-off a Tier-B `forward` closure forwards through).

## The caller contract (this is what makes either tier sound)
The arena cannot tell a term word from raw payload — a region's prefix is tagged term words *and* raw bytes — so both tiers are **purely mechanical**: they never decide what is live, they act on a claim the caller makes.
- **Tier A requires: no reachable term lies at or above `mark`** (neither in the frontier above the marked `used` figure nor in any region the rewind drops).
  Every root the caller still holds must address words below the mark or outside the arena **before** the call, and nothing may hold a raw `*mut Eterm` into the dropped words afterwards.
  A violated claim is a use-after-free, exactly as in `beam_alloc::Heap::reset`.
- **Tier B requires: the roots the `forward` closure hands the relocator ARE the whole live set**, and every word the copier walks is a well-formed term structure (any `LIST`/`BOXED` word whose address falls inside the arena must address a real object header there; raw payload is fine wherever the canonical child-slot walk does not visit it).
  A **forgotten root is silently dropped** — the arena is not a collector, it has no way to notice.
  `forward_roots` is idempotent, so several root slices may be forwarded (a `Vec<Eterm>` plus a `Machine::for_each_root_mut` pass, for instance); the roots are rewritten in place and must outlive the call.
- Prefer Tier A when the caller can order its allocations (a scratch window whose roots are all below the mark); use Tier B when live terms are interleaved with garbage or spread over several regions.

## Failure and refusal semantics (every arm typed, none silent)
- `heap_rewind` refuses with `BeamError::BadArg` and leaves the arena **byte-identical** for a mark that is **pre-reclaim** (its epoch differs), **stale** (a region index the arena no longer has, or a base address that is not the region's — a replaced region, or another arena's mark) or **forward** (it claims more words than the marked region handed out).
- `reclaim_heap_with_capacity` refuses with the typed `heap_exhausted` (`BeamError::Alloc`) **before the first copy** when the destination cannot hold the copy bound: no forwarding marker is written, no root is rewritten, and the untouched destination is dropped with the frame.
  The preflight compares against the exact upper bound `Σ used`, not the (unknowable before the roots arrive) live volume, so a capacity between the live volume and the total source volume is refused too — conservatively, and for the reason that matters: refusing up front instead of tearing a copy.
- `Ok(None)` (no state change, **no epoch bump**) when the arena holds no written word, or when it holds a single region whose `used` already fills its whole reservation.
- An `Err` from the caller's own `forward` closure — or an unexpected mid-copy refusal — is **unrecoverable** (`beam_gc` leaves the source partially forwarded): it is propagated and the arena is left in its defined **empty** state (every region dropped, the epoch bumped, the words accounted as released), so the host must abandon the arena's words **and its own roots**, which name freed memory.
- Both success paths bump the arena's `epoch`, so **every** mark taken before the call stops matching `heap_mark_matches` and is refused by `heap_rewind` — including the mark a rewind consumed and `HeapMark::EMPTY` (whose epoch is a fresh arena's `0`).
- A refusal never bumps the epoch, so a refused call leaves the caller's marks usable.

## Geometry notes (why the reports say what they say)
- **Only `Drop` gives a region's bytes back.** `beam_alloc::ReservedHeap::reset` merely moves the bump pointer; a region's whole reserved block (one anonymous, lazily-faulted carrier `mmap`) is released in `Drop` and nowhere else.
  So Tier A releases a carrier only for the regions it *drops* — a rewind inside the surviving frontier keeps that region's carrier, reservation, committed limit and peak and releases address space but no RSS — and Tier B releases every old region's carrier.
- **The survivor is reserved at the copy BOUND, not the live volume.** The relocator writes each copy's destination address into every copied child slot (its marker protocol knows nothing about a later move), so a copy made in a scratch region and installed with a memcpy would leave every interior pointer aiming at the freed scratch.
  The copies are therefore deposited **straight into the survivor**, which starts life as the copy destination and becomes the survivor; the price is `reserved_words == source_words`, which is still a fall (every old carrier is released) because a reservation is lazily faulted and RSS tracks only the words actually touched.
- `reclaim_heap` sizes the destination itself at `Σ regions[i].used()` — an exact upper bound, since each source word is copied at most once — so it never refuses for want of capacity; a genuinely huge arena can still fail with `beam_alloc`'s typed `Alloc` when the survivor is reserved.
- `reclaim_heap_with_capacity`'s capacity is also the survivor's **reservation**, committed in full, which is why the preflight must bound the copy volume exactly and why the next allocations never have to grow it.
- Tier A's cost is `O(regions dropped)` bookkeeping plus one bump-pointer move; Tier B's is one Cheney copy of the live set.
- Neither tier changes the allocation policy, the sizing rule, or `alloc_words`/`alloc_words_rooted`/`ensure_heap`/`heap_available`.

## The dependency edge (`beam_engine` → `beam_gc`)
Tier B drives `beam_gc`'s published relocation API — `FromSet`, `LinearDest`, `Relocator`, `Relocation` — and nothing else from that crate.
The edge is directionally legal: `beam_gc` is wave 1 and `beam_engine` wave 4 in `../../CONTEXT.md`'s layering table, and `beam_gc` depends only on `beam_common`/`beam_term`/`beam_alloc`/`libc`, never on `beam_engine`, so the DAG stays acyclic (`cargo tree -p beam_engine` shows no cycle).

## Constraints
- `unsafe` is confined to `reclaim.rs` (module-local `#![allow(unsafe_code)]` under the crate root's `#![deny(unsafe_code)]`): it builds `beam_gc::FromSet`/`LinearDest` over the arena's own reservations and advances a bump pointer to a validated position; every `unsafe` block carries a `// SAFETY:` rationale and `beam_gc`'s `# Safety` contracts are the two caller assertions above.
- Files ~1000 lines; all fallible APIs return `beam_common::Result<T>` (`BeamError` only, never a crate-local error); no `unwrap()` on runtime data, and a wrong result is never substituted for a refusal.
- Generated data, the frozen term layout and the engine's execution semantics are untouched: this node adds doors, it changes no instruction.

## Test Strategy
- `reclaim/tests.rs` (module tests, 8 rows) drives `ContextHeap::with_geometry` with exactly-known 4-/8-word reservations, which is what lets every word figure in a `HeapRewind`/`HeapReclaim`/`ContextHeapStats` be asserted **exactly**; being a descendant module it also fabricates the marks no public door can build (forward, replaced-region, foreign).
- `tests/heap_reclaim.rs` (7 rows) re-proves the same contracts through the **public** doors at the **production** geometry (`HEAP_RESERVE_WORDS` 4 MiB / `HEAP_COMMIT_WORDS` 64 KiB, a second region only past a bulk request): both tiers end to end, the reachable refusal arms, the reset door, the two `Ok(None)` arms, the pre-copy capacity refusal, and one `Machine`-rooted forward through `Machine::for_each_root_mut` — the exact call pattern a driver uses.
  Its file doc records the measurement caveat: no unit test measures RSS, so the region-count and reserved-words assertions **are** the memory claim, because `reset()` frees nothing and only `Drop` releases a carrier.
- Run both with `scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_engine` (`--test heap_reclaim` for the suite alone).

## Routing Table
- `reclaim.rs` → the two tiers: `HeapRewind`/`HeapReclaim`, `ContextHeap::{rewind,reclaim,abandon,live_words}`, and the two `impl MinimalContext` doors.
- `reclaim/tests.rs` → the exact-number module suite (also the only place a fabricated mark is driven).
