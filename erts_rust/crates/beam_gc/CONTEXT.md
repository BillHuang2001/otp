# `beam_gc` — Per-Process Garbage Collector (Stage 4)

## Intent
Owns the **per-process collector** of the Rust ERTS rewrite: a **generational semi-space Cheney copying**
collector over the per-process heap, plus the off-heap **refcounted large-object (MSO)** interface and the
process **heap-size policy**.
It is **barrier-free by construction**: terms are immutable ⇒ *no term on the mature heap may refer to a term
on the young heap*, so the mutator pays zero per-store overhead.
Reproduce the collector semantics of the legacy C (`../../../erts/emulator/beam/erl_gc.c` + `copy.c`)
**without** its C structure; the design source is `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D and the Stage-4 row
of §H.
Consumed by `beam_process`, which drives collections at safepoints (wave 1); it depends downward only on
`beam_common`, `beam_term`, `beam_alloc` and `libc`.
Honours the crate-layer contracts of `../CONTEXT.md`: the root-set contract (no frame liveness maps), the frozen
term layout (§2) and the return-based error strategy (§3).
MMTk/Boehm are **rejected**; GenImmix is a **measurement-gated** mature-generation option (`immix/` +
`immix_collector.rs`) selected by the **measurement-derived automatic rule** (`selection.rs`).

## API Surface (the seam shape a dependent designs against)
Everything is re-exported at the crate root and via `beam_gc::prelude::*`.
**The rustdoc in `src/` is authoritative for every signature and its full contract** — this section gives names
and the one-line semantic contract only.

### `space.rs` — copying semi-spaces
- `HeapSpace` — one copying semi-space: a thin wrapper over `beam_alloc::Heap` adding *space* semantics
  (`contains`/`contains_term`/`reset`/`base`/`top`/`limit`, capacity/used/available words); it **never
  reallocates**, so growth is by GC.
- `FromSet` — the from-space ranges a collection evacuates (one for a minor, nursery + mature for a fullsweep):
  `new`/`add(&HeapSpace)`/`contains`, with the ranges kept **sorted by `lo`** so a per-reference `contains` is
  one binary search, not a scan.

### `copy.rs` — the Cheney copy engine (the unsafe core)
- `Copier<'a>` — `new(from, to)`, `forward_roots(&mut [Eterm])`, `complete() -> Result<GcStats>` (the Cheney
  scan), `run(roots)` = forward + complete; and
  `collect(from, to, roots) -> Result<GcStats>`, the primitive young-generation entry over a single from-space.
- The shared `pub(crate)` evacuation primitives (`Target`, the `EvacDest` trait, `forward_slot`, `move_size`,
  `for_each_ref`) consumed by `Copier`, `immix/`, the atomic promotion and `live.rs`; **per-object layout is
  not this crate's** (Design Decisions).

### `live.rs` — the exact-live counter
- `pub(crate) live(from, roots) -> Live` and `live_full(nursery, mature, roots) -> Live`;
  `Live { words, lines }` where `words` is exactly the `GcStats::words_copied` a copy would report and `lines`
  is the per-object sum of line-granular charges.
- It counts the **exactly reachable** from-set graph **without mutating it**, driving the copier's own
  `move_size`/`for_each_ref`; `words` is the **whole-heap** post-copy live count a fullsweep hands to
  `Nursery::collapse_after_fullsweep` (Design Decisions).

### `reachable.rs` — the read-only whole-heap walk
- `ReachableObject<'a>` — the **safe view** of one reachable boxed object (`header()`, `words()`, `word(i)`,
  `len()`/`is_empty()`, `address()`), so a caller holds no raw pointer.
- `pub(crate) walk_reachable` / `for_each_reachable_view` / `for_each_reachable` — the **single** read-only,
  iterative whole-heap walk both `live.rs` and the public enumeration are built on: every reachable **boxed
  object** exactly once, cons cells traversed but not reported, **layout-agnostic**, no forwarding marker, dedup
  by first-word address, every reference outside `from` skipped.

### `collector.rs` — the two-generation collector
- `CollectionKind { Minor, Major }`; `Collection { kind, stats: GcStats }`.
- `Collector` — owns the young nursery **chain** (`src/nursery.rs`), a mature `HeapSpace` and the `HeapPolicy`:
  `new`/`with_allocator`, `policy`, `young`/`young_mut` (the **current**, allocatable space), `mature`, `stats`,
  `minor_gcs` (minors since the last fullsweep, the C `GEN_GCS`), `impl HeapObservable` — all mirrored on
  `GenImmixCollector`/`AnyCollector`.
- `should_collect(need_words)` — the **occupancy-only** admission signal (`current().available_words() <
  need_words`); `collection_debt()` — the **opt-in** debt seam over `Nursery::retains_old_spaces`.
- `collect(roots) -> Result<Collection>` picks minor vs major by policy and escalates a would-not-fit minor to a
  major; `collect_minor`/`collect_major(roots) -> Result<GcStats>` each **pre-check capacity**, so an unfittable
  destination returns the typed `heap_exhausted` *before* any copy.
- `collect_kind(kind, roots) -> Result<Collection>` — the **pinned** counterpart of `collect`: it runs exactly the
  kind it is given and **never escalates**, so a pinned `Minor` whose promotion does not fit is the typed
  `heap_exhausted` instead of a fullsweep (mirrored on `GenImmixCollector`, forwarded by `AnyCollector`).
- `ensure_capacity(need_words, roots)` — the on-demand growth seam; `grow_nursery(need_words)` — the
  **no-relocation** growth seam.
- `young_contains(ptr)` — whether `ptr` is a word of **any** nursery space (current or growth-retired), the range
  test the host's unscanned-root valve uses; `young_space_count()` — the chain length.
- `for_each_reachable_object(roots, visit)` / `for_each_reachable_object_view(roots, visit)` — the public
  read-only whole-heap enumeration (the off-heap liveness scan's marking half): each reachable boxed object once,
  as a raw `(header, ptr)` pair or as a safe view.
- After **every** collection the chain is collapsed to one space: a minor via
  `Nursery::collapse(policy, demand)`, a fullsweep via `Nursery::collapse_after_fullsweep(policy, observed,
  live)` — the C's **two-step** rule (Design Decisions).

### `immix/mod.rs` + `immix/collect.rs` — the mark-region mature space
- `ImmixSpace` — line/block mark-region mature generation; geometry `LINE_WORDS` 16, `LINES_PER_BLOCK` 16,
  `BLOCK_WORDS` 256; per-block `allocated`/`marked`/`kept` bitmaps; `BlockState::{Free,Partial,AllMarked}`.
- Geometry/allocation seam: `new`/`with_allocator`, `base`/`contains`/`contains_term`, the
  capacity/block/line/used/free word counters, `counters() -> ImmixCounters`, `reset()`,
  `unsafe alloc_words(words)` (line-aligned), `cons`/`tuple`, `can_reserve`, and the
  `release_words`/`release_lines` undo primitives (for undoing a *promotion's own* reservations only).
- `mark(roots, promoted)` and `collect(&mut self, roots, promoted) -> Result<ImmixStats>` — mark, classify,
  release, evacuate `Partial` blocks, pin `AllMarked`, finalize; `impl EvacDest for ImmixSpace` lets the shared
  `copy.rs` protocol evacuate *into* it.
- `ImmixStats` (objects/lines evacuated and pinned, blocks/words reclaimed, words pinned) and `ImmixCounters`.

### `immix_collector.rs` — `GenImmixCollector`
- `GenImmixCollector` — the copying young `Nursery` chain paired with an `ImmixSpace` mature;
  `with_config(&GcConfig)` (uses `cfg.policy()`; the `MatureKind` is ignored — this type *is* the Immix
  collector), `from_policy(policy)`, `with_allocator(alloc, policy)`.
- The same call set as `Collector`, plus `last_immix_stats() -> Option<ImmixStats>` (the most recent major's
  in-place reclaim accounting); its reachability enumeration differs only in the mature range, the whole
  `base .. base + capacity` region with its free lines included (over-inclusive yet harmless, as in `live_full`).
- A fullsweep that cannot fit its promotion grows the region by relocation (`grow_major`, which also rebuilds a
  fresh region at the **current** capacity when a ceiling forbids growing); only a `max_heap_words` cap that
  cannot hold the live set in any layout returns the typed `heap_exhausted`, and still **before any copy**.
- `collect` escalates a would-not-fit minor to a fullsweep and `run_major` falls back to `grow_major` on the
  same outcome — the promotion itself is atomic, so a refusal is a no-op to retry (Known Gaps).

### `any_collector.rs` — the strategy-dispatching seam
- `AnyCollector { Copying(Collector), Immix(GenImmixCollector) }` — a **complete drop-in for the concrete
  `Collector`**: the whole call set a host makes (including the kind-pinned `collect_kind`), forwarded with the
  selected strategy's semantics, so `young_contains` covers every space of the chain and the reachability
  enumeration is **strategy-agnostic**.
- `new(policy)` (= the **automatic rule** decides) / `from_config(&GcConfig)` (dispatches on the **resolved**
  `config.mature()`), `kind() -> MatureKind`, `mature_choice()`, `heap_info() -> HeapInfo`.
- The two mature types differ, so the mature generation is exposed as **geometry accessors** rather than a
  common `&`: `mature_capacity_words`/`mature_used_words`/`mature_available_words` (mark-region figures are
  line-rounded — "at least this many words") and `mature_base`/`mature_contains`.
- `mature_contains` **reuses each concrete space's own range test** — the copying space its live region
  `[base, top)`, the mark-region space its whole region including free lines (**over-inclusive**, harmless: a word
  the space never hands out cannot be a live reference's target).
- Forwarding only — the seam owns no collection logic, so neither strategy depends on the other.

### `config.rs` + `selection.rs` — the mature-generation decision
- `GcConfig { policy, selection }` — the heap-size policy plus **how** the mature strategy is chosen:
  `new`/`auto` (both = the automatic rule), `copying(policy)`/`immix(policy)` (explicit pins), `growable()`
  (= `auto(HeapPolicy::unbounded())`), plus `with_selection`/`with_mature`, `policy`, `selection`,
  `mature() -> MatureKind` and `mature_choice()`.
  `Default` = `GcConfig::new(HeapPolicy::default())` = `Auto`, which resolves to `Copying` for the default
  233-word policy.
- `MatureSelection { Auto, Explicit(MatureKind) }` (default `Auto`) — the *how*; `MatureKind` is the
  *strategy*; `Explicit` beats the rule in both directions.
- `MatureSource { Automatic, Explicit }`; `MatureChoice { kind, source, mature_words, break_even_words }` with
  `rule_kind`/`is_override`/`describe` — the **observable** resolved choice.
- `IMMIX_BREAK_EVEN_WORDS: usize = 8 * BLOCK_WORDS` (= 2048) and `IMMIX_BREAK_EVEN_BLOCKS` (= 8);
  `select_mature(mature_words)` (Immix iff `>=` the break-even — the pure rule), `mature_words_of`,
  `resolve_mature`, `choice_for`.

### `policy.rs` — heap-size policy (the `erts_next_heap_size` analogue)
- `HeapPolicy` — `min_heap_words`, `max_heap_words` (0 = unbounded), `fullsweep_after` (65535),
  `min_bin_vheap_words` (46422 = `next_heap_size(32768, 0)`, the C `BIN_VH_MIN_SIZE`), `initial_young_words`
  (233), `initial_mature_words`, `need_reserve_words`; `default()`, `from_min_max`, `unbounded()`, builder.
- `from_spawn_opts` — the C `spawn_opt` rounding through `erts_next_heap_size(x, 0)`, so a stored minimum is a
  growth-table fixed point; `fullsweep_due(minor_gcs)` — `minor_gcs >= fullsweep_after` (the C
  `GEN_GCS >= MAX_GEN_GCS`), with `0` forcing a fullsweep on every collection.
- Sizing rules: `next_words`; `size_after_fullsweep`; `mature_after_fullsweep(capacity, live, need)`
  (grow/shrink/keep — the C `adjust_after_fullsweep`); `young_after_collection(capacity, demand)` (the
  **minor's** bounded rule); `young_after_fullsweep(capacity, observed, live)` (the C's **two-step** fullsweep
  rule); `should_grow`, `should_shrink`.
- `DEFAULT_MIN_HEAP_WORDS: usize = 233` (the C `H_DEFAULT_SIZE`); `heap_sizes()` — the curated growth table
  (Fibonacci+1 to index 23, then +20%, stopping before the 64-bit cap `2^50` words) — and `next_heap_size`.

### `heap_info.rs` — the `garbage_collection` observables
- `HeapInfo { minor_gcs, fullsweep_after, min_heap_size, min_bin_vheap_size, max_heap_size, heap_size,
  total_heap_size }` — the `erlang:process_info(Pid, garbage_collection)` item (`new`, `values()`, `items()`,
  `of(&collector)`); `HeapObservable` — `minor_gcs`/`heap_size_words`/`total_heap_size_words`/`heap_policy`,
  implemented by `Collector`, `GenImmixCollector` and `AnyCollector`.
- `GARBAGE_COLLECTION_KEYS` — the oracle's key order (`max_heap_size`, `min_bin_vheap_size`, `min_heap_size`,
  `fullsweep_after`, `minor_gcs`), defaults `0`/`46422`/`233`/`65535`.
  `heap_size` = the nursery capacity (C `HEAP_SIZE`); `total_heap_size` = nursery + mature capacity (C
  `heap_sz + old heap`); the message buffer is `beam_message`'s.

### `mso.rs` — off-heap refcounted large objects & refc binaries
- `RefCount` — **re-exported** from `beam_alloc`, the workspace's single `erts_refc_*` primitive (this crate
  defines no count of its own); `Mso` / `MsoHandle` — an off-heap immutable refcounted payload and its clonable
  owning handle (`Clone` bumps the count, the last `Drop` runs the destructor); `OffHeapEntry { Mso, Binary }`
  — an entry holding either kind, with `overhead_bytes`/`apparent_size`/`as_mso`/`as_binary`.
- `OffHeap` — the per-process list (`MSO(p)`), **slot-stable for the list's whole lifetime**: `push`,
  `insert -> usize` (the slot an on-heap off-heap reference stores in its `val` word), `get(slot)`, `iter()`,
  `len`/`is_empty`/`overhead_bytes` (live entries only), and `sweep(is_live) -> usize`, which releases the dead
  entries **in place**. A released slot stays in place as `None`, so `sweep` never shifts, reorders, compacts or
  reuses a slot.

### `bounds.rs` — the O(1) aggregate scanned-region filter
- `ScannedBounds` — the covering half-open interval `[base, limit)` of **every** live scanned region of a
  collector (the whole nursery chain **and** the mature generation) plus a monotone `generation` epoch: `new`/
  `at`, `EMPTY`, `base`/`limit`/`generation`/`is_empty`, `may_contain`, `definitely_unscanned`, `span_words`.
- **Semantics: a fast-REJECT filter, never a containment claim.** `may_contain == false` ⇒ the address is in *no*
  scanned region; `may_contain == true` ⇒ nothing is known (it spans the holes between spaces, every unallocated
  tail and the mark-region free lines) and the caller must fall through to the exact tests. A fast accept would
  be a memory-safety bug.
- Built from each space's **reserved** extent, never the moving `top()`, so a cached value cannot go stale;
  exposed as `scanned_bounds()` on all three collector types (`O(1)`, allocation-free, safe Rust), consumed by
  `beam_rt/src/context/gc.rs::root_is_unscanned`.

### `stats.rs` / `error.rs`
- `GcStats` — `words_copied`, `words_reclaimed`, `objects_copied`, `minor_collections`, `major_collections`.
  `error.rs` holds `pub(crate)` allocation-failure constructors only (no new error enum).

## Design Decisions
- **Two generations: a young nursery + `mature`, with a minor tenuring young→mature (immediate promotion).**
  A minor Cheney-copies every live young term straight into `mature` and resets the nursery.
  Terms are immutable, so a mature object can never acquire a pointer to a later-allocated young object: the
  *no-mature→young-pointer* invariant holds after every minor and mature objects are **never rescanned** — the
  generational payoff.
  A major (fullsweep) evacuates young **and** mature into one freshly-sized space (the new `mature`), resetting
  the nursery — the C `major_collection`/`full_sweep_heaps`.
- **The destination is sized and checked up front — no partial-copy rollback — and the fresh mature space is
  sized from the EXACT reachable live set plus the young heap's occupancy, never the mature occupancy.**
  A minor runs only when `mature.available_words() >= young.used_words()` and the policy allows it; otherwise
  `collect` escalates to a major, which sizes `mature_after_fullsweep(capacity, live, need)` up front — `live`
  from `live.rs`, `need` = the young heap's occupancy (the C `adjust_after_fullsweep` `need_after`) — *before*
  copying, so a collection never leaves the from-space half-forwarded.
  The young occupancy lets the mature region receive the **next** nursery's promotion, so minors actually run
  instead of every collection escalating, and it is bounded by the nursery capacity, so the grow branch's target
  is a fixed point; the **mature** occupancy must never be used, being garbage-inflated (everything the sweep is
  about to reclaim), so `should_grow` would fire on every fullsweep and creep the capacity without bound.
- **The nursery's post-collection size is the C's TWO-STEP rule for a fullsweep, and the single bounded rule for
  a minor.** Step 1 — `major_collection` (`erl_gc.c:1866`) — is the **pre-copy-occupancy** sizing,
  `HeapPolicy::young_after_collection(capacity, observed_occupancy)`: a nursery the mutator genuinely filled
  keeps its capacity, so a steady workload pays no space install per fullsweep.
  Step 2 — `adjust_after_fullsweep` (`erl_gc.c:2062-2098`) — runs **after** the copy:
  `need_after = live_words + need_reserve_words`, and when `should_shrink(kept, need_after)` (the C's
  `4*need_after < HEAP_SIZE && HEAP_SIZE > H_MIN_SIZE`) the nursery shrinks to `next_words(2*need_after, 0)`
  clamped to `<= kept`.
  Step 2 is the half step 1 cannot see: a fullsweep fires *when the nursery is full*, so the pre-copy occupancy
  is ~the capacity and never triggers a shrink — only what **survived** says whether the nursery is
  over-provisioned.
  `live_words` is the **whole-heap** post-copy live count (`live::live` / `live_full().words`), never the
  nursery's surviving set, which is ≈0 after a fullsweep.
  Seams: `HeapPolicy::young_after_fullsweep(capacity_words, observed_words, live_words)`,
  `Nursery::collapse_after_fullsweep(policy, observed, live)` (fullsweep) alongside
  `Nursery::collapse(policy, demand)` (the minors-only seam), and `grow_major`/`relocate_major`'s `live_words`
  parameter.
- **`Nursery::collapse*` can never enlarge a nursery, and the capacity neither creeps nor oscillates.**
  The result is `<=` step 1's, which is `<=` `capacity_words` whenever `observed <= capacity` (a space's
  occupancy cannot exceed its capacity), so only `Nursery::grow` and `Collector::ensure_capacity` enlarge, and
  only on an explicit request.
  The shrink cannot oscillate either: `next_words(2*need_after, 0) < 4*need_after`, so the next `should_shrink`
  gate is false and the capacity is a fixed point; if the live set grows, the gate simply stops firing.
- **`GenImmixCollector` = the same copying nursery + a mark-region mature space, and its major reclaims the
  mature generation FIRST.** A minor promotes the nursery's live terms through an **atomic two-phase driver**
  (`src/immix_collector/promote.rs`, `Result<PromoteOutcome>`): phase R reserves each first-time live object,
  copies its words and writes the forwarding marker but rewrites **no** reference word; phase C rewrites the
  recorded slots and roots only once every object of the traversal is placed.
  The barrier-free invariant holds because promotion only ever moves young objects *into* mature, and a
  reservation that finds no run rolls both spaces back byte-for-byte (`PromoteOutcome::WouldNotFit`) instead of
  leaving a half-forwarded heap.
- **The Immix major is reclaim-first, and sound for the young→mature edge an in-place reclaim cannot ignore.**
  A read-only `scan_young` reports the promotion's exact **line** count and every nursery reference word pointing
  into mature; `ImmixSpace::collect` takes those terms as its `promoted` roots (so a mature object reachable only
  through the live nursery survives), each recorded nursery slot is rewritten to the object's final address, and
  only then does promotion run — by which point the nursery's mature references are `Foreign` and final.
  Reclaim-first (not promote-first) is what lets a would-not-fit minor escalate to a major that *makes room*;
  `words_reclaimed` = mature reclaim + nursery garbage.
- **`GenImmixCollector` ignores the `MatureKind` in `with_config`, and `AnyCollector` only forwards.**
  The type *is* the Immix-backed collector, so `with_config` always builds one over `cfg.policy()`; the strategy
  is decided by `AnyCollector::from_config` (on `config.mature()`) / `GcConfig::mature_choice` — the explicit pin
  or the automatic rule.
  The dispatch seam carries no collection logic, so neither strategy depends on the other; the geometry
  accessors exist only because the two mature spaces are differently typed, and they reproduce — not reinterpret
  — what each concrete space answers.
- **Heap sizing is ERTS-shaped and applied at every collection.** `HeapPolicy` carries the C minimums and the
  `fullsweep_after` cadence, `from_spawn_opts` reproduces the C `spawn_opt` rounding, and `Collector::collect`
  picks the collection with `fullsweep_due`.
  `mature_after_fullsweep` re-uses `should_grow` (`3*cap < 4*need`) / `should_shrink` (`cap > 4*need && cap >
  min`), floored by `min_heap_words` and clamped by `max_heap_words`.
- **The young nursery is a CHAIN of spaces (`src/nursery.rs`), and `grow_nursery` is a NO-RELOCATION seam.**
  `HeapSpace` never reallocates, so growing by *replacement* would strand every live word; `Nursery::grow`
  instead **retains** the old space and points the allocator at a fresh one, so every live word keeps its
  **original address** and nothing is moved.
  The step is the plain one-step `next_words(need_words, 0)` whenever that already enlarges the space, and **at
  least** `next_words(2 * capacity, 0)` when the one-step target would install a space no larger than the one it
  replaces — so a caller that grows repeatedly at a **constant** request climbs the growth table geometrically
  and the chain stays `O(log(heap))` long.
- **The ceiling bounds the chain, not one space, and a provably empty space is re-adopted rather than
  re-allocated.** `max_heap_words` is the process's heap budget (the C `+hmax`) and the nursery chain *is* the
  process's young heap, so it is enforced against `capacity_words()` (the sum over every space) rather than against
  the space being installed: a fresh space may claim only the remaining budget, and a request that budget cannot
  cover is the typed `heap_exhausted`. The two behaviours this replaces are an unbounded chain of ceiling-sized
  spaces and a same-size replacement that retained a whole capacity per call without an error.
  The ceiling never costs a *reallocation*: a retired space with `used_words() == 0` is provably empty (no word was
  ever handed out of it, so no term can name into it), so it is re-adopted as the current space — the smallest
  adequate one, leaving the largest free for a later request — and that swap allocates nothing, moves nothing and
  changes the summed capacity by nothing, so it consumes no budget.
  A re-adoption leaves the chain's *set* of spaces unchanged, so the aggregate covering cannot move and `extent` is
  untouched, but the geometry epoch still advances: which space is current (whose `top` may still move) is part of
  the cached geometry a host's fast-reject filter is invalidated by.
- **Containment over the chain is indexed, not scanned.** `Nursery::contains` answers from an ordered index of
  the retired spaces' live regions (`retired_regions: BTreeMap<base, top>`) plus one direct test of the current
  space, so it is `O(log #spaces)`; the intervals are pairwise disjoint (every space is a distinct live
  allocation), an indexed `top` is **frozen** from retirement, and the current space — the only one whose `top`
  moves — is tested directly, so no entry can go stale.
  That is what keeps the seam sound for a caller holding **no root set** — the un-rooted `beam_rt` allocation
  path, which must not collect (a copying collection may only run against a complete root set).
- **The chain is collapsed to exactly ONE space after any collection.** `young()`/`young_mut()` keep returning
  the current allocatable space; `young_contains` covers every space; a `max_heap_words`/growth-table ceiling that
  cannot reach a growth request returns the typed `heap_exhausted` and retires nothing.
  The chain's **length** is the bounded quantity; its retained **bytes** are the ladder's geometric sum,
  `O(requirement)`, not a function of the call count.
- **`xptr_tag` is not needed.** The C ORs the literal bit back in only for literal-area copies; this collector
  copies only in-heap objects, so a copied pointer never carries the literal bit, and a literal-tagged (or any
  other out-of-from-space) reference fails the `FromSet.contains` test and is left verbatim.
- **The copier relocates exactly the from-set-reachable graph — nothing else.** `Copier::complete` walks only the
  objects it copied into to-space, forwarding each child slot the canonical
  `beam_term::layout::for_each_child_slot` walk reports; a reference the `EvacDest` classifies `Target::Foreign`
  is left verbatim and its target is **not descended into**.
  So a from-space word reached *only* through a container outside the from-set — a mature object during a minor,
  a literal, or an unscanned foreign region (the runtime's pooled BIF heap / pointer-stable arena /
  `beam_message` fragment) — is neither kept alive nor updated.
  A major scans young *and* mature, so it repairs a mature→young pointer a minor left stale; a minor never can.
- **In-place Cheney forwarding, exactly as C.** A boxed object's **header word is overwritten** with the new
  BOXED-tagged pointer; a cons cell's car becomes `THE_NON_VALUE` and its cdr the new LIST pointer.
  The scan reproduces the C `sweep()` over the canonical per-object walk, so `copy.rs` holds no per-shape
  stepping logic.
- **Funs are sized and scanned from the inlined-descriptor layout in `beam_term::fun`, NOT
  `header_arity`/`thing_arityval`.** This runtime **inlines** the C's `entry` as descriptor words, so a fun block
  is `[header][desc_words(kind)][env[0..num_free]]` with `desc_words` = 2 (`Local`), 3 (`External`), 10
  (`Imported`).
  `header_arity` deliberately returns the C `ERL_FUN_SIZE - 1 == 1`, which would **under-size** the copy by
  `desc_words(kind) - 1` words (1/2/9) and land the scan on a raw descriptor word, following it as a bogus term
  reference and corrupting the heap.
- **Per-object sizing/walking is `beam_term`'s, shared not duplicated, with ONE copy-size override here.**
  `beam_term::layout::{object_words, for_each_child_slot}` is the only per-object walk, consumed by both the
  copying `Copier` and the mark-region `immix/` phases; `space.rs`, `collector.rs`, `policy.rs`, `stats.rs`,
  `config.rs` and `error.rs` never inspect a term's interior, and `beam_message` keeps its own private walk.
  The single exception is `copy.rs::move_size`, which is `object_words` for every shape **except** an
  `ErlSubBits`: there it returns `ERL_SUB_BITS_SIZE` (5) rather than the *sizing* graph's `ERL_REFC_BITS_SIZE`
  (8), because those are the words a copier lays down, after which forwarding `orig` copies the separate 3-word
  `BinRef`.
- **Root set = a flat `&mut [Eterm]`.** The C scans the whole stack *word by word* with no frame metadata (a word
  is a term iff its primary tag is BOXED/LIST and it points into a swept area), so a flat slice of root words
  (live x registers + live stack cells) is **semantically faithful**, not a shortcut.
  Frame-aware stack relocation (`copy_erlang_stack`) and the process dictionary / message-queue / saved-argument
  sub-rootsets belong to `beam_process`.
- **`HeapSpace` wraps `beam_alloc::Heap`; it is not a new allocator.** `beam_alloc::Heap` is the production bump
  heap (its `AllocKind::Heap` exhaustion means "collect") and `HeapSpace` adds only space semantics, so collector
  and mutator share one allocation implementation.
- **The off-heap list is `Mso`/`MsoHandle` + `beam_alloc::RefcBinary` + `OffHeap`, not bolted onto the heap.**
  Both payload kinds live **outside** every process heap; the collector copies only the small heap-resident
  handle's scalar words and leaves every off-heap pointer verbatim (the C `sweep_off_heap` rule), and refcount
  changes are the **off-heap API's** responsibility, so the collector never walks off-heap payloads.
  The count is `beam_alloc::RefCount`; the on-heap `BinRef`/`ErlSubBits` *handle term* is `beam_term`'s.
- **`OffHeap` is slot-stable for the list's whole lifetime** — a released slot is kept in place as `None`, so
  `sweep` never shifts, reorders, compacts or reuses a slot, which an on-heap off-heap reference's `val`-word
  slot index requires (`beam_message::layout::make_off_heap_container` / `off_heap_slot`).
- **The scanned-region aggregate is cached in the nursery chain and derived, not stored, at the collector.**
  `Nursery` keeps `extent: ScannedBounds` over the **reserved** extents of every space of the chain: `grow` only
  *widens* it (`min`/`max`, `O(1)` regardless of chain length) because the retired space was already covered
  while it was current; a collapse *re-seeds* it from the single surviving space (a stale widening would keep
  accepting the retired neighbourhood); both advance the epoch.
  `Collector`/`GenImmixCollector` compose it with their mature extent and a separate `mature_generation` counter
  bumped where the mature space is actually replaced (an in-place mark-region reclaim does not move it), the
  reported `generation` being the **sum** of the two monotone counters.
  The **reserved** extent (not the moving `top()`) is used because `top()` moves on every allocation, which would
  invalidate a cached bound immediately.
- **The minor→major escalation test is a CAPACITY test on the nursery's occupancy, and on the `gc_bound/churn`
  shape every escalation it takes is genuine** — `tests/churn_attribution.rs` records the mature as low as **1**
  free word against a nursery occupancy up to 550, so the ~0.5 majors/op are forced by the mature generation's
  promoted garbage, not by a conservative test.
  An exact-live variant would remove false escalations elsewhere, does not help this shape, and costs one extra
  young traversal per collection — deliberately not adopted.
- **Tightening the aggregate is not worth implementing; its acquisition is the only lever.** The covering equals
  the union of the scanned regions to within one block except after a collection, where the re-installed nursery
  lands ≈14 KB away from the mature space — and there a per-space interval list or a nursery-vs-mature split is a
  **lateral** trade, because the population falling through tier one is mature- and retired-space pointers, which
  every interval shape must fall through on (`may_contain` is a reject filter by contract).
  A sound fast-*accept* on the mature half would be a different design (`AnyCollector::kind()`); since `beam_rt`
  re-reads a freshly built `ScannedBounds` per reference word, hoisting one read per walk (a parameter here) is
  the cheaper lever.

### Measured
Every figure, table and caveat is in **`MEASURED.md`**; this section keeps only the load-bearing headlines and
the section to read them in.
- The `gc_bound/churn` shape allocates **806 words/op** against **2.00 collections/op** in-crate (1.50 minor +
  0.50 major) and 7.00 on the real runtime at a 376-word nursery, copies **1 269 words/op = 1.58×** the
  allocated volume, and never hands out ≈0.70 of every admitted young word — `MEASURED.md`, "Per-op attribution
  of the `gc_bound/churn` shape".
- A `perf` profile of that shape puts this crate's copy loop at **4.2 %** of samples (interpreter 20 %, libc
  malloc/free/memmove 15 %), so the dominant terms are the interpreted recursion and `beam_bifs`' `Vec` churn,
  not the copy loop (Handoffs) — `MEASURED.md`, same section.
- The two-step post-fullsweep nursery sizing removed the per-op space install (growths/op **1.0004 → 0.0004**)
  and cut the collection count a third (**2.000 → 1.500/op**), at a mildly higher copy volume (1 269.5 → 1 283.5
  words/op) — paired A/B ratios **0.964 / 0.960 / 0.929**, with the `perf stat -e task-clock` pass the stable
  signal on this perturbed host — `MEASURED.md`, "Post-fullsweep nursery sizing (the `gc_bound/churn` win)".
- Against the **oracle** at a matched heap policy (`+hms 233`, the C `H_DEFAULT_SIZE`), the minor count grows
  linearly with the loop count at **2.5×** the oracle's per-op slope on a small live set and **18×** on a
  live-set-protected one, at `total_heap_size` **4 039** vs **3 286** words — so the two-step rule **cuts
  footprint and makes the capacity track the live set**; it does **not** realign the cadence, which is
  dominated by fullsweep frequency and by the nursery size the heap policy installs (`beam_rt`'s
  `DEFAULT_HEAP_WORDS` sizes only its term-construction heaps, never a process nursery) — `MEASURED.md`,
  "The cadence against the oracle with a matched heap policy".
- Folding the young heap's occupancy into the fullsweep's size request is what makes the mature generation
  reachable at the default policy: the copying default runs **147 minors : 73 fullsweeps** — `MEASURED.md`,
  "The mature-generation room policy (generational cadence)".
- GenImmix wins **copy work on long-lived data** but pays footprint and pause on the small-heap/high-churn shape,
  so **`MatureKind::Copying` remains the default** and Immix is a size-/latency-tolerant option auto-selected at
  ≥ `IMMIX_BREAK_EVEN_WORDS` (2048, from the construction-time `initial_mature_words`) or by an explicit pin
  (`BEAM_RT_GC_STRATEGY=immix`) — `MEASURED.md`, "The long-lived break-even sweep" and "The two-axis selection
  curve".
- Under a fixed demand both capacities converge: nursery/mature **233/376** for Copying and **233/512** for Immix
  — `MEASURED.md`, "Heap-sizing measurements" (`tests/steady_state.rs`).

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only.
  Heap exhaustion while copying a term is `BeamError::Alloc(AllocFailure::new(AllocKind::Heap, bytes))` — never
  panic, never a crate-local error enum, never `unwrap()` on runtime data.
  `beam_term::TermError` is reconciled to `BeamError` upstream, so `?` crosses the boundary.
- **`unsafe` confinement:** production `unsafe` lives only in the word-level code — `copy.rs` (the Cheney
  copier), `immix_collector*` (the promotion driver) and `immix/` (the mark-region space's
  line/mark/evacuation walks) — plus the two narrow `NonNull`-deref / `Box::from_raw` blocks in `mso.rs`.
  Every `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`beam_common::safety`), and `unsafe_op_in_unsafe_fn` stays clean.
  `space.rs`, `policy.rs`, `heap_info.rs`, `stats.rs`, `config.rs` and the
  `Collector`/`GenImmixCollector`/`AnyCollector` driving logic are safe Rust (only their `#[cfg(test)]` sections
  call `unsafe alloc_words`).
- **Do NOT modify `beam_term` or `beam_alloc`** — build on their public APIs only.
  Do not touch `../../../erts/` (read-only ground truth) or `../../tools/`.
- **Never break the frozen term layout or the barrier-free invariant** (a minor must not leave a mature→young
  pointer).
- Files target ~1000 lines; split by responsibility.
- Licensing Apache-2.0; deps `beam_common`, `beam_term`, `beam_alloc`, `libc` only.

## Test Strategy
- **Each integration test file under `tests/` carries its own `//!` module doc stating the contract it pins and
  why** — read the file rather than looking for a per-file inventory here.
- In-module `#[cfg(test)] mod tests` live in `space`, `policy`, `mso`, `heap_info`, `config`, `selection`,
  `any_collector`, `collector`, `bounds`, `live`, `immix_collector` (+ `src/immix_collector/{tests,grow_tests}.rs`),
  in `src/nursery/tests.rs` (the nursery chain) and in `immix/tests.rs`.
  `copy.rs` carries no in-module tests: the copier is pinned by `tests/copy_correctness.rs`, the collector driver by
  `tests/generations.rs` / `tests/gc_pressure*.rs`.
- The suite is organised around five invariants:
  - **Exact copy correctness** — structure preserved, roots relocated, exactly the live size retained,
    off-from-space refs verbatim, verified per shape (cons/tuples, flat + HAMT maps, the refc pair, all three fun
    kinds, bignums, a native-record instance, an `ErlSubBits` match context, external ports/refs, a HAMT head
    node) against each shape's independently measured footprint; `memory_overhead.rs` cross-checks `object_words`
    against the bump allocator's real consumption.
  - **Generations, cadence and sizing** — a bounded nursery, the two-step fullsweep sizing, `fullsweep_after`,
    `from_spawn_opts`, convergence of both capacities, the per-op counters of the `gc_bound/churn` shape, and a
    system-`erl` cross-check that SKIPS LOUDLY when `erl` is absent.
  - **The nursery chain** — no-relocation growth (live terms keep their original address and no collection
    runs), the indexed containment answered exactly at long chain lengths, retention/debt semantics, and a
    bounded plateau under a grow-then-collect loop.
  - **The aggregate scanned bounds** — the safety property (a rejected address is in no scanned region, asserted
    over every region neighbourhood at chain lengths 1/2/3/8/64/1024, for capped and differing-capacity chains,
    both strategies and the `AnyCollector` forwarding), plus the aggregate's exactness and the epoch moving
    exactly on growth/no-op growth/collapse/mature-replacement.
  - **Immix** — geometry/allocation/mark/pin/evacuation/accounting, the break-even rule, the atomic promotion
    rollback (mutation-verified: replacing `rollback` with a no-op fails `tests/immix_promotion_rollback.rs` 4/4
    while `tests/immix_fragmentation.rs` still passes 5/5), the reclaim-first major ordering, region growth
    versus a `max_heap_words` cap, and both strategies in lockstep through one process-shaped workload.
- Cross-cutting adversarial coverage: a too-small destination returns typed **before any copy**, with counters
  byte-identical and identical on retry; a 50 000-deep chain proves the copier iterative; cyclic graphs never
  hang; off-heap payloads are freed exactly once and a `val`-slot container still resolves after a **middle**
  sweep. Pauses are printed only, never asserted.
- Post-fullsweep nursery sizing is pinned by `tests/post_fullsweep_nursery.rs`'s six tests — shrink toward twice
  `need_after`, keep when the live set is not far below the capacity, the churn geometry protected from the
  shrink, never exceeding the collected capacity, one constant capacity under a fixed demand, both mature
  strategies identical; the file's `//!` doc states the full rule.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc` and
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Cross-checks against the real system ERTS belong to `../tools/differential/`, not here.

## Known Gaps (still open)
- **The mark-region strategy is reachable in production; the DEFAULT remains the copying `Collector`.**
  `beam_process::Process` owns an `AnyCollector` built from the spawn policy (`collector()`), and `beam_rt` drives
  it at its heap-admission point (`src/context/gc.rs::{ensure_heap, ensure_heap_full}`), reading the mature
  generation through two geometry accessors (`mature_contains` in the unscanned-root valve, `mature_used_words` in
  the memory hook).
  `select_mature` chooses `Immix` only at/above `IMMIX_BREAK_EVEN_WORDS`, which the default 233-word policy never
  reaches, so `GcConfig::default()` builds the copying `Collector`; Immix needs a large configured mature
  generation (`beam_rt`'s default-OFF `BEAM_RT_GC_STRATEGY=immix` opt-in does exactly that) or an explicit pin.
  Either way the mature generation is genuinely exercised (**147 minors : 73 fullsweeps** at the default).
- **The Immix region never reallocates in place; the collector grows it by relocation.** `ImmixSpace` wraps a
  `beam_alloc::Heap` and is never resized in place, so a major replaces the whole region — the live set is
  relocated into a fresh `ImmixSpace` sized by the policy (`relocate_major`), or a larger region when an in-place
  fullsweep's reclaim cannot fit the nursery's promotion (`grow_major`).
  A growable heap therefore never fails a collectable collection; only a `max_heap_words` ceiling the promotion
  cannot fit returns the typed `heap_exhausted` (before any copy).
- **A retired nursery space is freed only by a collection; a provably empty one is re-adopted instead.**
  `Nursery::grow` supersedes the space the mutator was allocating in **without collecting and without moving a term**,
  so a live term may still name into it; the space therefore stays addressable as a chain link (whatever its
  occupancy) and only a collapse frees it, because the collection's from-set covers every space and has copied
  everything reachable out.
  A superseded space that never handed the mutator a word (`used_words() == 0`) is not retained: `grow` **re-adopts**
  the smallest adequate empty retired space (nothing allocated, no capacity consumed, nothing stranded) and only
  allocates a fresh space when no empty retired one is large enough.
  `max_heap_words` bounds the **summed** chain capacity: a fresh space may claim only the budget the chain has left
  (`max_heap_words - capacity_words()`) and, when that cannot reach the request, `grow` returns the typed
  `heap_exhausted` **before** allocating or retiring anything — so after every successful growth
  `capacity_words() <= max_heap_words` and an allocate-and-abandon loop cannot append one ceiling-sized space per
  call.
  `max_heap_words == 0` (the default) is "no cap": growth is then bounded only by the growth table's last entry (just
  under `(1<<53)/8` words), and the doubling rule keeps the chain `O(log(heap))` long by design.
  A policy whose `initial_young_words` already exceeds `max_heap_words` starts the chain above its ceiling (the
  caller's explicit configuration), and every growth is then the same typed refusal.
- **The chain's collection debt is an opt-in seam; `should_collect` stays occupancy-only** (seam semantics: Notes
  for Agents). `Nursery::retains_old_spaces` is exported as `collection_debt() -> bool` on all three collector
  types; `true` means *a collection is required to reclaim the chain — a heap-admission door should treat it like a
  full nursery*, and the retention is then **one space at most**, however many cycles run (measured: 400 grow+admit
  cycles → peak chain **2** spaces / **932** words — `MEASURED.md`, "The debt signal and the collection").
  Folding it into `should_collect` instead would tax **every** admission, which is why it is a separate accessor.
- **A refused promotion is ATOMIC; `collect` recovers, `collect_minor` does not.** Line-granular allocation needs
  a *contiguous* run inside one 16-line block, so a fragmented region can refuse an object although region-wide
  `free_lines()` reports room; the promotion driver then rolls BOTH spaces back byte-for-byte and reports
  `PromoteOutcome::WouldNotFit` — no marker, no rewritten root, no counter advanced.
  `collect` escalates that to a fullsweep and `grow_major` rebuilds a fresh region (fragmentation-free by
  monotone packing), even at the current capacity when a ceiling forbids growing; only a ceiling that cannot hold
  the live set in ANY layout reports the typed heap exhaustion, still before any word is copied — and
  `collect_minor`'s own public contract reports that typed error rather than escalating, which is `collect`'s job.
- **Deliberately NOT ported:** the C `high_water` single-block layout, per-object tenuring
  (`GEN_GCS`/`MAX_GEN_GCS`) and the `F_NEED_FULLSWEEP` flag protocol (replaced by the two-space young/mature
  scheme above); the dirty-scheduler escape for long collections (`ERTS_POTENTIALLY_LONG_GC_HSIZE`); and
  **literal-area collection** (`erts_garbage_collect_literals`, literal-area purge/`xptr_tag`) — literals are
  treated as permanent and left verbatim.
- **Out of scope by ownership:** frame-aware stack relocation (`copy_erlang_stack`), the process dictionary,
  on-heap message/signal queues and saved BIF/NIF arguments as sub-rootsets — `beam_process`'s; this crate takes
  a flat root slice. A.6 runtime heap options (`+hms`, `+hmbs`, …) — this crate exposes `HeapPolicy`, not the
  CLI surface (`beam_rt`'s).
- **`OffHeap` is on the production path as both the MSO slot store and the process's binary store; its liveness
  predicate is caller-driven.** It holds both MSO and refc-binary entries and IS the production MSO slot store
  inside `beam_message::OffHeapTable` (which stores the slot index an on-heap container names).
  `beam_process::Process` additionally owns a dedicated `OffHeap` (`owned_binaries`) swept as the last step of
  every rooted collection, in **two halves**: a **reachability-driven** half (enumerate the whole heap through
  `for_each_reachable_object`, record each reachable `BinRef` payload pointer, release every owned payload no
  reachable `BinRef` names; it **declines**, retaining everything, for a process with `has_unpublished_roots()`)
  and a **conservative** half (release exactly the entries the caller declared dead).
  The marking walk is public and layout-agnostic — it reports *which* objects are reachable, never what they
  contain — so the caller applies its own `ErlSubBits`/`BinRef` subtag test and reads the payload pointer word.

## Handoffs (owned by other nodes)
- **The `gc_bound/churn` residual is not this crate's.** The major count stays capacity-forced by the mature
  generation's promoted garbage and this crate's copy loop is only **4.2 %** of a `perf` profile of the shape; the
  dominant costs are the interpreter tier (20 %), libc malloc/free/memmove (15 %), `beam_rt::context::gc` (12.7 %)
  and `beam_bifs`' `Vec` churn (4.9 %) — `beam_bifs/src/binconv.rs:335-382` (`list_to_binary` builds two `Vec`s),
  `beam_bifs/src/term_ops.rs:84-139` (`proper_list` `Vec` for `list_to_tuple`),
  `beam_bifs/src/arith.rs:648-650` (`Big::from_i64` boxes a `Vec<u64>` per small-int operand) and
  `beam_rt/src/context/gc.rs:667-675` (`collect_or_grow` re-installing a space per op).
  Owners: `beam_bifs` / `beam_engine` / `beam_rt`.
- **The growth ladder keeps only ~40–60 words of slack,** so the nursery settles at 609 words where the op
  allocates 806 — the collection count improves by a third but not more; the one-step-then-doubling rule is not
  implicated (`peak_spaces = 1`).
- **The strategy switch is adopted; the residual is an explicit pin on the process-construction path.**
  `beam_process::Process` holds an `AnyCollector` and exposes `collector()`, with `run_rooted_collection` bound as
  `FnOnce(&mut AnyCollector, &mut [Eterm])`; `ProcessTable::spawn` takes only a `HeapPolicy`, so the strategy is
  always resolved by the automatic rule (`MatureSource::Automatic`).
  An explicit `GcConfig`/`MatureSelection` pin would need a parameter on the process-construction path (owner:
  `beam_process`).
- **`Process::collect_kind` is owed by `beam_process`** (this crate's half is in place: `AnyCollector::collect_kind`).
  `pub fn collect_kind(&mut self, kind: CollectionKind) -> Result<Collection>` = the `relocation_exempt` guard
  naming `collect_kind`, then `self.run_rooted_collection(|collector, roots| collector.collect_kind(kind, roots))`.
  The `beam_rt` handoff: `process.collect_kind(beam_gc::CollectionKind::Minor)?` yields
  `Collection { kind: Minor, .. }` or the typed `BeamError::Alloc` (`out_of_memory`) when the promotion cannot fit
  the mature space — never an escalated fullsweep — and `CollectionKind::Major` pins a fullsweep.
- **`beam_process` should adopt the safe reachability view (`for_each_reachable_object_view`).** Its
  `sweep_off_heap_binaries_reachable_from` still drives `AnyCollector::for_each_reachable_object`, whose raw
  `(header, ptr)` pairs force the single production `unsafe { *ptr.add(1) }` word read in its
  `reachable_bin_ref_payload` probe; the view removes it with zero caller `unsafe`. Owner: `beam_process`.
- **The growth seam is adopted; a `run` process collects only at a `test_heap`/`allocate_heap` admission door,
  so a loop with no admission instruction never collects.** `beam_rt/src/context/gc.rs`'s shared `collect_or_grow`
  (behind both `ensure_heap*` doors) grows the running process's nursery through `Process::ensure_capacity` when the
  policy admits the request and otherwise collects, and `Process::deliver` retries a refused copy the same way; a
  bounded `+hmax N` policy the request cannot reach still refuses, and that exhaustion raises `error:system_limit`
  (`HeapPolicy::unbounded()`/`GcConfig::growable()` express "no cap").
  Those two doors are the only readers of `collection_debt()` (`context/gc.rs:835-839`, `:1041-1045`), and only an
  `allocate_heap`/`test_heap` instruction reaches them: the per-term seams short-circuit first —
  `alloc_words_rooted`'s grow-and-retry (`context/gc.rs:1366-1373`) returns before its `process.collect()`
  (`:1472`), and the un-rooted `alloc_words` (`:1196-1235`) has no collect branch at all — so such a loop retires
  space after space with `minor_gcs == 0` and `statistics(garbage_collection) == {0,0,0}` while the chain-summed
  `heap_size` climbs in N (geometrically under the doubling rule), stopped only by the policy's `max_heap_words`
  chain budget — which now refuses with the typed `heap_exhausted` (`error:system_limit` at the runtime) instead of
  appending another space; an unbounded policy (`max_heap_words == 0`) has no such stop.
  `ensure_capacity` is the only method that collects *unconditionally* (no occupancy test,
  `src/collector.rs:321-334`), and an explicit `erlang:garbage_collect()` is honoured before the bump on both seams
  (`:1332`, `:1457`), so a `heap_size` that collapses after it distinguishes "no door reached" from a valve
  decline. Owner of the doors: `beam_rt`.
  Where a route *does* admit (an `ets:insert`/`ets:lookup` loop over a 5-field payload map),
  `process_info(memory)` is **flat** in N (identical bytes at N = 100 and N = 20 000), so nothing retains per
  iteration there.
- **Cross-crate term caveats.** A pid is a heap term via `beam_term::thing::write_external_pid` (4 words) and
  `beam_term::compare::eq` orders it **structurally through its parts** (field order per the C `erts_cmp`), with
  `external_pid_parts` to read them directly; `compare::eq` still **recurses forever on a cycle**.
  An MSO has no on-heap reference *term*: the safe reference is the `OffHeap::insert -> slot` index, never a raw
  pointer; reach a refc binary's `BinRef` via the `ErlSubBits` `orig` slot, not pointer adjacency.
- **`HeapInfo` is surfaced by the runtime.** `beam_rt` builds it directly (`HeapInfo::new` and
  `HeapObservable`) for `erlang:process_info(Pid, garbage_collection)` and `system_info(garbage_collection)`, in
  `GARBAGE_COLLECTION_KEYS` order (`beam_rt/src/native/memory/mod.rs`, `src/info/memory.rs`);
  `AnyCollector::heap_info()` itself is called only from tests.
- **Repo-root owned** (`../../CHANGE.md`): the mature-generation room policy and the measured GenImmix decision
  are recorded (Milestones 6 and 9); the two-step post-fullsweep nursery sizing, the `HeapPolicy` field surface
  (`min_bin_vheap_words`/`fullsweep_after`) and the `HeapInfo` observables are still owed entries.

## Notes for Agents
- **`minor_gcs` is NOT monotone — it is `minor_since_major`, and a major RESETS it to 0.**
  The only increment sites are `collector.rs`'s `collect_minor` and `immix_collector.rs`'s `run_minor`; both
  majors (`collect_major`, `run_major`'s in-place path and `relocate_major`) assign 0.
  Consequence for observational forensics: a **constant non-zero** `minor_gcs` read at two different workloads
  proves *no collection of either kind ran in between*; a major-only workload reports `0`, never a stale count.
  The monotone lifetime counters are `GcStats::{minor_collections, major_collections}` via `stats()`, but they
  are **not** part of `HeapInfo` / `process_info(P, garbage_collection)`, whose only collection counter is
  `minor_gcs`.
- **`heap_size`/`total_heap_size` are CAPACITIES (occupancy-independent), so a flat footprint is NOT evidence that
  collections are running.** `heap_size_words` = `Nursery::capacity_words` (summed over the chain);
  `total_heap_size_words` = nursery + mature capacity; neither moves while words are handed out — only a
  collection (a collapse) or a `grow_nursery`/`ensure_capacity` growth changes them.
  A collection that runs with a small live set *shrinks* them (the fullsweep's second step; a fixed live set
  converges to one constant capacity), so a steady state under continuous collection shows a **converged
  constant** capacity plus a **rising** `minor_gcs`; a constant capacity with a *frozen* `minor_gcs` means no
  collection.
- **`should_collect` is occupancy-only and ignores the chain, so a retired space is visible only through
  `collection_debt()`.** `should_collect(need)` reads the **current** space alone
  (`current().available_words() < need`) and a growth guarantees that room, so it cannot be true right after a
  growth that satisfied the same `need` — nor on either `ensure_capacity` entry point (`next_words(need, 0) <
  need` pre-check, post-collection `grow`).
  `collection_debt()` is `!retired.is_empty()`, set by every `Nursery::grow` that actually replaces a space (the
  `available_words() >= need` early return retires nothing, so the chain is monotonic *between* collections) and
  cleared only by a collapse — a collection, or `ensure_capacity`'s own collect, which then grows again and leaves
  the chain at `2`, so the runtime's pairing (`collect_or_grow` / `Process::deliver`) makes `young_space_count`
  oscillate ≈`1`→`2`→`1`.
  `retired.clear()`/`retired_regions.clear()` exist in exactly one place, `Nursery::install_collapsed`
  (`src/nursery.rs:363-364`), and `retired.push` in exactly one, `Nursery::grow` (`src/nursery.rs:292`).
- **Address-level membership is public; enumeration of the nursery chain is NOT.** From outside the crate a caller
  reaches only the **current** nursery space (`young()`), a space **count** (`young_space_count()`) and a
  membership test over **every** space (`young_contains(ptr)`); the `Nursery` chain itself (`spaces()`, the
  retired spaces' base/top) is `pub(crate)`.
  The mature side is fully addressable (`mature_base`/`mature_capacity_words`/`mature_used_words`/
  `mature_contains` on `AnyCollector`; `Collector::mature()`, `GenImmixCollector::mature()`).
- **There is no public from-space / "retired spaces" / epoch accessor.** `FromSet` is public and constructible
  (`new`/`add(&HeapSpace)`/`contains`/`words`) but `ranges()` is `pub(crate)` and the collector builds its
  from-set internally; a major **drops** the old mature space (`self.mature = fresh`), so a collection's
  from-space ranges must be captured from `young()`/`mature_base()` **before** it runs, never queried after.
- **The single-object size helper is `beam_term::layout::object_words(header, ptr) -> usize` (`pub unsafe`, in
  `beam_term::prelude`), NOT `beam_gc::copy::move_size`.** `move_size` is `pub(crate)` and equals `object_words`
  except for the `ErlSubBits` override; `beam_term::size::size_object(Eterm)` is the whole-graph (public) size;
  the per-object reference walk `for_each_ref` is `pub(crate)`, and the public enumerator is
  `beam_term::layout::for_each_child_slot`.
- **Reading the counters for a leak-vs-recycle diagnosis.** `heap_size_words()` is the **summed** capacity of the
  chain, so a value constant across N iterations proves **no uncollected retirement is outstanding** without
  reading `collection_debt()`; no counter of this crate advances on the `grow_nursery` path except
  `young_space_count()`, `collection_debt()` and that summed `heap_size_words()`.
  The only counter-free reclamation is the bump-pointer rewind `HeapSpace::reset`/`Heap::reset`/`Heap::reset_to`,
  reached through `young_mut().heap_mut()` and used by `beam_rt` per BIF call on its *pooled* BIF heap — never on
  a process heap.
- **Address stability under growth vs. collection.** A `grow_nursery` always installs a **new** `HeapSpace` (so
  `young().base()` changes and the replaced base joins the non-enumerable chain); a collapse keeps the base when
  the target capacity equals the current one (it only calls `reset`) and installs a new space only when the
  target differs.
- **`young()` is the CURRENT space; every "nursery" figure in `HeapInfo` is CHAIN-SUMMED.** Neither `Collector` nor
  `AnyCollector` exposes a chain-summed capacity getter: `young().capacity_words()` (public `HeapSpace`) is the
  current space only, while `heap_size_words()`/`HeapInfo::heap_size` is `Nursery::capacity_words()`, the sum over
  the chain (`src/collector.rs:471-473`). They agree only while `young_space_count() == 1` — between a collapse and
  the next `grow_nursery`, never after a growth.
- **`GcStats` *returned* by `collect()` is per-collection; `stats()` is lifetime-cumulative for all five fields.**
  The "per-collection counters are reported for the most recent collection" wording at `src/stats.rs:25-27`
  describes only the returned value: `words_copied`/`words_reclaimed`/`objects_copied` are `+=`-accumulated into
  the collector's own `GcStats` (`src/collector.rs:207-210`, `:284-287`, `immix_collector.rs:531-538`), so
  `stats()` sums the whole collector lifetime.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/space.rs` → `HeapSpace` + `FromSet` (base-sorted ranges, binary-search `locate`)
- `src/nursery.rs` → `Nursery`: the young nursery **chain of spaces** (`grow`/`collapse`/
  `collapse_after_fullsweep`/`spaces`/`contains`/`add_to_from_set`/`used_words`/`capacity_words`), its indexed
  containment (`retired_regions`), its cached reserved-extent aggregate (`extent: ScannedBounds`, `bounds()`), its
  debt query (`retains_old_spaces`), the `max_heap_words` chain budget and the provably-empty-space re-adoption
  (`empty_retired_for`/`readopt_retired`/`install`)
- `src/nursery/tests.rs` → the nursery chain's in-module test suite (the growth contract, the re-adoption, the
  containment index and the aggregate extent, incl. a directly installed long chain)
- `src/bounds.rs` → `ScannedBounds`: the O(1) aggregate extent over every scanned region (nursery chain + mature)
  and its geometry epoch — the fast-reject filter a host's unscanned-root valve calls
- `src/copy.rs` → the Cheney copy engine: `Copier`, `forward_roots`/`complete`, `collect` + the copy-specific
  `pub(crate)` primitives
- `src/collector.rs` → the two-generation `Collector` driver (incl. the kind-pinned `collect_kind`)
- `src/live.rs` → the exact-live counter (`live`/`live_full`, `Live { words, lines }`)
- `src/reachable.rs` → the read-only whole-heap walk: `ReachableObject`, `walk_reachable`,
  `for_each_reachable_view`/`for_each_reachable`, the `Visited` set (its `index` delegates to `FromSet::locate`)
- `src/config.rs` → `GcConfig`, `MatureKind`, `MatureSelection`, `mature()`/`mature_choice()`, `growable`
- `src/selection.rs` → the automatic rule: `IMMIX_BREAK_EVEN_WORDS`/`IMMIX_BREAK_EVEN_BLOCKS`, `select_mature`,
  `mature_words_of`, `resolve_mature`/`choice_for`, `MatureSource`, `MatureChoice`
- `src/immix/mod.rs` → `ImmixSpace` geometry, block metadata, line-granular allocation, the `release_*` rollback
  primitives, `ImmixStats`/`ImmixCounters`
- `src/immix/collect.rs` → `ImmixSpace` mark/classify/evacuate/finalize phases + its `EvacDest` impl
- `src/immix_collector.rs` → `GenImmixCollector`: `scan_young`, fullsweep sizing via `live_full`,
  `grow_major`/`relocate_major`, `ensure_capacity`, the
  collect/collect_minor/collect_major/collect_kind driver
- `src/immix_collector/promote.rs` → the ATOMIC two-phase promotion driver (`promote`, `PromoteOutcome`)
- `src/immix_collector/{tests,grow_tests}.rs` → `GenImmixCollector`'s in-module promotion + `grow_major` tests
- `src/any_collector.rs` → `AnyCollector`: the `Copying`/`Immix` dispatch over one API — the drop-in for the
  concrete `Collector` incl. the kind-pinned `collect_kind` and the mature geometry accessors
- `src/policy.rs` → `HeapPolicy`, the curated `heap_sizes` table, `next_heap_size`, `from_spawn_opts`/
  `fullsweep_due`/`mature_after_fullsweep`/`young_after_collection`/`young_after_fullsweep`, `unbounded`
- `src/heap_info.rs` → `HeapInfo`, `HeapObservable`, `GARBAGE_COLLECTION_KEYS`
- `src/mso.rs` → re-exported `RefCount`, `Mso`, `MsoHandle`, `OffHeapEntry`, `OffHeap` — the **slot-stable**
  off-heap list over off-heap refcounted large objects **and** refc binaries
- `src/stats.rs` → `GcStats`; `src/error.rs` → `pub(crate)` GC allocation-failure constructors
- `tests/` → the integration suite; each file's `//!` module doc states the contract it pins
- `../../../erts/emulator/beam/erl_gc.c` → the collector being mirrored (`move_boxed`/`move_cons`/`sweep`/
  `do_minor`/`full_sweep_heaps`/`adjust_after_fullsweep`) (read-only)
- `../../../erts/emulator/beam/copy.c` → term traversal reference (`size_object_x`/`copy_struct_x`) (read-only)
- `../../../erts/emulator/beam/erl_gc.h` → forwarding macros (`IS_MOVED_*`, `follow_moved`, `ErtsInYoungGen`)
  (read-only)

## See Also
- `MEASURED.md` → **all** measurement figures and caveats for the mature-generation strategies (the
  allocation-heavy workload, the break-even sweep, the selection curve, the multi-workload measurement, heap
  sizing, the room policy, the nursery chain, the debt signal, the per-op attribution, and the end-to-end
  `ets_copy` A/B RSS/latency).
- `../CONTEXT.md` → crate-layer contracts, seam types and the wave-1 dependency position.
- `../../CONTEXT.md` → workspace contracts (§5 GC/memory model, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D ("Memory management in Rust") and §H (Stage 4).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → foundations this crate builds on (do not modify).
- `../beam_process/CONTEXT.md` → the primary consumer that drives collections at safepoints.
