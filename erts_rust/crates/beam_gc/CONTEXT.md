# `beam_gc` — Per-Process Garbage Collector (Stage 4)

## Intent
Owns the **per-process collector** of the Rust ERTS rewrite: a **generational semi-space Cheney copying**
collector over the per-process heap, **barrier-free by construction** (terms are immutable ⇒ *no term on the
mature heap may refer to a term on the young heap*, so the mutator pays zero per-store overhead), plus the
off-heap **refcounted large-object (MSO)** interface and the process **heap-size policy**. Implements the GC
half of `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D and the Stage-4 row of §H; reproduces the collector
semantics of the legacy C (`../../erts/emulator/beam/erl_gc.c` + `copy.c`) **without** its C structure.
Consumed by `beam_process`, which drives collections at safepoints; depends downward only on `beam_common`,
`beam_term`, `beam_alloc`, `libc` (wave 1).

Honours the crate-layer contracts of `../CONTEXT.md` (root-set contract, no frame liveness maps; frozen term
layout §2; return-based error strategy §3). MMTk/Boehm are **rejected**; GenImmix is a **measurement-gated**
mature-generation option (`immix/` + `immix_collector.rs`) selected by the **measurement-derived automatic
rule** (`selection.rs`) — Design Decisions.

## API Surface (the seam dependents design against)
Re-exported at the crate root and via `beam_gc::prelude::*`.

### `space.rs` — copying semi-spaces
- `pub struct HeapSpace` — one copying semi-space: a thin wrapper over `beam_alloc::Heap` adding *space*
  semantics (range tests + reset); never reallocates (growth is by GC).
  - `new(capacity_words) -> Result<HeapSpace>`; `with_allocator(alloc: AllocRef, capacity_words)`;
    `from_heap(Heap)`/`into_heap()`; `heap()`/`heap_mut()`
  - `capacity_words()`, `used_words()`, `available_words()`, `base()`, `top()` (the bump pointer `hp`),
    `limit()` (= `hend`) — `*mut Eterm` — and `reset()`
  - `contains(*const Eterm) -> bool` (the live region `[base, top)`); `contains_term(Eterm) -> bool`
- `pub struct FromSet` — the from-space ranges a collection evacuates (one for a minor, two — nursery +
  mature — for a fullsweep): `new()`, `add(&HeapSpace)`, `contains(*const Eterm) -> bool`, plus the
  `pub(crate)` helpers `add_range(lo, hi)` and `ranges()`. The ranges are stored **sorted by `lo`**, so the
  per-reference `contains` is one binary-search `locate(addr) -> Option<usize>` and not a scan over the
  ranges; `add_range` is the single insertion point (re-sorts), so the invariant cannot be bypassed.

### `copy.rs` — the Cheney copy engine (the unsafe core)
- `pub struct Copier<'a>` — copies live terms from a `FromSet` into a `HeapSpace`, mirroring the C
  `move_boxed`/`move_cons`/`sweep`: `new(from, to)`; `forward_roots(&mut [Eterm])`; `complete(self) ->
  Result<GcStats>` (the Cheney scan); `run(self, roots)` = `forward_roots` + `complete`.
- `pub fn collect(from: &HeapSpace, to: &mut HeapSpace, roots: &mut [Eterm]) -> Result<GcStats>` — the
  primitive young-generation entry over a single from-space.
- The shared `pub(crate)` evacuation primitives: `Target` (`Foreign`/`Live`/`Forwarded`), the `EvacDest`
  trait (`classify_boxed`/`classify_cons`/`can_reserve`/`reserve`), `forward_slot<D: EvacDest>`, `move_size`,
  `for_each_ref` — consumed by `Copier`, `immix/`, the atomic promotion and `live.rs`. The per-object layout is
  **not** this crate's (Design Decisions).

### `live.rs` — the exact-live counter
- `pub(crate) fn live(from: &FromSet, roots: &[Eterm]) -> Live`; `live_full(nursery: &Nursery, mature:
  &ImmixSpace, roots) -> Live`; `pub(crate) struct Live { pub words, pub lines: usize }` — `words` = the
  exact `GcStats::words_copied` a copy would report; `lines` = the per-object `lines_for` sum of
  line-granular allocation charges (not `lines_for(words)`, which rounds the total once).
- Counts the **exactly reachable** from-set graph **without mutating it** (read-only, iterative, visited set
  keyed by pointer address), driving the copier's own `move_size`/`for_each_ref`, so it provably equals a
  real collection's `GcStats::words_copied` on the same from-set and roots. The traversal is
  `reachable.rs`'s `walk_reachable`; this module only accumulates the sizes.
- Consumers: `Collector::collect_major` and `GenImmixCollector`'s `run_major`/`grow_major`/`relocate_major`,
  to size a **fresh** mature space from the true live set (the C `adjust_after_fullsweep` `need_after`).

### `reachable.rs` — the read-only whole-heap reachability walk
- `pub struct ReachableObject<'a>` — the **safe view** of one reachable boxed object, so the caller holds no
  raw pointer: `header() -> Eterm`, `words() -> &'a [Eterm]`, `word(index) -> Option<Eterm>`,
  `len()`/`is_empty()`, `address() -> usize`. Re-exported at the crate root and in `prelude`.
- `pub(crate) fn walk_reachable<F: FnMut(Eterm, *const Eterm, usize)>(from, roots, on_object)` — the shared
  iterative worklist with its one-bit-per-word visited set: `on_object(first_word, ptr, total_words)` once
  per reachable object out of `from`'s ranges. The **single** walk both `live.rs` and the public enumeration
  are built on, so their per-object sizing and child enumeration cannot drift.
- `pub(crate) struct Visited` — the walk's one-bit-per-range visited set: a range is resolved per address by
  `Visited::index(ptr, from)`, which **delegates to `FromSet::locate`** (the one place the range geometry is
  decided), so the walk classifies an address as from-space in `O(log #ranges)`, exactly as
  `FromSet::contains` does.
- `pub(crate) fn for_each_reachable_view(...)` / `for_each_reachable(...)` — the shared body and the raw
  `(header, ptr)` form of the enumeration: every reachable **boxed object** exactly once; a cons cell is
  traversed (so the objects behind it are reached) but not reported; **layout-agnostic**. All are read-only
  (no forwarding marker), iterative (a 50 000-chain does not recurse), dedup by first-word address, and skip
  every reference outside `from` (literal, foreign space, off-heap pointer — `Target::Foreign`); the
  word-level `unsafe` lives here.

### `collector.rs` — the two-generation collector
- `pub enum CollectionKind { Minor, Major }`; `pub struct Collection { pub kind, pub stats: GcStats }`
- `pub struct Collector` — owns the young nursery `Nursery` chain (`src/nursery.rs`) + a `mature`
  `HeapSpace` and the `HeapPolicy`.
  - `new(policy)`; `with_allocator(alloc, policy) -> Result<Collector>`; `policy()`; `young()`/`young_mut()`
    (the **current**, allocatable nursery space); `mature() -> &HeapSpace`; `stats() -> GcStats`;
    `minor_gcs() -> u32` (minors since the last fullsweep, `GEN_GCS`); `impl HeapObservable`.
  - `should_collect(need_words) -> bool` — the **occupancy-only** admission signal (`current()
    .available_words() < need_words`); `collection_debt() -> bool` — the **opt-in** debt seam over
    `Nursery::retains_old_spaces`. Both mirrored on `GenImmixCollector`/`AnyCollector`.
  - `collect(roots) -> Result<Collection>` — picks minor vs major (policy), escalating a would-not-fit minor
    to a major; `collect_minor`/`collect_major(roots) -> Result<GcStats>` — each **pre-checks capacity**, so
    a destination that cannot hold the live set returns the typed `heap_exhausted` *before* `Copier::run`.
  - `ensure_capacity(need_words, roots) -> Result<Collection>` — the on-demand growth seam;
    `grow_nursery(need_words) -> Result<()>` — the **no-relocation** growth seam (both: Design Decisions;
    mirrored). `young_contains(ptr)` — whether `ptr` is a word of **any** nursery space (current or
    growth-retired): the range test the host's unscanned-root valve uses; `young_space_count() -> usize` —
    how many spaces the chain holds (read only from tests).
  - `for_each_reachable_object(roots, visit: impl FnMut(Eterm, *const Eterm)) -> Result<()>` — the **public,
    read-only reachability enumeration** (the off-heap liveness scan's marking half): every reachable **boxed
    object** across the **whole heap** (every nursery space + the mature generation's live region `[base,
    top)`), each once, as `(header_word, *const Eterm)`; `for_each_reachable_object_view(...)` is the
    **no-caller-`unsafe`** sibling (safe `ReachableObject` callback). Both mirrored.
  - After **every** collection the chain is collapsed to one space via `Nursery::collapse(policy,
    demand_words)`, which re-sizes that space via `young_after_collection(capacity, demand)` from a minor's
    **surviving** words and a fullsweep's **observed occupancy** before the copy, and a fullsweep sizes the
    fresh mature space from `live.rs`'s exact live count plus the **young heap's occupancy** — never the
    **mature** occupancy (Design Decisions).

### `immix/mod.rs` + `immix/collect.rs` — the mark-region mature space (`ImmixSpace`)
- `pub struct ImmixSpace` — line/block mark-region mature generation; geometry `LINE_WORDS` 16,
  `LINES_PER_BLOCK` 16, `BLOCK_WORDS` 256; per-block line bitmaps (`allocated`/`marked`/`kept`);
  `BlockState::{Free,Partial,AllMarked}`.
  - `new(capacity_words)`/`with_allocator(alloc, capacity_words)`; `base`/`contains`/`contains_term`;
    `capacity_words`/`block_count`/`line_count`/`used_lines`/`free_lines`/`used_words`/`available_words`;
    `counters() -> ImmixCounters`; `reset()`; `unsafe alloc_words(words) -> Result<*mut Eterm>` (line-aligned);
    `cons`/`tuple`; `can_reserve(words)`; `release_words(ptr, words)`/`release_lines(start_g, need)` — the
    inverse of the private `claim` (clear the `allocated` bits, a block whose bitmap empties back to `Free`),
    documented for undoing a *promotion's own* reservations only; `marked`/`kept`/cursors are left untouched.
  - `mark(roots, promoted) -> Result<()>`; `collect(&mut self, roots: &mut [Eterm], promoted: &mut [Eterm])
    -> Result<ImmixStats>` — mark, classify, release, evacuate `Partial` blocks, pin `AllMarked`, finalize;
    `impl EvacDest for ImmixSpace` (so the shared `copy.rs` protocol can evacuate *into* it).
- `pub struct ImmixStats` (`objects_evacuated`/`lines_evacuated`/`objects_pinned`/`lines_pinned`/
  `blocks_reclaimed`/`words_reclaimed`/`words_pinned`); `pub struct ImmixCounters`.

### `immix_collector.rs` — `GenImmixCollector` (copying nursery + mark-region mature)
- `pub struct GenImmixCollector` — the copying young `Nursery` chain paired with an `ImmixSpace` mature;
  `with_config(&GcConfig)` (uses `cfg.policy()`; the `MatureKind` is ignored — this type *is* the Immix
  collector), `from_policy(policy)`, `with_allocator(alloc, policy)`.
  - `policy()`; `young()`/`young_mut()`; `mature() -> &ImmixSpace`; `stats()`; `last_immix_stats() ->
    Option<ImmixStats>` (the most recent major's in-place reclaim accounting); `minor_gcs()`; `impl
    HeapObservable`; `should_collect`; `collection_debt()`; `collect`/`collect_minor`/`collect_major`
    (reusing `Collection`/`CollectionKind`; no new variant); `ensure_capacity`; `grow_nursery`;
    `young_contains`; `young_space_count()` — otherwise the same semantics as `Collector`.
  - `for_each_reachable_object(roots, visit)` — the mark-region sibling of `Collector`'s (same whole-heap
    walk); its only difference is the mature range, the whole `base .. base + capacity` region (with its free
    lines — over-inclusive yet harmless, as in `live_full`); `for_each_reachable_object_view` mirrors it.
  - A fullsweep that cannot fit its promotion grows the region (`grow_major`, which also rebuilds a fresh
    region of the **current** capacity when a ceiling forbids growing); only a `max_heap_words` cap that
    cannot hold the live set in any layout returns the typed `heap_exhausted`, and still **before any copy**
    (Known Gaps; coverage `src/immix_collector/grow_tests.rs`, `tests/immix_major_growth.rs`).
  - `collect` escalates a **would-not-fit minor** to a fullsweep and `run_major` falls back to `grow_major`
    on the same outcome — the promotion itself is atomic, so a refusal is a no-op to retry (Known Gaps).

### `any_collector.rs` — the strategy-dispatching seam
- `pub enum AnyCollector { Copying(Collector), Immix(GenImmixCollector) }` — a **complete drop-in for the
  concrete `Collector`**: the whole call set a host makes is present, so a host switches mature strategy by
  building an `AnyCollector`. The two mature types differ (`&HeapSpace` vs `&ImmixSpace`), so the mature
  generation is exposed as **geometry accessors** rather than a common `&`.
  - `new(policy)` (= `from_config(&GcConfig::new(policy))`, so the **automatic rule** decides);
    `from_config(&GcConfig)` (dispatches on the **resolved** `config.mature()`); `kind() -> MatureKind`;
    `mature_choice() -> MatureChoice`; `policy()`; `stats()`; `minor_gcs() -> u32`; `heap_info() -> HeapInfo`
    (`impl HeapObservable`).
  - `young()`/`young_mut()`; `should_collect`; `collection_debt()`; `collect`/`collect_minor`/`collect_major`;
    `ensure_capacity`; `grow_nursery`; `young_space_count()`; `for_each_reachable_object` /
    `for_each_reachable_object_view` — all forwarded with the selected strategy's semantics, so the
    enumeration is **strategy-agnostic** (both strategies enumerate the same nursery chain plus their own
    mature region, so the reachable-object set is identical), while `young_contains(ptr)` covers **every**
    space of the chain.
  - The **mature generation** accessors: `mature_capacity_words()`/`mature_used_words()`/
    `mature_available_words()` (mark-region figures are line-rounded — `used_lines * LINE_WORDS`, "at least
    this many words"), `mature_base()`/`mature_contains()`; `mature_contains` **reuses each concrete space's
    own range test** — the copying space covers the **live region `[base, top)`**, the mark-region space the
    **whole region including free lines** (**over-inclusive**, harmless: a word the space never hands out
    cannot be a live reference's target, and a foreign container lies outside every mature range).
  - Forwarding only — the seam owns no collection logic, so neither strategy depends on the other.

### `config.rs` + `selection.rs` — the mature-generation decision
- `pub struct GcConfig { pub policy: HeapPolicy, pub selection: MatureSelection }` — the heap-size policy
  plus **how** the mature strategy is chosen: `new`/`auto` (both = the automatic rule),
  `copying(policy)`/`immix(policy)` (explicit pins), `growable()` (= `auto(HeapPolicy::unbounded())`),
  `with_selection`/`with_mature`, `policy()`, `selection()`, `mature() -> MatureKind` (the resolved
  strategy), `mature_choice() -> MatureChoice`. `Default` = `GcConfig::new(HeapPolicy::default())` =
  `Auto`, which resolves to `Copying` for the default (233-word) policy.
- `pub enum MatureSelection { Auto, Explicit(MatureKind) }` (default `Auto`) — the *how*; `MatureKind` is
  the *strategy*; `Explicit` beats the rule in both directions.
- `pub enum MatureSource { Automatic, Explicit }`; `pub struct MatureChoice { kind, source, mature_words,
  break_even_words }` with `rule_kind()`/`is_override()`/`describe()` — the **observable** resolved choice.
- `pub const IMMIX_BREAK_EVEN_WORDS: usize = 8 * BLOCK_WORDS` (= 2048) and `IMMIX_BREAK_EVEN_BLOCKS` (= 8);
  `select_mature(mature_words) -> MatureKind` (Immix iff `>=` the break-even — the pure rule);
  `mature_words_of(policy)`; `resolve_mature(&GcConfig) -> MatureChoice`; `choice_for(kind, mature_words)`.

### `policy.rs` — heap-size policy (the `erts_next_heap_size` analogue)
- `pub struct HeapPolicy` — `min_heap_words`, `max_heap_words` (0 = unbounded), `fullsweep_after` (default
  65535), `min_bin_vheap_words` (default 46422 = `next_heap_size(32768, 0)`, the C `BIN_VH_MIN_SIZE`),
  `initial_young_words` (233), `initial_mature_words`, `need_reserve_words`; `default()`,
  `from_min_max(min, max)`, `unbounded()` (explicit `max_heap_words: 0`), builder.
- `from_spawn_opts(min_heap_words, min_bin_vheap_words, fullsweep_after)` — the C `spawn_opt` rule: each
  minimum is floored and rounded through `erts_next_heap_size(x, 0)`, so a stored minimum is a growth-table
  fixed point. `fullsweep_due(minor_gcs) -> bool` — `minor_gcs >= fullsweep_after` (the C `GEN_GCS >=
  MAX_GEN_GCS`); `fullsweep_after == 0` forces a fullsweep on every collection.
- `next_words(size, offset)`; `size_after_fullsweep(live, need)`; `mature_after_fullsweep(capacity, live,
  need)` (grow / shrink / keep — the C `adjust_after_fullsweep`); `young_after_collection(capacity, demand)`
  (the **bounded** nursery rule; a minor passes what it promoted, a fullsweep the occupancy it observed —
  it can never enlarge the nursery); `should_grow(...)`, `should_shrink(...)`.
- `pub const DEFAULT_MIN_HEAP_WORDS: usize = 233` — the C `H_DEFAULT_SIZE`.
- `pub fn heap_sizes() -> &'static [usize]` — the curated growth table (Fibonacci+1 to index 23, then +20%,
  stopping before the first entry that would exceed the 64-bit cap `cap = (1u64 << 53) / 8` = `2^50` words
  ≈ 8 PB), `LazyLock`-built from `erts_init_gc`; `pub fn next_heap_size(size, offset) -> usize` —
  `erts_next_heap_size` over it.

### `heap_info.rs` — the `garbage_collection` observables
- `pub struct HeapInfo { minor_gcs, fullsweep_after, min_heap_size, min_bin_vheap_size, max_heap_size,
  heap_size, total_heap_size }` — the `erlang:process_info(Pid, garbage_collection)` item
  (`ERTS_PI_IX_GARBAGE_COLLECTION`): `new(...)`, `values() -> [usize; 5]`, `items() -> [(&'static str,
  usize); 5]`, `of<C: HeapObservable + ?Sized>(collector: &C) -> HeapInfo`.
- `pub trait HeapObservable` — `minor_gcs()`/`heap_size_words()`/`total_heap_size_words()`/`heap_policy()`;
  implemented by `Collector`, `GenImmixCollector` and `AnyCollector` (`AnyCollector::heap_info()` too).
- `pub const GARBAGE_COLLECTION_KEYS: [&str; 5]` — the oracle's key order (`max_heap_size,
  min_bin_vheap_size, min_heap_size, fullsweep_after, minor_gcs`); defaults `min_heap_size 233`,
  `min_bin_vheap_size 46422`, `fullsweep_after 65535`, `max_heap_size 0`. Word accounting: `heap_size` = the
  nursery capacity (C `HEAP_SIZE`); `total_heap_size` = nursery + mature capacity (C `heap_sz + old heap`);
  the message buffer is `beam_message`'s (not counted here).

### `mso.rs` — off-heap refcounted large objects & refc binaries
- `RefCount` — **re-exported** from `beam_alloc` (`pub use beam_alloc::RefCount;`); the workspace's single
  `erts_refc_*` primitive (`new`, `count`, `inc`, `dec`, `dec_and_test(min)`). Both `beam_gc::RefCount` and
  `beam_gc::mso::RefCount` resolve; this crate defines no count of its own.
- `pub struct Mso` — an off-heap, immutable, refcounted shared object: `new(payload, apparent_size)`,
  `downcast_ref::<T>()`, `apparent_size()`, `refcount()`. `pub struct MsoHandle` — clonable owning handle
  (the on-heap `BinRef` analogue): `Clone` bumps the refcount, the last `Drop` runs the payload's destructor.
- `pub enum OffHeapEntry { Mso(MsoHandle), Binary(beam_alloc::RefcBinary) }` — an off-heap list entry:
  `overhead_bytes()`/`apparent_size()` (Mso → apparent size; Binary → `len()`), `as_mso()`, `as_binary()`;
  `From<MsoHandle>`/`From<RefcBinary>`.
- `pub struct OffHeap` — the per-process list (`MSO(p)`), **slot-stable for the list's whole lifetime**:
  `push` (slot-discarding append), `insert -> usize` (append returning the slot — the index an on-heap
  off-heap reference stores in its `val` word), `get(slot) -> Option<&OffHeapEntry>` (`None` also for a
  **released** slot), `iter()` (the **live** entries in slot order), `len()`/`is_empty()`/`overhead_bytes()`
  (**live** entries only), and `sweep<F: FnMut(&OffHeapEntry) -> bool>(is_live) -> usize` — releases the dead
  entries **in place** (dropping each exactly once), returning how many were released; the caller supplies
  the liveness predicate. A released slot is kept in place (as `None`), so `sweep` never shifts, reorders,
  compacts or reuses a slot — required because an on-heap off-heap reference stores the slot index in its
  `val` word (`beam_message::layout::make_off_heap_container` / `off_heap_slot`).

### `bounds.rs` — the O(1) aggregate scanned-region filter
- `pub struct ScannedBounds` — the covering half-open interval `[base, limit)` of **every** live scanned region
  of a collector (the whole nursery chain — current + every growth-retired space — **and** the mature
  generation) plus a monotone `generation` epoch: `new(base, limit)`/`at(base, limit, generation)`, `EMPTY`,
  `base()`, `limit()`, `generation()`, `is_empty()`, `may_contain(addr) -> bool`,
  `definitely_unscanned(ptr) -> bool`, `span_words()`; `pub(crate)` `covering(other)` (interval `min`/`max`,
  keeps the receiver's epoch) and `with_generation`.
- **Semantics: a fast-REJECT filter, never a containment claim.** `may_contain == false` ⇒ the address is in
  *no* scanned region (so `young_contains`/`mature_contains` are both `false`); `may_contain == true` ⇒
  nothing is known — it spans the holes between spaces, every space's unallocated tail and the mark-region
  free lines — and the caller must fall through to the exact tests. A fast accept would be a memory-safety bug.
- Built from each space's **reserved** extent (`HeapSpace::limit()`, `ImmixSpace base + capacity_words`),
  never the moving `top()`, so a cached value cannot go stale between allocations; only
  install/retire/collapse/mature-replacement move it (and the epoch).
- Exposed as `scanned_bounds() -> ScannedBounds` on `Collector` (nursery chain + copying mature),
  `GenImmixCollector` (nursery chain + mark-region region) and forwarding on `AnyCollector` — `O(1)`,
  allocation-free, `&self`, safe Rust. The consumer is `beam_rt/src/context/gc.rs::root_is_unscanned`; the
  filter's tightness depends on how close a collector's regions sit in address space (measured spans printed by
  `tests/scanned_bounds.rs --nocapture`), so it is always sound but not always profitable.

### `stats.rs` / `error.rs`
`GcStats` — `words_copied`, `words_reclaimed`, `objects_copied`, `minor_collections`, `major_collections`
(`Clone + Copy + Default + PartialEq + Eq + Debug`). `error.rs` holds `pub(crate)` allocation-failure
constructors only (no new error enum).

## Design Decisions
- **Two generations: `young` nursery + `mature`; a minor tenures young→mature (immediate promotion).** A
  **minor** Cheney-copies every live young term straight into `mature` and resets the nursery; terms being
  immutable, a mature object can never acquire a pointer to a later-allocated young object, so the
  *no-mature→young-pointer* invariant holds after every minor and mature objects are **never rescanned** —
  the generational payoff. A **major** (fullsweep) evacuates `young` **and** `mature` into one freshly-sized
  space (the new `mature`), resetting the nursery — the C `major_collection`/`full_sweep_heaps`.
- **The destination is sized and checked up front — no partial-copy rollback.** A minor runs only when
  `mature.available_words() >= young.used_words()` and the policy allows it; otherwise `collect` escalates to
  a major, which sizes a fresh space up front (`mature_after_fullsweep(capacity, live, 0)` over the **exact
  reachable live count**) *before* copying. A collection therefore never leaves the from-space
  half-forwarded.
- **The fresh mature space is sized from the EXACT reachable live set plus the young heap's occupancy —
  never the mature occupancy.** `mature_after_fullsweep(capacity, live, need)` takes `live` = `live.rs`'s
  exact reachable word count and `need` = the **young heap's occupancy** (`nursery.used_words()`), the C
  `adjust_after_fullsweep` `need_after`. The young occupancy lets the mature region receive the **next**
  nursery's promotion, so minors actually run instead of every collection escalating to a fullsweep, and it
  stays bounded (the young occupancy is capped by the nursery capacity, which `young_after_collection` keeps
  bounded), so the grow branch's target `next_words(need_after, 0)` is a fixed point. The **mature** occupancy
  must never be used — it is garbage-inflated (everything the sweep is about to reclaim), so `should_grow`
  would fire on every fullsweep and creep the capacity without bound. Evidence:
  `tests/generational_cadence.rs`, `tests/steady_state.rs`, `tests/mature_capacity_convergence.rs`;
  `MEASURED.md`.
- **`GenImmixCollector` = the same copying nursery + a mark-region mature space; its major reclaims the
  mature generation *first*.** A **minor** promotes the nursery's live terms into the `ImmixSpace` through an
  **atomic two-phase driver** (`src/immix_collector/promote.rs`, `Result<PromoteOutcome>`): phase R reserves
  each first-time live object (`ImmixSpace::alloc_words`), copies its words and writes the forwarding marker,
  but rewrites **no** reference word; phase C rewrites the recorded slots and roots only once every object of
  the traversal has been placed — the barrier-free invariant holds because promotion only ever moves young
  objects *into* mature, and a reservation that finds no run rolls both spaces back byte-for-byte
  (`PromoteOutcome::WouldNotFit`) instead of leaving a half-forwarded heap. A **major** is the prescribed
  `ImmixSpace::collect`-then-promote order, made sound for the young→mature edge an in-place reclaim cannot
  ignore: a read-only `scan_young` reports the promotion's exact **line** count and every nursery reference
  word pointing into mature (`(slot, term)` pairs); `ImmixSpace::collect` takes those terms as its
  `promoted` roots (so a mature object reachable only through the live nursery survives), each recorded
  nursery slot is rewritten to the object's final address, and only then does promotion run — by which point
  the nursery's mature references are `Foreign` and final. If `free_lines() < needed` after the reclaim the
  collector **grows the region by relocation** (`grow_major`); a typed `heap_exhausted` is returned only when
  a `max_heap_words` cap forbids the growth, and then before any copy. `words_reclaimed` = mature reclaim +
  nursery garbage. Reclaim-first (not promote-first) is what lets a would-not-fit minor escalate to a major
  that *makes room*.
- **`GenImmixCollector` ignores the `MatureKind` in `with_config`, and `AnyCollector` only forwards.** The
  type *is* the Immix-backed collector, so `with_config` always builds one over `cfg.policy()`; the strategy
  is decided by `AnyCollector::from_config` (on `config.mature()`) / `GcConfig::mature_choice` — the explicit
  pin or the automatic rule. `Collector` and every existing public signature are unchanged. The dispatch seam
  carries no collection logic, so neither strategy depends on the other: `mature_choice()` derives the
  decision from the live collector (`selection.rs`, safe Rust depending only on `config`/`policy`/`immix`),
  and its geometry accessors exist because the two mature spaces are differently typed — they reproduce, not
  reinterpret, what each concrete space answers.
- **Heap sizing is ERTS-shaped and applied at every collection.** `HeapPolicy` carries the C minimums and the
  `fullsweep_after` cadence; `from_spawn_opts` reproduces the C `spawn_opt` rounding through
  `erts_next_heap_size(x, 0)`. `Collector::collect` picks the collection with `fullsweep_due`;
  `mature_after_fullsweep` sizes the fresh mature space from `need` = the **exact reachable live word count**
  and re-uses `should_grow` (`3*cap < 4*need`) / `should_shrink` (`cap > 4*need && cap > min`), floored by
  `min_heap_words` and clamped by `max_heap_words`.
  - **The nursery re-size (`young_after_collection`) is a BOUNDED rule, not a growth rule.** `demand` is a
    **minor**'s surviving words (what it promoted) and, for a **fullsweep**, the occupancy the round
    **observed** before the copy (`collect_major`/`run_major`/`relocate_major` sample `nursery.used_words()`
    up front and pass it to `Nursery::collapse` as `demand_words`) — the C `erl_gc.c::adjust_after_fullsweep`
    `need_after = HEAP_TOP(p) - HEAP_START(p)` rule, and the same `need` input
    `mature_after_fullsweep(capacity, live, young_occupancy)` already takes. Passing `0` there collapsed a
    settled nursery to the 64-word reserve floor on every fullsweep, so the next allocation re-grew through
    `Nursery::grow` (1.00 space installs/op on the `gc_bound/churn` shape — `MEASURED.md`). With
    `request = demand.max(need_reserve_words)`, the *only* grow branch is `request > capacity_words` (a
    surviving set larger than the nursery), which steps up to offset-0 `next_words(request, 0)`; otherwise
    `should_shrink(capacity, request)` falls it back to `next_words(request, 0)`, and otherwise the capacity
    is kept. **`collapse` therefore can never grow the nursery**: `demand_words` is a space's own occupancy,
    so it is `≤` the chain's `capacity_words`, and the enlarge branch needs `request > capacity_words`; the
    sole exception is the `need_reserve_words` policy constant flooring a nursery smaller than itself. Only
    `Nursery::grow`/`ensure_capacity` enlarge, and only on an explicit request. A nursery the round left
    mostly empty therefore **shrinks** and one that turned out about right **keeps** its size — the rule the
    pressure tests depend on (a test that must keep a large nursery alive raises `min_heap_words`). The
    capacity is therefore **bounded**: a fixed live set (or a fixed demand) converges to a constant, and the
    nursery never grows merely because a collection ran (`tests/nursery_bounded.rs`,
    `tests/post_fullsweep_nursery.rs`; `MEASURED.md`).
- **The young nursery is a CHAIN of spaces (`src/nursery.rs`), and `grow_nursery` is a NO-RELOCATION seam.**
  `Collector`/`GenImmixCollector` own a `Nursery` — a **current** space plus the spaces a growth **retired**
  — rather than a single `HeapSpace`. `HeapSpace` never reallocates, so growing by *replacement* would strand
  every live word; `Nursery::grow` instead **retains** the old space and points the allocator at a fresh one,
  so every live word keeps its **original address** and nothing is moved. The step is the plain one-step
  `next_words(need_words, 0)` whenever that already enlarges the space (a genuinely larger request is
  unchanged), and only **at least** `next_words(2 * capacity, 0)` when the one-step target would install a
  space no larger than the one it replaces, so a caller that grows repeatedly at a **constant** request
  climbs the growth table geometrically — each call retires a genuinely larger space, and the doubling
  branch fires on every call whose predecessor did not already enlarge past the request — so the chain stays
  `O(log(heap))` long.
  **Containment over the chain is indexed, not scanned.** `Nursery::contains` answers from an ordered index
  of the retired spaces' live regions plus one direct test of the current space
  (`retired_regions: BTreeMap<base, top>`), so it is `O(log #spaces)` rather than a walk over every space.
  The index is exact because every space is a distinct live allocation, so the intervals `[base, top)` are
  pairwise disjoint and the greatest base at or below the probe is the only candidate (`addr < top`, the same
  half-open rule as `HeapSpace::contains`). An indexed `top` is **frozen** from the moment its space is
  retired (nothing is ever allocated in it again), and the current space — the only one whose `top` moves —
  is tested directly instead of being indexed, so no entry can go stale. That is what keeps the seam sound
  for a caller holding **no root set** — the un-rooted `beam_rt` allocation path, which must not collect
  (a copying collection may only run against a complete root set).
  `young()`/`young_mut()` keep returning the **current** (allocatable) space; `young_contains` covers
  **every** space of the chain; after any collection the chain is **collapsed to exactly one space**
  (`Nursery::collapse`, which clears both the chain and its index); `HeapObservable::heap_size_words`
  reports the **summed** nursery while a chain is live. A `max_heap_words`/growth-table ceiling that cannot
  reach the request returns the typed `heap_exhausted` and retires nothing. The chain's **length** is the
  bounded quantity; its retained **bytes** are the ladder's geometric sum, `O(requirement)` (≈ 2× the last
  installed space), and not a function of the call count. Retention/debt semantics: Known Gaps; the runtime
  pairing: Notes for Agents; the measured curve: `MEASURED.md`.
- **`xptr_tag` is not needed.** The C ORs the literal bit back in only for literal-area copies (`copy.c`);
  this collector copies only in-heap objects, so a copied pointer never carries the literal bit. A
  literal-tagged (and every other out-of-from-space) reference fails the `FromSet.contains` test and is left
  verbatim (the C `erts_is_literal`/area-test rule).
- **The copier relocates exactly the from-set-reachable graph — nothing else.** `Copier::complete`
  (`src/copy.rs`) walks only the objects it copied into to-space, forwarding each child slot the canonical
  `beam_term::layout::for_each_child_slot` walk reports; a reference the `EvacDest` classifies
  `Target::Foreign` is left verbatim and its target is **not descended into**. So a from-space word reached
  *only* through a container outside the from-set — a mature object during a **minor**, a literal, or an
  unscanned foreign region (the runtime's pooled BIF heap / pointer-stable arena / `beam_message` fragment) —
  is neither kept alive nor updated, and (the nursery being reset/replaced at every collection) such a
  pointer then reads back as reused from-space. A **major** scans young *and* mature, so it repairs a
  mature→young pointer a minor left stale; a minor never can.
- **In-place Cheney forwarding, exactly as C.** A boxed object's **header word is overwritten** with the new
  BOXED-tagged pointer (`IS_MOVED_BOXED`); a cons cell's car is set to `THE_NON_VALUE` and its cdr to the new
  LIST pointer (`IS_MOVED_CONS`). The scan reproduces the C `sweep()` over the canonical per-object walk — a
  `HEADER` word is sized by `move_size`, its copy children are forwarded by `for_each_child_slot`, and the
  object is jumped past — so `copy.rs` holds no per-shape stepping logic.
- **Funs are sized and scanned from the inlined-descriptor layout in `beam_term::fun`, *not*
  `header_arity`/`thing_arityval`.** `header_arity` deliberately returns the C `ERL_FUN_SIZE - 1 == 1` for a
  fun — the assumption that a fun block is `[header][entry-pointer][env…]`. This runtime instead **inlines**
  the C `entry` as descriptor words, so a fun block is `[header][desc_words(kind)][env[0..num_free]]`, with
  `desc_words` = 2 (`Local`), 3 (`External`: `module`·`function`·`arity`), 10 (`Imported`) and `env[i]` at
  `hp[1 + desc_words(kind) + i]`. The copier sizes it as `1 + fun_desc_words(kind) + fun_num_free` and the
  canonical walk reports exactly the `num_free` env slots, so the descriptor is skipped opaquely.
  `header_arity`/`thing_arityval` would **under-size** the copy by `desc_words(kind) - 1` words (1/2/9) and
  land the scan on a raw descriptor word, following it as a bogus term reference and corrupting the heap.
- **Per-object sizing/walking is `beam_term`'s, shared not duplicated, with one copy-size override here.** The
  per-object walk lives in exactly one place — `beam_term::layout::{object_words, for_each_child_slot}` — and
  both the copying `Copier` and the mark-region `immix/` phases consume it, so no crate re-derives a shape's
  size or its child slots; `space.rs`, `collector.rs`, `policy.rs`, `stats.rs`, `config.rs` and `error.rs`
  never inspect a term's interior, and `beam_message` keeps its own private walk. The single exception is
  `copy.rs::move_size(header, ptr)`, which is `object_words` for every shape **except** an `ErlSubBits` —
  there it returns `ERL_SUB_BITS_SIZE` (`5`) rather than the *sizing* graph's `ERL_REFC_BITS_SIZE` (`8`),
  because those are the words a copier lays down, after which forwarding `orig` copies the separate 3-word
  `BinRef`; `tests/refc_binary_copy.rs` pins the override (the pair copies as two objects totalling exactly 8
  words).
- **Root set = a flat `&mut [Eterm]`.** The C scans the whole stack *word by word* with no frame metadata (a
  word is a term iff its primary tag is BOXED/LIST and it points into a swept area), so a flat slice of root
  words (live x registers + live stack cells) is **semantically faithful**, not a shortcut. Frame-aware stack
  relocation (`copy_erlang_stack`) and the process dictionary / message-queue / saved-argument sub-rootsets
  belong to `beam_process`.
- **`HeapSpace` wraps `beam_alloc::Heap`; it is not a new allocator.** `beam_alloc::Heap` is the production
  bump heap (its `AllocKind::Heap` exhaustion means "collect"); `HeapSpace` adds only space semantics
  (`contains`, `reset`, `base`/`top`/`limit`), so collector and mutator share one allocation implementation.
- **The off-heap list is `Mso`/`MsoHandle` + `beam_alloc::RefcBinary` + `OffHeap`, not bolted onto the heap;
  `OffHeap` owns the slot↔object index.** Both payload kinds live **outside** every process heap; the
  collector **copies only the small heap-resident handle's scalar words** and leaves every off-heap pointer
  verbatim (the C `sweep_off_heap` rule). Refcount changes are the **off-heap API's** responsibility, so the
  collector never walks off-heap payloads. The count is `beam_alloc::RefCount` — the workspace's single
  `erts_refc_*` primitive, **re-exported** here; the on-heap `BinRef`/`ErlSubBits` *handle term* is
  `beam_term`'s job, while this crate owns the refcounted off-heap objects, the shared count, and the list —
  whose slot number an on-heap off-heap reference stores, stable for the list's lifetime (`mso.rs`, API
  Surface).
- **The scanned-region aggregate is cached in the nursery chain and derived, not stored, at the collector.**
  `Nursery` keeps `extent: ScannedBounds` over the **reserved** extents of every space of the chain: `grow`
  only *widens* it (`min`/`max`, `O(1)` regardless of chain length) because the retired space was already
  covered while it was current, `collapse` *re-seeds* it from the single surviving space (a stale widening
  would keep accepting the retired neighbourhood), and both advance the epoch. `Collector`/
  `GenImmixCollector` compose it with their mature extent and a separate `mature_generation` counter bumped
  where the mature space/region is actually replaced (`collect_major`'s fresh `HeapSpace`, `relocate_major`'s
  fresh `ImmixSpace`; an in-place mark-region reclaim keeps `base`/capacity and therefore does not move the
  epoch). The reported `generation` is the **sum** of the two monotone counters, so it changes whenever either
  half does. `Nursery::contains` is unchanged (`spaces().any(|s| s.contains(ptr))`), so the aggregate can
  never make it inexact. Why the reserved extent and not the live `top()`: `top()` moves on every allocation,
  which would invalidate a cached bound immediately, and the filter only ever rejects.
- **The minor→major escalation test is a CAPACITY test on the nursery's occupancy, and for the
  `gc_bound/churn` shape every escalation it takes is genuine.** `Collector::collect` escalates whenever
  `mature.available_words() < nursery.used_words()`; `tests/churn_attribution.rs` records the escalation's own
  inputs (the mature as low as **1** free word against a nursery occupancy up to 550, with the live young set
  at 464–1 353 words), so the ~0.5 majors/op the shape pays are forced by the mature generation's promoted
  garbage, not by the test being conservative. An exact-live variant
  (`mature.available_words() >= live::live(nursery_from_set, roots).words`) would remove false escalations
  elsewhere but does **not** help this shape (its live set exceeds the mature's room) and costs one extra young
  traversal per collection — so it is deliberately not adopted.

### Measured (every figure, table and caveat is in `MEASURED.md`)
- The `gc_bound/churn` shape's per-op split is recorded there (`tests/churn_attribution.rs`): **806 words
  allocated/op against 2.00 collections/op (1.50 minor + 0.50 major) in-crate and 7.00 (5.50 + 1.50) on the
  real runtime at a 376-word nursery**, with **1 269 words copied/op = 1.58× the allocated volume** and
  ≈0.70 of every admitted young word never handed out. A `perf` profile of the same shape puts this crate's
  copy loop at **4.2 %** of samples (interpreter 20 %, libc malloc/free/memmove 15 %), so the shape's dominant
  terms are the interpreted recursion, `beam_bifs`' `Vec`-based list→binary/tuple conversion and the
  per-collection space installs — not the copy loop (Handoffs).
- **Post-fullsweep nursery sizing (`gc_bound/churn`).** Sizing the young heap from the round's observed
  occupancy removes the per-op space install (growths/op **1.0004 → 0.0004**, debt admissions 0.5002 →
  0.0002) and cuts the collection count a third (**2.000 → 1.500/op**; minor 1.500 → 1.000, major 0.500
  unchanged), at the cost of a mildly higher copy volume (1 269.5 → 1 283.5 words/op) and a higher young
  occupancy (347 → 461 peak words). Paired A/B on the same `.beam` (`taskset -c 3`, N = 2 000, 15/21 wall
  rounds and 15 `task-clock` rounds, loads recorded both sides): ratio new/base **0.964 / 0.960 / 0.929**.
  The wall clock on this host is badly perturbed, so the `perf stat -e task-clock` pass is the stable signal.
  Full table and caveats in `MEASURED.md`.
- Folding the **young heap's occupancy** into the fullsweep's size request is what makes the mature
  generation reachable at the default policy — the copying default runs **147 minors : 73 fullsweeps**.
- GenImmix wins **copy work on long-lived data** but pays footprint and pause on the small-heap/high-churn
  shape, so **`MatureKind::Copying` remains the default** and Immix is a size-/latency-tolerant option
  auto-selected at ≥ `IMMIX_BREAK_EVEN_WORDS` (2048, from the construction-time `initial_mature_words`) or
  by an explicit pin (`BEAM_RT_GC_STRATEGY=immix`; `../beam_rt/tests/gc_{immix_payload,strategy}.rs`).

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only. Heap exhaustion while copying a term is
  `BeamError::Alloc(AllocFailure::new(AllocKind::Heap, bytes))` (never panic, never a crate-local error
  enum, never `unwrap()` on runtime data). `beam_term::TermError` is reconciled to `BeamError` upstream,
  so `?` crosses the boundary.
- **`unsafe` in production code is confined to the word-level code** — `copy.rs` (the Cheney copier),
  `immix_collector.rs` (the promotion driver) and `immix/` (the mark-region space's line/mark/evacuation
  walks) — plus the two narrow `NonNull`-deref / `Box::from_raw` blocks in `mso.rs`; every `unsafe fn`
  carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`beam_common::safety`), and `unsafe_op_in_unsafe_fn` stays clean. `space.rs`, `policy.rs`, `heap_info.rs`,
  `stats.rs`, `config.rs` and the `Collector`/`GenImmixCollector`/`AnyCollector` driving logic are safe Rust
  (only their `#[cfg(test)]` sections call `unsafe alloc_words`).
- **Do NOT modify `beam_term` or `beam_alloc`** — build on their public APIs only. Do not touch
  `../../../erts/` (read-only ground truth) or `../../tools/`.
- **Never break the frozen term layout or the barrier-free invariant** (a minor must not leave a
  mature→young pointer).
- Files target ~1000 lines; split by responsibility.
- Licensing Apache-2.0; deps `beam_common`, `beam_term`, `beam_alloc`, `libc` only.

## Test Strategy
- In-module `#[cfg(test)] mod tests` live in `space`, `policy`, `mso`, `heap_info`, `config`, `selection`,
  `any_collector`, `nursery`, `immix_collector` (+ `src/immix_collector/{tests,grow_tests}.rs`), `immix/tests.rs`
  and `live`; `live`'s module proves `live`/`live_full` equal a real collection's `words_copied`/line need, incl.
  the mature generation (a relocating major; a refc binary counted on-heap only). **`copy.rs` and `collector.rs`
  carry no in-module tests** — the copier is pinned by `tests/copy_correctness.rs`, the driver by
  `tests/generations.rs` / `tests/gc_pressure*.rs`.
- **Copy correctness**: `tests/copy_correctness.rs` (structure preserved, roots relocated, exactly the live size
  retained, off-from-space refs verbatim); `tests/map_copy.rs` / `tests/refc_binary_copy.rs` (the map forms and
  the 8-word refc pair, sized to an independently measured footprint); `tests/fun_copy.rs` (all three fun kinds,
  exact `1 + desc_words + num_free` sizing, the >255-arity External descriptor, descriptor opacity);
  `tests/subtag_copy.rs` (bignum, native-record instance, `ErlSubBits` match context, external port/ref, HAMT
  head node — each with its exact independent footprint).
- **Generations, cadence and sizing**: `tests/generations.rs` (`CollectionKind`, survival, empty nursery,
  escalation); `tests/steady_state.rs` (nursery + mature converge — 233/**376** Copying, 233/512 Immix);
  `tests/generational_cadence.rs` (the room-policy gate: the default policy's churn workload runs **minors**,
  `minor_gcs()` advances/resets, survivors stay intact in mature, both capacities converge — both strategies);
  `tests/mature_capacity_convergence.rs`; `tests/heap_sizing.rs` (growth/shrink bounds, clamping,
  `from_spawn_opts`, `fullsweep_after` incl. 0, the relocating Immix fullsweep, `HeapInfo` order, a system-`erl`
  cross-check that SKIPS LOUDLY); `tests/memory_overhead.rs` (per-object overhead `< 1` word, measured 0, plus an
  RSS/word-count plateau, skipped loudly off-Linux).
- **Nursery growth**: `tests/nursery_chain.rs` (`grow_nursery` preserves live terms **at their original
  address** and runs no collection; a graph spanning the growth boundary still collects under BOTH strategies;
  `young_contains` covers every space and the chain collapses after a collection; a ceiling it cannot reach
  returns the typed `heap_exhausted` and leaves the collector untouched; a grow-then-collect loop plateaus);
  `tests/containment_index.rs` (the indexed range test: 1 200 retired spaces answered exactly, both concrete
  collectors agreeing at chain lengths 1/2/3/8/64/RETIRED, a long chain claiming only its own live regions);
  `tests/nursery_growth_retention.rs` (a superseded space is retained, never freed by a growth, and is debt
  whatever its occupancy; `collection_debt()` true while a space is retained and false after a collection;
  `should_collect` stays **occupancy-only**; 400 grow + "debt ⇒ collect" cycles hold the chain at one retired
  space with a bounded plateau); `tests/nursery_bounded.rs` (a growing load plateaus the nursery + total heap; a
  garbage-heavy collection shrinks it; a larger request grows it via `ensure_capacity`);
  `tests/heap_growth.rs` (the mature generation grows while the nursery stays bounded; a too-small cap returns
  typed with **nothing copied**); `tests/reachable_objects.rs` (the enumeration over young + mature + a
  **chained** nursery, each object once, no garbage; a live refc binary's `BinRef` reported and an unreachable
  one not; a 50 000-object chain, a three-tuple ring, `AnyCollector` forwarding, and the safe view — the reached
  range geometry pinned in-module by `reachable.rs`/`space.rs`/`nursery.rs`).
- **Aggregate scanned bounds**: `tests/scanned_bounds.rs` (the safety property — a rejected address is in no
  scanned region, asserted over every region neighbourhood at chain lengths 1/2/3/8/64/1024 and for capped
  *and* differing-capacity chains, both strategies + `AnyCollector` forwarding; the aggregate's exactness
  against the reserved extents; the epoch moving exactly on growth/no-op growth/collapse/mature-replacement of
  both kinds; the over-approximation measured and printed, never asserted); in-module `bounds.rs` (the
  half-open boundaries, `EMPTY` as `covering`'s identity, `span_words`) and `nursery.rs` (the cache equals the
  chain's covering at every step).
- **Per-op attribution**: `tests/churn_attribution.rs` reproduces the `tools/bench` `gc_bound/churn` shape and
  drives it through the runtime's own admission door (`should_collect`/`collection_debt` →
  `ensure_capacity`), asserting the collection counts, the garbage share, the growth-event count and the
  bounded nursery/mature/chain capacities — the table is in `MEASURED.md`.
- **Post-fullsweep nursery sizing**: `tests/post_fullsweep_nursery.rs` (a fullsweep keeps the capacity the
  round observed, keeps the `need_reserve_words` floor over an untouched nursery, never exceeds the collected
  capacity, and converges to ONE constant capacity under a fixed demand — mutation-verified: stashing the
  `collector.rs`/`immix_collector.rs` change fails 2/4).
- **Immix**: `immix/*` in-module tests + `tests/immix_{parity,partial_block,measure}.rs` +
  `tests/gen_immix_collector.rs` (geometry/allocation/mark/pin/evacuation/accounting and the hard major cases;
  one corpus through both strategies behind one structural walker; a multi-block object);
  `tests/immix_fragmentation.rs` (the **first-reservation** refusal: every object of the corpus is 7 lines and
  the region is packed to 112/128, so the *very first* reservation is refused and the rollback's restore list
  is **empty** — same roots, no marker, counters unchanged, identical on retry — plus the 7-line/100-word and
  9-line steady states under both strategies with the live set `compare`-equal to a witness every round);
  `tests/immix_promotion_rollback.rs` (the **non-trivial** refusal: the same packed region — 112 used / 16 free
  lines, `can_reserve(2) == true`, `can_reserve(100) == false`, up-front `needed == 8 ≤ free_lines()` — with a
  mixed corpus whose 2-word tuple is **placed, copied and marked** before the 100-word tuple is refused, so
  `plan.copies` holds one entry when `rollback` runs. Pins the positive half (the released line still holds the
  placed copy's own words), the negative half (the nursery is byte-identical — a no-op rollback leaves
  `make_boxed(dst)` in the placed tuple's header), the restored `ImmixCounters` (112/16, 8 partial blocks, same
  `lines_marked`), the reuse of the released line (the small tuple alone then promotes into a leftover tail
  line), the reproducible retry, the `collect` escalation to a completing `Major` with witness-equal roots, and
  the same two outcomes through the `AnyCollector` seam. **Mutation-verified**: with `rollback` replaced by a
  no-op this file fails 4/4 while `immix_fragmentation.rs` still passes 5/5);
  `tests/immix_break_even.rs` (a geometry `Law` at every point; the 8-block constant pinned NOT one line below);
  `tests/immix_selection_curve.rs` ("moved words are object-size- and occupancy-invariant" asserted);
  `tests/immix_workloads.rs` / `tests/allocation_heavy_workload.rs` (the measurements behind `MEASURED.md`);
  `tests/immix_invariants.rs` (mutation-verified `run_major` ordering, line-bitmap consistency, no double-free,
  idempotent repeats); `tests/immix_stress.rs` (multi-block promotion, fragmentation/reuse, repeated-minor
  promotion, a mixed workload equal under both strategies); `tests/immix_major_growth.rs` (a growable heap grows
  the region instead of erroring; a `max_heap_words` cap yields the typed `heap_exhausted` before any copy);
  `tests/mature_selection.rs` (the rule + the explicit override both ways + a threshold-crossing corpus);
  `tests/production_dispatch.rs` (one process-shaped workload through both strategies in **lockstep** — identical
  kinds, structurally equal roots — with the accessors agreeing with the concrete spaces and the mark-region
  range test the over-inclusive one). Pauses are printed only, never asserted.
- **Barriers, adversarial cases and off-heap**: `tests/fill_barrier.rs` (a too-small destination returns typed
  **before any copy**, counters byte-identical and identical on retry; `ensure_capacity` fails fast under a cap);
  `tests/collector_adversarial.rs` (break-attempts through both strategies + the primitive path: deep nesting
  proves the copier **iterative**, an exact-capacity boundary, off-heap frees exactly once);
  `tests/cyclic_safety.rs` (`size_object_bounded`'s typed `TermGraphCyclic`, never a hang, + each cyclic object
  copied once); `tests/retained_vs_live.rs` (copying hits `mature_used == live`, Immix ≤4x live, RSS plateau
  ≤1.5x); `tests/stress_cross_generation.rs` (one mixed graph through 6 minors + 2 majors under both strategies:
  structural equality after *each* collection, sharing, cycle closure, dropped garbage);
  `tests/gc_pressure.rs` / `_structures` / `_offheap` (many minors/majors with the nursery empty and the roots
  intact after *each* collection, HAMT promotion, shared/cyclic graphs, off-heap binaries freed once);
  `tests/offheap_liveness.rs` (an off-heap handle inside a moved container: refcount stable, payload verbatim,
  handle relocated, freed once); `tests/offheap_slot_stability.rs` (a `val`-slot container still resolves after a
  **middle** sweep, released slots answer `None` and are never reused); `tests/offheap_steady_state.rs` (exact
  `live`/`live_full` agreement with a real `collect_major` in place and on relocation, `.lines` load-bearing).
- **Per-shape copy coverage.** Sizing/walk is `../beam_term`'s `layout::{object_words, for_each_child_slot}`;
  direct evidence exists for every shape (cons/tuples, flat + HAMT maps, the refc pair, all three fun kinds,
  bignums, a native-record instance, an `ErlSubBits` match context, external ports/refs, a HAMT head node, a
  mixed cross-generation corpus); `memory_overhead.rs` cross-checks `object_words` against the bump
  allocator's real consumption.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Cross-checks against the real system ERTS belong to `../tools/differential/`, not here.

## Known Gaps (intentionally NOT covered in Milestone-1)
- **The mark-region strategy is reachable in production; the DEFAULT remains the copying `Collector`.**
  `beam_process::Process` owns an `AnyCollector` built from the spawn policy (`AnyCollector::new(policy)`),
  exposes it as `collector() -> &AnyCollector`, and `beam_rt` drives it at its heap-admission point
  (`src/context/gc.rs::{ensure_heap, ensure_heap_full}` → `Process::collect` / `ensure_capacity`), reading the
  mature generation through two geometry accessors (`mature_contains` in the unscanned-root valve,
  `mature_used_words` in the memory hook; `mature_capacity_words`/`mature_base`/`mature_available_words` are
  read only from tests). `selection::select_mature` chooses `Immix` only at/above `IMMIX_BREAK_EVEN_WORDS`,
  which the default 233-word policy never reaches — so `GcConfig::default()` / `AnyCollector::new(default
  policy)` build the copying `Collector` (the corpus gate reports 0 mismatches); Immix needs a large
  configured mature generation (the automatic rule — `beam_rt`'s default-OFF `BEAM_RT_GC_STRATEGY=immix`
  opt-in does exactly that) or an explicit pin. Either way the **mature generation is genuinely exercised**:
  the fullsweep room policy sizes the fresh mature for a nursery's worth of promotion, so the default really
  runs minors (the churn gate's **147 minors : 73 fullsweeps**), and Immix runs the same cadence once its
  region leaves promotion room (a region-granularity consequence, not a sizing gap; `MEASURED.md`).
- **The Immix region never reallocates in place; the collector grows it by relocation.** `ImmixSpace` wraps
  a `beam_alloc::Heap` and is never resized in place; a major replaces the whole region — the live set is
  relocated into a fresh `ImmixSpace` sized by the policy (`relocate_major`), or a larger region when an
  in-place fullsweep's reclaim cannot fit the nursery's promotion (`grow_major`). A growable heap therefore
  never fails a collectable collection; only a `max_heap_words` ceiling the promotion cannot fit returns the
  typed `heap_exhausted` (before any copy).
- **A retired nursery space is freed only by a collection.** `Nursery::grow` (`src/nursery.rs`) supersedes
  the space the mutator was allocating in **without collecting and without moving a term**, so a live term
  may still name into it; it therefore keeps that space addressable as a new link of the chain
  (`retired: Vec<HeapSpace>`), whatever its occupancy — a superseded space is never dropped, and dropping the
  provably-empty (`used_words() == 0`) case would be sound but changes `young_space_count()`, so it is
  deliberately not done. The **only** thing that frees a retired space is `Nursery::collapse`
  (`retired.clear()`, run at the end of every `collect_minor`/`collect_major`), because the collection's
  from-set covers every space (`Nursery::spaces`) and has copied everything reachable out.
- **The chain's *collection debt* is an opt-in seam; `should_collect` stays occupancy-only.**
  `Nursery::retains_old_spaces` is the chain's debt query, exported on `Collector`, `GenImmixCollector` and
  `AnyCollector` (forwarding) as `collection_debt() -> bool`. A `true` answer means *a collection is required
  to reclaim the chain — a heap-admission door should treat it like a full nursery*. It is deliberately
  **not** ORed into `should_collect` (which reports `current().available_words() < need_words` only): a door
  that consults it must first **admit** the collection. When it does — a grow + "debt ⇒ collect" admission
  loop — `collapse` releases the chain at the next admission and the retention is **one space at most**,
  however many cycles run (measured: 400 grow+admit cycles with the debt claimed → peak chain **2**
  spaces with **932** words retained = the largest single space installed; `MEASURED.md`); the cost that
  makes it opt-in: Handoffs.
- **A refused promotion is ATOMIC; `collect` recovers, `collect_minor` does not.** Line-granular allocation
  needs a *contiguous* run inside one 16-line block (`find_run`/`first_hole`), so a fragmented region can
  refuse an object although region-wide `free_lines()` reports room; the promotion driver
  (`src/immix_collector/promote.rs`) then rolls BOTH spaces back byte-for-byte (each source restored from its
  own copy, each reservation un-claimed via `ImmixSpace::release_words`) and reports `PromoteOutcome::
  WouldNotFit` — no marker, no rewritten root, no counter advanced. `collect` escalates that to a fullsweep
  and `grow_major` rebuilds a fresh region (fragmentation-free by monotone packing), even at the *current*
  capacity when a ceiling forbids growing; only a ceiling that cannot hold the live set in ANY layout reports
  the typed heap exhaustion, and still before any word is copied. `collect_minor`'s own public contract
  reports the typed error rather than escalating — escalation is `collect`'s job. The public-route evidence
  for the *placed-copy* rollback is `tests/immix_promotion_rollback.rs`; the packed-only corpus in
  `immix_fragmentation.rs` refuses the **first** reservation, so its restore list is empty and its
  byte-identity would equally hold for a no-op rollback.
- **The C `high_water` single-block layout, per-object tenuring (`GEN_GCS`/`MAX_GEN_GCS`) and the
  `F_NEED_FULLSWEEP` flag protocol** — replaced by the simpler two-space young/mature scheme above.
- **Dirty-scheduler escape for long collections** (`ERTS_POTENTIALLY_LONG_GC_HSIZE`).
- **Frame-aware stack relocation** (`copy_erlang_stack`), the process dictionary, on-heap message/signal
  queues and saved BIF/NIF arguments as sub-rootsets — owned by `beam_process`; this crate takes a flat root
  slice.
- **Literal-area collection** (`erts_garbage_collect_literals`, literal-area purge/`xptr_tag`) — literals are
  treated as permanent and left verbatim.
- **A.6 runtime heap options** (`+hms`, `+hmbs`, …) — this crate exposes `HeapPolicy`, not the CLI surface
  (owned by `beam_rt`).
- **`OffHeap` is on the production path as both the MSO slot store and the process's binary store; its
  liveness predicate is caller-driven.** `OffHeap` holds both MSO and refc-binary entries and IS the
  production MSO slot store inside `beam_message::OffHeapTable` (which stores the slot index an on-heap
  container names). `beam_process::Process` additionally owns a dedicated `OffHeap` (`owned_binaries`) and
  sweeps it as the last step of every rooted collection, in **two halves**: a **reachability-driven** half
  (`sweep_off_heap_binaries_reachable` — enumerate the whole heap through `for_each_reachable_object`, record
  each reachable `BinRef` payload pointer, release every owned payload no reachable `BinRef` names; it
  **declines** — retains everything — for a process with `has_unpublished_roots()`) and a **conservative**
  half (`sweep_off_heap_binaries`, releasing exactly the entries the caller declared dead). **This crate's
  half — the marking walk — is public and layout-agnostic:** it reports *which* objects are reachable, never
  what they contain, so the caller applies its own `ErlSubBits`/`BinRef` subtag test and reads the payload
  pointer word, and this crate takes no dependency on `beam_message`'s container interior.

## Handoffs (owned by other nodes)
- **The `gc_bound/churn` residual is not this crate's.** The major (fullsweep) count stays capacity-forced by
  the mature generation's promoted garbage, and this crate's copy loop is only **4.2 %** of a `perf` profile of
  the shape; the remaining dominant costs are the interpreter tier (20 %), libc malloc/free/memmove (15 %),
  `beam_rt::context::gc` (12.7 %) and `beam_bifs`' `Vec` churn (4.9 %): `beam_bifs/src/binconv.rs:335-382`
  (`list_to_binary` builds two `Vec`s), `beam_bifs/src/term_ops.rs:84-139` (`proper_list` `Vec` for
  `list_to_tuple`), `beam_bifs/src/arith.rs:648-650` (`Big::from_i64` boxes a `Vec<u64>` per small-int
  operand), and `beam_rt/src/context/gc.rs:667-675` (`collect_or_grow` re-installing a space per op — now
  ≈0 for this shape). Owners: `beam_bifs` / `beam_engine` / `beam_rt`.
  The growth ladder also keeps only ~40–60 words of slack, so the nursery settles at 609 words where the op
  allocates 806; the collection count improves by a third but not more. The one-step-then-doubling rule is
  not implicated (`peak_spaces = 1`).
- **The strategy switch is adopted; the residual is an explicit pin on the process-construction path.**
  `beam_process::Process` holds an `AnyCollector` and exposes `collector() -> &AnyCollector`, with
  `run_rooted_collection` bound as `FnOnce(&mut AnyCollector, &mut [Eterm])` (its consumer sites and the
  geometry accessors it reads: Known Gaps). `ProcessTable::spawn` takes only a `HeapPolicy`, so the strategy
  is always resolved by the automatic rule (`MatureSource::Automatic`); an explicit
  `GcConfig`/`MatureSelection` pin would need a parameter on the process-construction path (owner:
  `beam_process`).
- **`beam_process` should adopt the safe reachability view (`for_each_reachable_object_view`).** Its
  `sweep_off_heap_binaries_reachable_from` still drives `AnyCollector::for_each_reachable_object`, whose raw
  `(header, ptr)` pairs force the single production `unsafe { *ptr.add(1) }` word read in its
  `reachable_bin_ref_payload` probe; the view removes it (`for_each_reachable_object_view(roots, |obj| { if
  obj.header() == HEADER_BIN_REF { if let Some(val) = obj.word(1) { reachable.insert(val.raw()); } } })`) —
  zero caller `unsafe`. Owner: `beam_process`.
- **The growth seam is adopted by the runtime.** `beam_rt/src/context/gc.rs`'s shared `collect_or_grow`
  (behind both admission doors) grows the running process's nursery through `Process::ensure_capacity` when
  the policy admits the request and otherwise collects, and `Process::deliver` retries a refused copy the
  same way; a bounded `+hmax N` policy the request cannot reach still refuses, and that exhaustion raises
  `error:system_limit`. `HeapPolicy::unbounded()`/`GcConfig::growable()` express "no cap".
- **A `run` process collects at its own heap admissions; the retained chain is reachable only through the
  un-rooted allocation seam.** A `$TMPDIR`-built probe under `beam_rt run` (an `ets:insert`/`ets:lookup` loop
  over a 5-field payload map) shows `minor_gcs` advancing ≈ `N/3` and a `process_info(memory)` **flat** in N
  (identical bytes at N = 100 and N = 20 000), so nothing retains per iteration on that route and an explicit
  `erlang:garbage_collect()` adds no collection of its own. A chain therefore only builds up on the paths
  that never reach a heap-admission point (the un-rooted `alloc_words` growth seam, a declined valve, a
  `relocation_exempt`/halted/disabled process), where the door may consult `collection_debt()` before it can
  collect at all. Owner of the door: `beam_rt`.
- **Test-id / MSO / `compare` caveats (cross-crate).** A pid is a heap term via
  `beam_term::thing::write_external_pid` (4 words) **and** `beam_term::compare::eq` orders it
  **structurally through its parts** (field order per the C `erts_cmp`), with `external_pid_parts` available
  to read the parts directly; `compare::eq` still **recurses forever on a cycle**. An MSO has no on-heap
  reference *term*: the safe reference is the `OffHeap::insert -> slot` index, never a raw pointer. Reach a
  refc binary's `BinRef` via the `ErlSubBits` `orig` slot, not pointer adjacency. Covered by
  `tests/stress_cross_generation.rs`.
- **`HeapInfo` is surfaced by the runtime.** `beam_rt` builds it directly (`HeapInfo::new` and
  `HeapObservable`) for `erlang:process_info(Pid, garbage_collection)` and `system_info(garbage_collection)`,
  in `GARBAGE_COLLECTION_KEYS` order (`beam_rt/src/native/memory/mod.rs`, `src/info/memory.rs`);
  `AnyCollector` is the production collector type, but `AnyCollector::heap_info()` itself is still called
  only from tests — the runtime derives `HeapInfo` from the `HeapObservable` items directly.
- **Repo-root owned** (`../../CHANGE.md`): the mature-generation room policy and the measured GenImmix
  decision are recorded (Milestones 6 and 9); the `HeapPolicy` field surface
  (`min_bin_vheap_words`/`fullsweep_after`) and the `HeapInfo` observables are still owed an entry.

## Notes for Agents
- **`minor_gcs` is NOT monotone — it is `minor_since_major`, and a major RESETS it to 0** (the only increment sites
  are `collector.rs`'s `collect_minor` and `immix_collector.rs`'s `run_minor`; both majors — `collect_major`,
  `run_major`'s in-place path and `relocate_major` — assign 0). Consequence for any observational forensics: a
  **constant non-zero** `minor_gcs` read at two different workloads proves *no collection of either kind ran in
  between* (a minor would have raised it, a major would have zeroed it); a major-only workload reports `0`, never a
  stale count. The monotone lifetime counters are `GcStats::{minor_collections, major_collections}` — reachable via
  `Collector::stats()`/`AnyCollector::stats()`, but **not** part of `HeapInfo`/`process_info(P, garbage_collection)`,
  whose only collection counter is `minor_gcs`.
- **`heap_size`/`total_heap_size` are CAPACITIES (occupancy-independent), so a flat footprint is NOT evidence that
  collections are running.** `HeapObservable::heap_size_words` = `Nursery::capacity_words` (summed over the chain),
  `total_heap_size_words` = nursery + mature capacity; neither moves while words are handed out — only a collection
  (`Nursery::collapse`) or a `grow_nursery`/`ensure_capacity` growth changes them. A collection that runs with a
  small live set *shrinks* them (`young_after_collection`'s shrink branch: `capacity > 4 * live.max(64)` ⇒
  `next_words(live.max(64), 0)`, i.e. ~90 words under the floor-less default policy; `collect_major` always passes
  `live_words = 0`), so a steady state under continuous collection shows a **converged constant** capacity plus a
  **rising** `minor_gcs`. A constant capacity with a *frozen* `minor_gcs` means no collection.
- **Address-level membership is public; enumeration of the nursery chain is NOT.** From outside the crate a caller reaches only the **current** nursery space (`young() -> &HeapSpace`), a space **count** (`young_space_count()`) and a membership test over **every** space (`young_contains(ptr)`); the `Nursery` chain itself (`spaces()`, the retired spaces' base/top) is `pub(crate)`, so a grown chain's individual ranges cannot be enumerated externally. The mature side is fully addressable (`mature_base`/`mature_capacity_words`/`mature_used_words`/`mature_contains` on `AnyCollector`; `Collector::mature() -> &HeapSpace` / `GenImmixCollector::mature() -> &ImmixSpace`).
- **There is no public from-space/"retired spaces"/epoch accessor.** `FromSet` is public and constructible (`new`/`add(&HeapSpace)`/`contains`/`words`) but `ranges()` is `pub(crate)` and the collector builds its from-set internally; a major **drops** the old mature space (`self.mature = fresh`) — so a collection's from-space ranges must be captured from `young()`/`mature_base()` **before** it runs, never queried after.
- **The single-object size helper is `beam_term::layout::object_words(header, ptr) -> usize` (`pub unsafe`, in `beam_term::prelude`), not `beam_gc::copy::move_size`.** `move_size` is `pub(crate)` and equals `object_words` except for the `ErlSubBits` override (5 vs 8 — Design Decisions). `beam_term::size::size_object(Eterm)` is the whole-graph (public) size; the per-object reference walk `for_each_ref` is likewise `pub(crate)`, and the public enumerator is `beam_term::layout::for_each_child_slot(header, ptr, visit)`.
- **The nursery chain is monotonic *between collections*.** `Nursery::grow` holds the sole `retired.push` and `Nursery::collapse` (the sole `retired.clear()` + `retired_regions.clear()`) runs **only** from `collect_minor`/`collect_major`, so a loop that never reaches a heap-admission point keeps growing `retired` while a loop whose admission door consults `collection_debt()` collapses it back to exactly one space each round.
- **`ensure_capacity` keeps *both* spaces.** It collects first (which collapses the chain to 1) and then `Nursery::grow` retires that just-created space (chain back to 2); the retired space is freed only by the next collection's `collapse`. `grow_nursery` is the no-collect, no-relocation form with **no** relocation-exempt guard, and it reports that retention through `collection_debt()`. The runtime pairs the two (`beam_rt`'s `collect_or_grow` / `Process::deliver`), so `young_space_count` oscillates ≈`1`→`2`→`1` rather than accumulating.

- **Reading the counters for a leak-vs-recycle diagnosis.** `heap_size_words()` (the runtime's
  `erlang:process_info(P, heap_size)`) is the **summed** capacity of the nursery chain, so every
  `grow_nursery` retirement is visible in it: a value that stays constant across N iterations proves **no
  uncollected retirement is outstanding** (the chain is one space), without reading `collection_debt()`.
  `minor_gcs()` is **not** cumulative — it is minors *since the last fullsweep* and is zeroed by every
  major ([`Collector::collect_major`], `GenImmixCollector`'s major), so a stream of majors keeps it low;
  the cumulative counts are `GcStats::minor_collections`/`major_collections` through `stats()`, which
  `HeapInfo`/`process_info(P, garbage_collection)` does **not** expose (its 5 keys are `max_heap_size`,
  `min_bin_vheap_size`, `min_heap_size`, `fullsweep_after`, `minor_gcs`). **No counter of this crate
  advances on the `grow_nursery` path** except `young_space_count()`, `collection_debt()` and the summed
  `heap_size_words()` — `stats()` and `minor_gcs()` are untouched, and a growth grows no cumulative
  "allocated words" figure (this crate has none). The only counter-free reclamation is the bump-pointer
  rewind `HeapSpace::reset`/`Heap::reset`/`Heap::reset_to`, reachable through `young_mut().heap_mut()`
  and used by `beam_rt` per BIF call on its *pooled* BIF heap — never on a process heap (a word rewound
  that way is simply handed out again, with no collection, no chain change and no counter).
- **Address stability under growth vs. collection.** A `grow_nursery` always installs a **new**
  `HeapSpace` (so `young().base()` changes and the replaced base joins the non-enumerable chain);
  `Nursery::collapse` keeps the base when the policy's target capacity equals the current one (it only
  calls `reset`), and installs a new space (new base) only when the target differs.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/space.rs` → `HeapSpace` (copying semi-space over `beam_alloc::Heap`) + `FromSet` (its base-sorted
  ranges and the binary-search `locate`)
- `src/nursery.rs` → `Nursery`: the young nursery **chain of spaces** (`grow`/`collapse`/`spaces`/
  `contains`/`add_to_from_set`/`used_words`/`capacity_words`), its indexed containment
  (`retired_regions: BTreeMap<base, top>`), its cached reserved-extent aggregate (`extent: ScannedBounds`,
  `bounds()`) and its debt query (`retains_old_spaces`)
- `src/bounds.rs` → `ScannedBounds`: the O(1) aggregate extent over every scanned region (nursery chain +
  mature) and its geometry epoch — the fast-reject filter a host's unscanned-root valve calls
- `src/copy.rs` → the Cheney copy engine: `Copier`, `forward_roots`/`complete`, `collect` + the
  copy-specific `pub(crate)` primitives
- `src/collector.rs` → the two-generation `Collector` driver (see **API Surface**)
- `src/live.rs` → the exact-live counter (`live`/`live_full`, `Live { words, lines }`)
- `src/reachable.rs` → the read-only whole-heap walk: `ReachableObject`, `walk_reachable`,
  `for_each_reachable_view`/`for_each_reachable` + the `Visited` set whose `index` delegates to
  `FromSet::locate`
- `src/config.rs` → `GcConfig`, `MatureKind`, `MatureSelection`, `mature()`/`mature_choice()`, `growable`
- `src/selection.rs` → the automatic rule: `IMMIX_BREAK_EVEN_WORDS`/`IMMIX_BREAK_EVEN_BLOCKS`,
  `select_mature`, `mature_words_of`, `resolve_mature`/`choice_for`, `MatureSource`, `MatureChoice`
- `src/immix/mod.rs` → `ImmixSpace` geometry, block metadata, line-granular allocation, the `release_*`
  rollback primitives, `ImmixStats`/`ImmixCounters`
- `src/immix/collect.rs` → `ImmixSpace` mark/classify/evacuate/finalize phases + its `EvacDest` impl
- `src/immix_collector.rs` → `GenImmixCollector`: `scan_young`, fullsweep sizing via `live_full`,
  `grow_major`/`relocate_major`, `ensure_capacity`, the collect/collect_minor/collect_major driver
- `src/immix_collector/promote.rs` → the ATOMIC two-phase promotion driver (`promote`, `PromoteOutcome`)
- `src/immix_collector/{tests,grow_tests}.rs` → `GenImmixCollector`'s in-module promotion + `grow_major` tests
- `src/any_collector.rs` → `AnyCollector`: the `Copying`/`Immix` dispatch over one API — the drop-in for the
  concrete `Collector` incl. the mature geometry accessors
- `src/policy.rs` → `HeapPolicy`, the curated `heap_sizes` table, `next_heap_size`, `from_spawn_opts`/
  `fullsweep_due`/`mature_after_fullsweep`/`young_after_collection`, `unbounded`
- `src/heap_info.rs` → `HeapInfo`, `HeapObservable`, `GARBAGE_COLLECTION_KEYS`
- `src/mso.rs` → re-exported `RefCount`, `Mso`, `MsoHandle`, `OffHeapEntry`, `OffHeap` — the **slot-stable**
  off-heap list over off-heap refcounted large objects **and** refc binaries
- `src/stats.rs` → `GcStats`; `src/error.rs` → `pub(crate)` GC allocation-failure constructors
- `tests/` → the integration test files enumerated in **Test Strategy** above (each line gives the contract it pins), incl. `tests/containment_index.rs` (the indexed nursery range test), `tests/scanned_bounds.rs` (the aggregate fast-reject filter), `tests/immix_promotion_rollback.rs` (the placed-copy rollback) and `tests/churn_attribution.rs` (the churn shape's per-op counters)
- `../../../erts/emulator/beam/erl_gc.c` → the collector being mirrored (`move_boxed`/`move_cons`/`sweep`/
  `do_minor`/`full_sweep_heaps`/`adjust_after_fullsweep`) (read-only)
- `../../../erts/emulator/beam/copy.c` → term traversal reference (`size_object_x`/`copy_struct_x`) (read-only)
- `../../../erts/emulator/beam/erl_gc.h` → forwarding macros (`IS_MOVED_*`, `follow_moved`, `ErtsInYoungGen`)
  (read-only)

## See Also
- `MEASURED.md` → **all** measurement figures and caveats for the mature-generation strategies (the allocation-heavy workload, the break-even sweep, the selection curve, the multi-workload measurement, heap sizing, and the end-to-end `ets_copy` A/B RSS/latency).
- `../CONTEXT.md` → crate-layer contracts, seam types and the wave-1 dependency position.
- `../../CONTEXT.md` → workspace contracts (§5 GC/memory model, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D ("Memory management in Rust") and §H (Stage 4).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → foundations this crate builds on (do not modify).
- `../beam_process/CONTEXT.md` → the primary consumer that drives collections at safepoints.
