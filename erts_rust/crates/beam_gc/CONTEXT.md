# `beam_gc` — Per-Process Garbage Collector (Stage 4)

## Intent
Owns the **per-process collector** of the Rust ERTS rewrite: a **generational semi-space Cheney
copying** collector over the per-process heap, **barrier-free by construction** (Erlang terms are
immutable ⇒ *no term on the mature heap may refer to a term on the young heap*, so the mutator pays
zero per-store overhead), plus the off-heap **refcounted large-object (MSO)** interface and the
process **heap-size policy**. Implements the GC half of `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D and
the Stage-4 row of §H; reproduces the collector semantics of the legacy C (`../../erts/emulator/beam/erl_gc.c`
+ `copy.c`) **without** its C structure. Consumed by `beam_process`, which drives collections at
safepoints; depends downward only on `beam_common`, `beam_term`, `beam_alloc`, `libc` (wave 1).

Contracts honoured here (crate-layer `../CONTEXT.md`): the **root-set contract** (`x_reg_array[0..Live)`
+ the whole stack, **no frame liveness maps**), the **frozen term layout** (§2 — build/inspect only
through `beam_term`), and the **return-based error strategy** (§3). MMTk and Boehm are **rejected**;
GenImmix is a **measurement-gated** mature-generation option (`immix/` + `immix_collector.rs`),
selected through `GcConfig`/`AnyCollector` by the **measurement-derived automatic rule**
(`selection.rs`), which keeps the copying space for every configurable heap size that is not a
*large, long-lived* mature generation (Design Decisions).

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
  `pub(crate)` `add_range(lo, hi)` and `ranges()` the exact-live counter walks.

### `copy.rs` — the Cheney copy engine (the unsafe core)
- `pub struct Copier<'a>` — copies live terms from a `FromSet` into a `HeapSpace`, mirroring the C
  `move_boxed`/`move_cons`/`sweep` (`erl_gc.c:2280-2358`): `new(from, to)`; `forward_roots(&mut [Eterm])`;
  `complete(self) -> Result<GcStats>` (the Cheney scan); `run(self, roots)` = `forward_roots` + `complete`.
- `pub fn collect(from: &HeapSpace, to: &mut HeapSpace, roots: &mut [Eterm]) -> Result<GcStats>` — the
  primitive young-generation entry over a single from-space.
- The shared `pub(crate)` evacuation primitives: `Target` (`Foreign`/`Live`/`Forwarded`), the `EvacDest`
  trait (`classify_boxed`/`classify_cons`/`can_reserve`/`reserve`), `forward_slot<D: EvacDest>`, `move_size`,
  `for_each_ref` — consumed by `Copier`, `immix/`, `PromoteDest` and `live.rs`. **The per-object layout is
  not this crate's:** `move_size`/`for_each_ref`/`Copier::complete` all drive
  `beam_term::layout::{object_words, for_each_child_slot}`, and fun blocks are sized/scanned from
  `beam_term::fun`'s inlined descriptor, **never** `header_arity`/`thing_arityval` (Design Decisions).

### `live.rs` — the exact-live counter
- `pub(crate) fn live(from: &FromSet, roots: &[Eterm]) -> Live`;
  `pub(crate) fn live_full(nursery: &Nursery, mature: &ImmixSpace, roots: &[Eterm]) -> Live`;
  `pub(crate) struct Live { pub words: usize, pub lines: usize }` — `words` = the exact
  `GcStats::words_copied` a copy would report; `lines` = the per-object `lines_for` sum line-granular
  allocation charges (not `lines_for(words)`, which rounds the total once).
- Counts the **exactly reachable** from-set graph **without mutating it**: read-only, iterative (a worklist
  — 50 000 cons cells without recursing), with a visited set keyed by the pointer word address. Built on the
  copier's own `move_size`/`for_each_ref`, so it provably equals the `GcStats::words_copied` of a real
  collection on the same from-set and roots.
- Consumers: `Collector::collect_major` and `GenImmixCollector`'s `run_major`/`grow_major`/`relocate_major`,
  to size a **fresh** mature space from the true live set (the C `adjust_after_fullsweep` `need_after`).

### `collector.rs` — the two-generation collector
- `pub enum CollectionKind { Minor, Major }`; `pub struct Collection { pub kind, pub stats: GcStats }`
- `pub struct Collector` — owns the young nursery `Nursery` chain (`src/nursery.rs`) + a `mature`
  `HeapSpace` and the `HeapPolicy`.
  - `new(policy)`; `with_allocator(alloc, policy) -> Result<Collector>`
  - `policy()`; `young()`/`young_mut()` (the **current** nursery space, where the mutator allocates);
    `mature() -> &HeapSpace`; `stats() -> GcStats`; `minor_gcs() -> u32` (minors since the last fullsweep,
    `GEN_GCS`); `impl HeapObservable`
  - `should_collect(need_words) -> bool` — true when the nursery cannot satisfy `need_words` more
  - `collect(&mut self, roots: &mut [Eterm]) -> Result<Collection>` — picks minor vs major (policy),
    escalating a would-not-fit minor to a major
  - `collect_minor`/`collect_major(&mut self, roots) -> Result<GcStats>` — each **pre-checks capacity**: a
    destination that cannot hold the live set returns the typed `heap_exhausted` *before* `Copier::run`, so a
    failed collection never leaves the from-space half-forwarded
  - `ensure_capacity(&mut self, need_words, roots) -> Result<Collection>` — the **on-demand growth seam**
    (Design Decisions); mirrored on `GenImmixCollector`/`AnyCollector`
  - `grow_nursery(&mut self, need_words) -> Result<()>` — the **no-relocation** growth seam: enlarges the
    nursery so a `need_words`-word allocation can proceed **without collecting and without moving a single
    live term** (safe for a caller holding **no root set**); a policy ceiling that cannot reach the request
    returns the typed `heap_exhausted` and leaves the collector untouched (Design Decisions)
  - `young_contains(ptr) -> bool` — whether `ptr` is a word of **any** nursery space (current or one a growth
    retained); `young_space_count() -> usize` — how many spaces the chain holds (`1`, or more after a growth
    retained one, back to `1` once a collection collapses it). The nursery-range test + chain observable a
    host's unscanned-root valve needs
  - After **every** collection the chain is collapsed back to one space via `Nursery::collapse`, which re-sizes
    that space via `young_after_collection` from the round's
    **surviving** words, and a fullsweep sizes the fresh mature space via `mature_after_fullsweep` from
    `live.rs`'s **exact reachable live word count** — never the pre-collection occupancy (Design Decisions).

### `immix/mod.rs` + `immix/collect.rs` — the mark-region mature space (`ImmixSpace`)
- `pub struct ImmixSpace` — line/block mark-region mature generation; geometry `LINE_WORDS` 16,
  `LINES_PER_BLOCK` 16, `BLOCK_WORDS` 256; per-block line bitmaps (`allocated`/`marked`/`kept`);
  `BlockState::{Free,Partial,AllMarked}`.
  - `new(capacity_words)` / `with_allocator(alloc, capacity_words)`; `base`/`contains`/`contains_term`;
    `capacity_words`/`block_count`/`line_count`/`used_lines`/`free_lines`/`used_words`/`available_words`;
    `counters() -> ImmixCounters`; `reset()`
  - `unsafe alloc_words(words) -> Result<*mut Eterm>` (line-aligned); `cons`/`tuple`; `can_reserve(words)`
  - `mark(roots, promoted) -> Result<()>`; `collect(&mut self, roots: &mut [Eterm], promoted: &mut [Eterm])
    -> Result<ImmixStats>` — mark, classify, release, evacuate `Partial` blocks, pin `AllMarked`, finalize
  - `impl EvacDest for ImmixSpace` — so the shared `copy.rs` forwarding protocol can evacuate *into* it
- `pub struct ImmixStats` (`objects_evacuated`/`lines_evacuated`/`objects_pinned`/`lines_pinned`/
  `blocks_reclaimed`/`words_reclaimed`/`words_pinned`); `pub struct ImmixCounters`.

### `immix_collector.rs` — `GenImmixCollector` (copying nursery + mark-region mature)
- `pub struct GenImmixCollector` — the copying young `Nursery` chain paired with an `ImmixSpace` mature.
  - `with_config(&GcConfig)` (uses `cfg.policy()`; the `MatureKind` is ignored — this type *is* the Immix
    collector); `from_policy(policy)`; `with_allocator(alloc, policy)`
  - `policy()`; `young()`/`young_mut()`; `mature() -> &ImmixSpace`; `stats() -> GcStats`;
    `last_immix_stats() -> Option<ImmixStats>` (the most recent major's in-place reclaim accounting)
  - `should_collect(need_words)`; `collect(&mut self, roots) -> Result<Collection>` (reuses
    `Collection`/`CollectionKind`; no new variant); `collect_minor`/`collect_major` (`-> Result<GcStats>`) to
    force a specific collection; `ensure_capacity(need_words, roots)` (same semantics as `Collector`);
    `grow_nursery(need_words)`/`young_contains(ptr)`/`young_space_count()` (the chain seam + observables,
    same semantics as `Collector`); `minor_gcs()`; `impl HeapObservable`
  - A fullsweep that cannot fit its promotion grows the region (`grow_major`); a `max_heap_words` cap the
    promotion cannot fit returns the typed `heap_exhausted` **before any copy**. Coverage:
    `src/immix_collector/grow_tests.rs`.

### `any_collector.rs` — the strategy-dispatching seam
- `pub enum AnyCollector { Copying(Collector), Immix(GenImmixCollector) }` — a **complete drop-in for the
  concrete `Collector`** for the runtime hosts: the whole call set a host makes is present, so a host
  switches mature strategy by building an `AnyCollector` instead of a `Collector`. The two mature types
  differ (`&HeapSpace` vs `&ImmixSpace`), so the mature generation is exposed as **geometry accessors**
  rather than a common `&` reference.
  - `new(policy) -> Result<AnyCollector>` — the `Collector::new` counterpart:
    `from_config(&GcConfig::new(policy))`, i.e. the **automatic rule** decides.
  - `from_config(&GcConfig) -> Result<AnyCollector>` — dispatches on the **resolved** `config.mature()`
    (`MatureKind::Copying` → `Collector`; `MatureKind::Immix` → `GenImmixCollector`).
  - `kind() -> MatureKind`; `mature_choice() -> MatureChoice`; `policy()`; `stats()`;
    `minor_gcs() -> u32`; `heap_info() -> HeapInfo` (`impl HeapObservable`).
  - `young() -> &HeapSpace`; `young_mut() -> &mut HeapSpace`; `should_collect(need_words) -> bool` — the
    young-nursery admission signal; `young()`/`should_collect` read the **current** space only (the sole
    allocatable one), so the answer is strategy-independent — while `young_contains(ptr)` covers **every**
    space of the chain.
  - `collect(&mut self, roots) -> Result<Collection>`; `collect_minor`/`collect_major` (`-> Result<GcStats>`);
    `ensure_capacity(need_words, roots) -> Result<Collection>`; `grow_nursery(need_words) -> Result<()>` (the
    chain's no-relocation growth seam); `young_contains(ptr) -> bool`; `young_space_count() -> usize`.
  - The **mature generation** accessors: `mature_capacity_words()`, `mature_used_words()`,
    `mature_available_words()` (mark-region figures are line-rounded — `used_lines * LINE_WORDS`, i.e. "at
    least this many words"), `mature_base() -> *mut Eterm`, `mature_contains(ptr: *const Eterm) -> bool`.
  - `mature_contains` **reuses each concrete space's own range test, so the two differ in exactly one way**:
    the copying space covers the **live region `[base, top)`**, the mark-region space the **whole region
    including free lines** (deliberately **over-inclusive**). Harmless for the membership/valve question it
    answers: a word the space never hands out cannot be the target of a live reference, and a genuinely
    foreign container lies outside every mature range entirely.
  - forwarding only — the seam owns no collection logic, so neither strategy depends on the other.

### `config.rs` + `selection.rs` — the mature-generation decision
- `pub struct GcConfig { pub policy: HeapPolicy, pub selection: MatureSelection }` — the heap-size policy
  plus **how** the mature strategy is chosen: `new`/`auto` (both = the automatic rule),
  `copying(policy)`/`immix(policy)` (explicit pins), `growable()` (= `auto(HeapPolicy::unbounded())`),
  `with_selection`/`with_mature`, `policy()`, `selection()`, `mature() -> MatureKind` (the resolved
  strategy), `mature_choice() -> MatureChoice`. `Default` = `GcConfig::new(HeapPolicy::default())` =
  `Auto`, which resolves to `Copying` for the default (233-word) policy.
- `pub enum MatureSelection { Auto, Explicit(MatureKind) }` (default `Auto`) — the *how*; `MatureKind` is
  the *strategy*. `Explicit` beats the rule in both directions.
- `pub enum MatureSource { Automatic, Explicit }`; `pub struct MatureChoice { kind, source, mature_words,
  break_even_words }` with `rule_kind()`/`is_override()`/`describe()` — the **observable** resolved choice.
- `pub const IMMIX_BREAK_EVEN_WORDS: usize = 8 * BLOCK_WORDS` (= 2048) and `IMMIX_BREAK_EVEN_BLOCKS` (= 8);
  `select_mature(mature_words) -> MatureKind` (Immix iff `>=` the break-even — the pure rule);
  `mature_words_of(policy)`; `resolve_mature(&GcConfig) -> MatureChoice`; `choice_for(kind, mature_words)
  -> MatureChoice`.

### `policy.rs` — heap-size policy (the `erts_next_heap_size` analogue)
- `pub struct HeapPolicy` — `min_heap_words`, `max_heap_words` (0 = unbounded), `fullsweep_after`
  (default 65535), `min_bin_vheap_words` (default 46422 = `next_heap_size(32768, 0)`, the C
  `BIN_VH_MIN_SIZE`), `initial_young_words` (233), `initial_mature_words`, `need_reserve_words`;
  `default()`, `from_min_max(min, max)`, `unbounded()` (explicit `max_heap_words: 0`), builder.
  - `from_spawn_opts(min_heap_words, min_bin_vheap_words, fullsweep_after)` — the C `spawn_opt` rule
    (`erl_process.c:12321-12349`): each minimum is floored and rounded through `erts_next_heap_size(x, 0)`,
    so a stored minimum is a growth-table fixed point.
  - `fullsweep_due(minor_gcs) -> bool` — `minor_gcs >= fullsweep_after` (the C `GEN_GCS >= MAX_GEN_GCS`);
    `fullsweep_after == 0` forces a fullsweep on every collection.
  - `next_words(size, offset)`; `size_after_fullsweep(live, need)`; `mature_after_fullsweep(capacity, live,
    need)` (grow / shrink / keep — the C `adjust_after_fullsweep`, `erl_gc.c:2061-2098`);
    `young_after_collection(capacity, live)` (the **bounded** nursery rule; the C minor-time young resize,
    `erl_gc.c:1493`); `should_grow(...)`, `should_shrink(...)`.
- `pub const DEFAULT_MIN_HEAP_WORDS: usize = 233` — the C `H_DEFAULT_SIZE`.
- `pub fn heap_sizes() -> &'static [usize]` — the curated growth table (Fibonacci+1 to index 23, then
  +20%, capped at `2^53 / word`), `LazyLock`-built from `erts_init_gc` (`erl_gc.c:240-303`).
- `pub fn next_heap_size(size, offset) -> usize` — `erts_next_heap_size` over that table.

### `heap_info.rs` — the `garbage_collection` observables
- `pub struct HeapInfo { minor_gcs, fullsweep_after, min_heap_size, min_bin_vheap_size, max_heap_size,
  heap_size, total_heap_size }` — the `erlang:process_info(Pid, garbage_collection)` item
  (`ERTS_PI_IX_GARBAGE_COLLECTION`, `erl_bif_info.c:2073`): `new(...)`, `values() -> [usize; 5]`,
  `items() -> [(&'static str, usize); 5]`, `of<C: HeapObservable + ?Sized>(collector: &C) -> HeapInfo`.
- `pub trait HeapObservable` — `minor_gcs()`/`heap_size_words()`/`total_heap_size_words()`/`heap_policy()`;
  implemented by `Collector`, `GenImmixCollector` and `AnyCollector` (`AnyCollector::heap_info()` too).
- `pub const GARBAGE_COLLECTION_KEYS: [&str; 5]` — the oracle's key order (`max_heap_size,
  min_bin_vheap_size, min_heap_size, fullsweep_after, minor_gcs`); defaults `min_heap_size 233`,
  `min_bin_vheap_size 46422`, `fullsweep_after 65535`, `max_heap_size 0`.
- Word accounting: `heap_size` = the nursery capacity (C `HEAP_SIZE`); `total_heap_size` = nursery + mature
  capacity (C `heap_sz + old heap`); the message buffer is `beam_message`'s (not counted here).

### `mso.rs` — off-heap refcounted large objects & refc binaries
- `RefCount` — **re-exported** from `beam_alloc` (`pub use beam_alloc::RefCount;`); the workspace's single
  `erts_refc_*` primitive (`new`, `count`, `inc`, `dec`, `dec_and_test(min)`). This crate defines no count
  of its own; both `beam_gc::RefCount` and `beam_gc::mso::RefCount` resolve.
- `pub struct Mso` — an off-heap, immutable, refcounted shared object: `new(payload, apparent_size)`,
  `downcast_ref::<T>()`, `apparent_size()`, `refcount()`.
- `pub struct MsoHandle` — clonable owning handle to an `Mso` (the on-heap `BinRef` analogue):
  `Clone` bumps the refcount; the last `Drop` runs the destructor (the payload's `Drop`) and frees it.
- `pub enum OffHeapEntry { Mso(MsoHandle), Binary(beam_alloc::RefcBinary) }` — an entry of the per-process
  off-heap list: `overhead_bytes()`/`apparent_size()` (Mso → the MSO's apparent size; Binary → `len()`),
  `as_mso() -> Option<&MsoHandle>`, `as_binary() -> Option<&RefcBinary>`; `From<MsoHandle>`/`From<RefcBinary>`.
- `pub struct OffHeap` — the per-process list (`MSO(p)`), **slot-stable for the list's whole lifetime**:
  storage keeps a released slot in place (as a released/`None` slot), so `insert`'s returned slot index is
  valid for the list's entire lifetime — `sweep` never shifts, reorders, compacts or reuses a slot, and a
  later `insert` always appends at a fresh index.
  - `push(impl Into<OffHeapEntry>)` — a slot-discarding append.
  - `insert(impl Into<OffHeapEntry>) -> usize` — append returning the slot — the index an on-heap off-heap
    reference stores in its `val` word.
  - `get(slot) -> Option<&OffHeapEntry>` — the entry at `slot`; `None` for a **released** slot as well as
    for an out-of-range slot.
  - `iter() -> impl Iterator<Item = &OffHeapEntry>` — the **live** entries in slot order (released skipped).
  - `len()`/`is_empty()`/`overhead_bytes()` — **live** entries only (`overhead_bytes` is the summed off-heap
    payload bytes, not the `Vec` capacity).
  - `sweep<F: FnMut(&OffHeapEntry) -> bool>(is_live) -> usize` — releases the dead entries **in place**
    (dropping each exactly once: a `RefcBinary` frees its payload at zero refcount, an `Mso` runs its
    destructor) and returns how many were released. The caller supplies the liveness predicate.
  - **Why slot stability is required:** an on-heap off-heap reference stores the slot index in its `val`
    word (`beam_message::layout::make_off_heap_container` / `off_heap_slot`), so a compacting sweep would
    silently invalidate every such reference — which is exactly why `beam_message::OffHeapTable` could not
    call it. The deliberate trade-off: released slots are retained, so memory is bounded by total inserts,
    not by live objects.

### `stats.rs` / `error.rs`
`GcStats` — `words_copied`, `words_reclaimed`, `objects_copied`, `minor_collections`, `major_collections`
(`Clone + Copy + Default + PartialEq + Eq + Debug`). `error.rs` holds `pub(crate)` allocation-failure
constructors only (no new error enum).

## Design Decisions
- **Two generations: `young` nursery + `mature`; minor tenures young→mature (immediate promotion).**
  A **minor** Cheney-copies every live young term straight into `mature` and resets the nursery; because
  terms are immutable a mature object can never acquire a pointer to a later-allocated young object, so the
  *no-mature→young-pointer* invariant holds after every minor and mature objects are **never rescanned** —
  the generational payoff. A **major** (fullsweep) evacuates `young` **and** `mature` into one freshly-sized
  space (the new `mature`), resetting the nursery — the C `major_collection`/`full_sweep_heaps`
  (`erl_gc.c:1854,1962`). The C's finer `high_water` single-block splitting and per-object tenuring are
  deliberately replaced by this simpler but equally-correct scheme (Known Gaps).
- **The destination is sized up front — no partial-copy rollback.** A minor runs only when
  `mature.available_words() >= young.used_words()` and the policy allows it; otherwise `collect` escalates
  to a major, which sizes a fresh space up front (`mature_after_fullsweep(capacity, live, 0)` over the
  **exact reachable live count**) *before* copying — the C's `heap_sizes`/`adjust_after_fullsweep`
  estimate. A collection therefore never leaves the from-space half-forwarded.
- **The fresh mature space is sized from the EXACT reachable live set, never the pre-collection
  occupancy.** Occupancy is **garbage-inflated** (it includes everything the sweep is about to reclaim), so
  sizing from `young.used_words() + mature.used_words()` fires the policy's grow test (`should_grow`,
  `3*capacity < 4*need`) on *every* fullsweep and the mature capacity climbs without bound; sizing from
  `live.rs`'s exact reachable count converges the same loop to a constant, matching the C reference
  (`erl_gc.c:2072`). Measured sequences: `MEASURED.md`. Evidence: `tests/steady_state.rs`,
  `tests/mature_capacity_convergence.rs`.
- **`GenImmixCollector` = the same copying nursery + a mark-region mature space; its major reclaims the
  mature generation *first*.** A **minor** promotes the nursery's live terms into the `ImmixSpace` through
  the shared `copy.rs` forwarding protocol over a private `PromoteDest` (`EvacDest` classifying against the
  nursery's `FromSet`, reserving from `ImmixSpace::alloc_words`) — the barrier-free invariant holds because
  promotion only ever moves young objects *into* mature. A **major** is the prescribed
  `ImmixSpace::collect`-then-promote order, made sound for the young→mature edge an in-place reclaim cannot
  ignore: a read-only `scan_young` first reports the promotion's exact **line** count and every nursery
  reference word pointing into mature (`(slot, term)` pairs); `ImmixSpace::collect` takes those terms as its
  `promoted` roots (so a mature object reachable only through the live nursery survives), each recorded
  nursery slot is then rewritten to the object's final address (the aligned `promoted` entry `collect`
  updated), and only then does promotion run — by which point the nursery's mature references are `Foreign`
  and final, so no stale pointer can result. If `free_lines() < needed` after the reclaim the collector
  **grows the region by relocation** (`grow_major`); a typed `heap_exhausted` is returned only when a
  `max_heap_words` cap forbids the growth, and then before any copy. `words_reclaimed` = mature reclaim +
  nursery garbage. Why reclaim-first rather than promote-first: it is what lets a would-not-fit minor
  escalate to a major that *makes room*.
- **`GenImmixCollector` ignores the `MatureKind` in `with_config`.** The type *is* the Immix-backed
  collector, so `with_config` always builds one over `cfg.policy()`; the strategy is decided by
  `AnyCollector::from_config` (on `config.mature()`) / `GcConfig::mature_choice` — the explicit pin or the
  automatic rule. `Collector` and every existing public signature are unchanged.
- **`AnyCollector` only forwards.** The dispatch seam carries no collection logic, so neither strategy
  depends on the other; `mature_choice()` derives the decision from the live collector (`selection.rs`, safe
  Rust depending only on `config`/`policy`/`immix`). Its geometry accessors exist because the two mature
  spaces are differently typed — they reproduce, not reinterpret, what each concrete space answers.
- **Heap sizing is ERTS-shaped and applied at every collection.** `HeapPolicy` carries the C minimums and
  the `fullsweep_after` cadence; `from_spawn_opts` reproduces the C `spawn_opt` rounding through
  `erts_next_heap_size(x, 0)`. `Collector::collect` picks the collection with `fullsweep_due`;
  `mature_after_fullsweep` sizes the fresh mature space from `need` = the **exact reachable live word
  count** and still re-uses `should_grow` (`3*cap < 4*need`) / `should_shrink` (`cap > 4*need && cap >
  min`), floored by `min_heap_words` and clamped by `max_heap_words`.
  - **The nursery re-size (`young_after_collection`) is a BOUNDED rule, not a growth rule.** `live` is the
    words the round **left in the nursery** (a minor passes what it promoted, a fullsweep `0`). With
    `request = live.max(need_reserve_words)`, the *only* grow branch is `request > capacity_words` (a
    surviving set larger than the nursery), which steps up to offset-0 `next_words(request, 0)`; otherwise
    `should_shrink(capacity, request)` falls it back to `next_words(request, 0)`, and otherwise the capacity
    is kept. A nursery the round left mostly empty therefore **shrinks** and one that turned out about right
    **keeps** its size — the rule the pressure tests depend on (a test that must keep a large nursery alive
    raises `min_heap_words`). The capacity is therefore **bounded**: with no growth seam in play a fixed
    live set converges to a constant, and the nursery never grows merely because a collection ran.
  - **Why the surviving set, at offset 0.** Sizing from the pre-`reset()` fill (≈ capacity) or stepping
    offset-1 `next_words(capacity.max(request), 1)` both fire on *every* collection, growing the nursery ~20 %
    each round with no bound, so RSS scales with the workload size (`MEASURED.md` records the sequence).
    Under the bounded rule the nursery is *flat* — a 200-round growing load holds it at its 233-word default
    while the total heap plateaus at 609 (Copying) / 1257 (Immix) words (`tests/nursery_bounded.rs`).
- **On-demand growth is an explicit seam, not a collect side effect.** Growth normally happens *only* inside
  a collection; `ensure_capacity(need_words, roots)` (on `Collector`/`GenImmixCollector`/`AnyCollector`)
  adds the caller-driven path a runtime needs when the mutator demands room: it collects once to reclaim,
  then grows the now-empty nursery to the smallest curated size covering `need_words` (`Nursery::grow`, one
  bounded step). It cannot "collect until it fits" because `young_after_collection` re-sizes the next
  nursery from the (now empty) *surviving* set — a further collect would shrink it, never grow it. A
  `max_heap_words` cap that cannot reach `need_words` yields a typed `heap_exhausted` before any copy.
  `HeapPolicy::unbounded()`/`GcConfig::growable()` express "no cap" (see Handoffs).
- **The young nursery is a CHAIN of spaces (`src/nursery.rs`), and `grow_nursery` is a NO-RELOCATION seam.**
  `Collector`/`GenImmixCollector` own a `Nursery` — a **current** space plus the spaces a growth
  **retired** — rather than a single `HeapSpace`. `HeapSpace` never reallocates, so growing the nursery by
  *replacement* would strand every live word; `Nursery::grow` instead **retains** the old space and points
  the allocator at a fresh, larger one (`target = next_words(need_words, 0).max(current_capacity)`, one
  bounded step), so every live word keeps its **original address** and nothing is moved. That is exactly what
  makes the seam sound for a caller holding **no root set** — the un-rooted `beam_rt` allocation path, which
  must not collect (a copying collection may only run against a complete root set). That path **consumes** the
  seam: `beam_rt`'s `alloc_words` grows the nursery (`Process::grow_nursery`) and retries the bump before its
  arena fallback, and its admission-point root valve (`root_is_unscanned`, through `young_contains`) accepts a
  root into **any** nursery space, so a retirement never makes a live root look unscanned. `young()`/`young_mut()`
  keep returning the **current** (allocatable) space, so every host site keeps compiling unchanged;
  `young_contains` and the collection from-set cover **every** space (`Nursery::spaces`); and after any
  collection the chain is **collapsed to exactly one space** (`Nursery::collapse`), because the collection
  already evacuated them all. `HeapObservable::heap_size_words` reports the **summed** nursery while a chain
  is live; `should_collect` reads the current space only (the sole allocatable one). A `max_heap_words`/
  growth-table ceiling that cannot reach the request returns the typed `heap_exhausted` and retires nothing.

- **`xptr_tag` is not needed.** The C ORs the literal bit back in only for literal-area copies
  (`copy.c:2109`); a GC collection copies only in-heap objects, so a copied pointer never carries the literal
  bit. A literal-tagged (and every other out-of-from-space) reference fails the `FromSet.contains` test and
  is left verbatim — the C `erts_is_literal`/area-test rule (`erl_gc.c:2013,2292-2297`).
- **In-place Cheney forwarding, exactly as C.** A boxed object's **header word is overwritten** with the new
  BOXED-tagged pointer (`IS_MOVED_BOXED = !is_header`, `erl_gc.h:36`); a cons cell's car is set to
  `THE_NON_VALUE` and its cdr to the new LIST pointer (`IS_MOVED_CONS`, `erl_gc.h:37,42-55`). The scan phase
  reproduces the C `sweep()` (`erl_gc.c:2328-2348`) over the canonical per-object walk: a `BOXED`/`LIST` word
  is a *reference* to forward and is stepped over one word (a cons cell's two words directly), while a
  `HEADER` word sizes its object with `move_size`, forwards every copy child slot
  `beam_term::layout::for_each_child_slot` reports, and jumps past the whole object — so `copy.rs` holds no
  per-shape stepping logic.
- **Funs are sized and scanned from the inlined-descriptor layout in `beam_term::fun`, *not*
  `header_arity`/`thing_arityval`.** `header_arity` (hence `thing_arityval`) deliberately returns the C
  `ERL_FUN_SIZE - 1 == 1` for a fun — the assumption that a fun block is `[header][entry-pointer][env…]`.
  This runtime instead **inlines** the C `entry` as descriptor words, so a fun block is
  `[header][desc_words(kind)][env[0..num_free]]`, with `desc_words` = 2 (`Local`), 3 (`External`:
  `module`·`function`·`arity`), 10 (`Imported`) and `env[i]` at `hp[1 + desc_words(kind) + i]`. The copier
  sizes it as `fun_words(kind, num_free)` == `1 + fun_desc_words(kind) + fun_num_free` and the canonical walk
  reports exactly the `num_free` env slots, so the descriptor is skipped opaquely; every offset/width comes
  from `beam_term::fun`. `header_arity`/`thing_arityval` would **under-size** the copy by
  `desc_words(kind) - 1` words (1/2/9) and land the scan on a raw descriptor word (`index`, the `arity` word,
  `uniq` bytes), following it as a bogus term reference and corrupting the heap.
- **Object sizing/walking is shared, not duplicated.** The per-object walk lives in exactly one place —
  `beam_term::layout::{object_words, for_each_child_slot}`. `copy.rs` exposes only the copy-specific
  `pub(crate)` pieces, and both the copying `Copier` and the mark-region `immix/` phases consume them, so no
  crate re-derives a shape's size or its child slots; `space.rs`, `collector.rs`, `policy.rs`, `stats.rs`,
  `config.rs` and `error.rs` never inspect a term's interior.
- **The copier sizes and walks through `beam_term::layout`, with one copy-size override.**
  `copy.rs::move_size(header, ptr)` is `beam_term::layout::object_words` for every shape **except** an
  `ErlSubBits`, for which it returns `ERL_SUB_BITS_SIZE` (`5`) rather than the *sizing* graph's
  `ERL_REFC_BITS_SIZE` (`8`) — those are the words a copier lays down, after which forwarding `orig` copies
  the separate 3-word `BinRef`. It does **not** call `beam_term::size::size_object` (a whole-graph size that
  cannot size the single object the copier is looking at), and it is **not** the message copier's layout
  (`beam_message` has its own private walk). The override is the only shape-specific decision left in
  `copy.rs`; `tests/refc_binary_copy.rs` pins it (the pair copies as two objects totalling exactly 8 words).
- **Root set = a flat `&mut [Eterm]`.** The C scans the whole stack *word by word* with no frame metadata (a
  word is a term iff its primary tag is BOXED/LIST and it points into a swept area — `erl_gc.c:1677-1717`),
  so a flat slice of root words (live x registers + live stack cells) is **semantically faithful**, not a
  shortcut. Frame-aware stack relocation (`copy_erlang_stack`) and the process dictionary / message-queue /
  saved-argument sub-rootsets belong to `beam_process`.
- **`HeapSpace` wraps `beam_alloc::Heap`; it is not a new allocator.** `beam_alloc::Heap` is the production
  bump heap (its `AllocKind::Heap` exhaustion means "collect"). `HeapSpace` adds only space semantics
  (`contains`, `reset`, `base`/`top`/`limit`), so collector and mutator share one allocation implementation.
- **The off-heap list is `Mso`/`MsoHandle` + `beam_alloc::RefcBinary` + `OffHeap`, not bolted onto the
  heap.** Both payload kinds live **outside** every process heap; the collector **copies only the small
  heap-resident handle's scalar words** and leaves every off-heap pointer verbatim (the C `sweep_off_heap`
  rule, `erl_gc.c:2995-3110`). Refcount changes are the **off-heap API's** responsibility, so the collector
  never walks off-heap payloads. The count is `beam_alloc::RefCount` — the workspace's single `erts_refc_*`
  primitive, **re-exported** here (no duplicate). The on-heap `BinRef`/`ErlSubBits` *handle term* itself is
  `beam_term`'s job; this crate owns the refcounted off-heap objects, the shared count, and the list.
- **`OffHeap` owns the slot↔object index; a copy boundary only reads it.** An on-heap off-heap reference
  stores a *slot number* into the owning list, so that index association is a property of the list (the LCA
  that owns the MSO model): `insert -> slot`, `get(slot)`, `iter()`, `push` (the slot-discarding append).
  The slot is **stable for the list's lifetime** because `sweep` releases dead entries **in place**, so the
  `val` word a collection wrote stays resolvable across any number of sweeps (`mso.rs`, API Surface).

### Measured (summary — every figure, table and caveat is in `MEASURED.md`)
- **The multi-workload gate (`tests/immix_measure.rs`, `tests/immix_workloads.rs`, `tests/immix_selection_curve.rs`).**
  GenImmix wins **copy work on long-lived data**, but every win is paired with a loss: **footprint** 3.6×–8×
  (line-granular internal fragmentation), a longer reclaim **pause**, and an outright loss on the common
  small-heap/high-churn shape. Both strategies find the **same** live set and reclaim identical garbage;
  Immix's moved words are **distribution- and occupancy-invariant** while its footprint is a fixed geometry
  multiple of the live set. The constant below is therefore a **geometry-driven copy-work** break-even.
- **The automatic rule (`tests/immix_break_even.rs`, `tests/mature_selection.rs`).** A single-variable sweep
  of a purely long-lived corpus (0.25 → 64 whole Immix blocks, three shapes, forced fullsweep) yields the rule
  `select_mature` implements — switch at **8 whole blocks = `IMMIX_BREAK_EVEN_WORDS` (2048 words)**:
  **(1) below one whole block Immix is strictly dominated** (no full block to pin, so it moves *more* words
  than a copy and its footprint is larger); **(2) from one whole block up it pins the whole live set and moves
  0 words**, with an object-size-independent geometry multiple as footprint; **(3) 2048 is the smallest
  power-of-two block count in that flat regime**. The rule reads the **construction-time**
  `initial_mature_words` once (`mature_words_of(policy)`), so it leaves the default 233-word mature generation
  (under one block) on the copying path — `GcConfig::default()` resolves to the copying collector.
  `MatureSelection::Explicit(kind)` beats the rule both ways; `mature_choice()` reports the source.
- **Standing decision (`tests/allocation_heavy_workload.rs`).** On an allocation-heavy workload — a modest
  bounded long-lived set plus a large volume of short-lived allocations that die before the next collection —
  the mark-region fullsweep moves **35.7 % fewer words over the run**, exactly the long-lived set it pins
  instead of re-copying at each fullsweep, but costs a **3.79× larger peak mature footprint** and a
  **1.7–2.3× longer fullsweep pause** (a printed, never-asserted signal). Where Immix is selected it moves
  fewer words while pausing longer, so **`MatureKind::Copying` remains the default** and Immix stays a
  size-/latency-tolerant option auto-selected at ≥ `IMMIX_BREAK_EVEN_WORDS` configured mature words or by an
  explicit pin.
- **Runtime-shaped dispatch measured (`../beam_rt/tests/gc_immix_payload.rs`, `tests/gc_strategy.rs`).** The
  same workload driven through the runtime's own GC doors with a runtime-spawned process reproduces the
  small-object/high-churn verdict at the break-even (Copying at `initial_mature_words` 2047 vs Immix at
  2048): Immix moves 1.146× the words for 6.57× the mature occupancy, which is why the automatic rule's
  default stays Copying and `MatureKind::Immix` remains an opt-in (`BEAM_RT_GC_STRATEGY=immix` raises the
  spawned policy's mature generation past the break-even). Load-invariant counts only — the host is heavily
  loaded, so timings are not asserted.

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only. Heap exhaustion while copying a term is
  `BeamError::Alloc(AllocFailure::new(AllocKind::Heap, bytes))` (never panic, never a crate-local error
  enum, never `unwrap()` on runtime data). `beam_term::TermError` is reconciled to `BeamError` upstream,
  so `?` crosses the boundary.
- **`unsafe` in production code is confined to the word-level code** — `copy.rs` (the Cheney copier),
  `immix_collector.rs` (the promotion driver) and `immix/` (the mark-region space's line/mark/evacuation
  walks) — plus the two narrow `NonNull`-deref / `Box::from_raw` blocks in `mso.rs`; every `unsafe fn`
  carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`beam_common::safety`). `unsafe_op_in_unsafe_fn` stays clean. `space.rs`, `policy.rs`, `heap_info.rs`,
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
  `any_collector`, `immix_collector` (+ `src/immix_collector/grow_tests.rs`), `immix/tests.rs`, and `live`;
  `live`'s module proves `live`/`live_full` equal a real collection's `words_copied`/line need, including on
  the mature generation (a relocating major; a refc binary counted on-heap only).
  **`copy.rs` and `collector.rs` carry no in-module tests** — the copier is pinned by
  `tests/copy_correctness.rs`, the driver by `tests/generations.rs` / `tests/gc_pressure*.rs`.
- `tests/copy_correctness.rs` → structure preserved from the copied graph, roots relocated, exactly the live
  size retained, off-from-space refs left verbatim.
- `tests/generations.rs` → minors + a major: `CollectionKind`, survival, empty nursery, escalation.
- `tests/map_copy.rs` / `tests/refc_binary_copy.rs` → the map forms and the refc pair survive, sized exactly
  to a footprint measured **independently** of every size heuristic.
- `tests/fun_copy.rs` → all three fun kinds with exact `1 + desc_words + num_free` sizing, incl. the
  >255-arity External descriptor and a descriptor-opacity probe.
- `tests/subtag_copy.rs` → bignum, native-record instance, an `ErlSubBits` match context (pins the 5-word
  copy override), external port/ref and HAMT head node, each with its exact independent footprint.
- `immix/*` in-module tests + `tests/immix_{parity,partial_block,measure}.rs` + `tests/gen_immix_collector.rs`
  → geometry/allocation/mark/pin/evacuation/accounting and the hard major cases; the SAME corpus through both
  strategies behind one structural walker; a multi-block object; the measured gate.
- `tests/immix_break_even.rs` → a geometry `Law` asserted at every point, and the 8-block constant pinned
  (NOT one line below); pause printed only (`MEASURED.md`).
- `tests/immix_selection_curve.rs` → the two-axis curve with the "moved words are object-size- and
  occupancy-invariant" invariant asserted; pauses printed only.
- `tests/immix_workloads.rs` / `tests/allocation_heavy_workload.rs` → the two measurements behind
  `MEASURED.md`; deterministic counts asserted, pause printed only.
- `tests/mature_selection.rs` → the rule + the explicit override both ways + a threshold-crossing corpus
  running the same collection-kind sequence with `compare::eq`-equal roots.
- `tests/production_dispatch.rs` → the drop-in/dispatch seam: one process-shaped workload through both
  strategies in **lockstep** (identical kinds, structurally equal roots); `should_collect` + the mature
  accessors agree with the concrete spaces; the mark-region range test is the over-inclusive one; `default` →
  Copying, `>= IMMIX_BREAK_EVEN_WORDS` → Immix by the rule.
- `tests/heap_sizing.rs` → growth/shrink bounds, clamping, `from_spawn_opts`, the `fullsweep_after` counter
  (incl. 0), the relocating Immix fullsweep, `HeapInfo` order, a system-`erl` cross-check (SKIPS LOUDLY).
- `tests/gc_pressure.rs` / `_structures` / `_offheap` → pressure: many minors/majors (nursery empty + roots
  intact after *each* collection), HAMT promotion, shared/cyclic graphs, off-heap binaries freed once.
- `tests/heap_growth.rs` → the mature generation grows while the nursery stays bounded; a too-small cap
  returns typed with **nothing copied**; `ensure_capacity` grows or errors typed.
- `tests/nursery_bounded.rs` → a growing load plateaus the nursery + total heap; a garbage-heavy collection
  shrinks it; a larger request grows it via `ensure_capacity`.
- `tests/nursery_chain.rs` → the chained-nursery **no-relocation** growth seam: `grow_nursery` preserves
  every live term **at its original address** and runs no collection; a graph spanning the growth boundary
  (retired↔new references) still collects correctly under BOTH strategies; `young_contains` covers every
  nursery space (false for a foreign address) and the chain collapses to one space after a collection; a
  request the policy ceiling cannot reach returns the typed `heap_exhausted` and leaves the collector
  untouched; a grow-then-collect loop plateaus (nursery + chain + mature).
- `tests/memory_overhead.rs` → per-object overhead `< 1` word (measured 0) + an RSS/word-count plateau
  (skipped loudly off-Linux).
- `tests/stress_cross_generation.rs` → one mixed graph through 6 minors + 2 majors under both strategies:
  structural equality after *each* collection, sharing, cycle closure, dropped garbage.
- `tests/collector_adversarial.rs` → break-attempts through both strategies + the primitive path (deep
  nesting proves the copier **iterative**; an exact-capacity boundary; off-heap frees exactly once).
- `tests/cyclic_safety.rs` → `size_object_bounded` returns the typed `TermGraphCyclic` (never a hang), and
  the collector copies each cyclic object once.
- `tests/fill_barrier.rs` → a too-small destination returns typed **before any copy** (counters
  byte-identical, identical on retry); `ensure_capacity` fails fast under a cap; no half-forward observable.
- `tests/retained_vs_live.rs` → copying hits `mature_used == live` (0 surplus), Immix ≤4× live (1.778×);
  constant capacities; RSS plateau ≤1.5×.
- `tests/immix_invariants.rs` → mutation-verified: the `run_major` ordering, line-bitmap consistency, no
  double-free, idempotent repeated collections.
- `tests/immix_stress.rs` → multi-block promotion, fragmentation/reuse, repeated-minor promotion, a mixed
  workload equal under both strategies, an asserted live-set-bounded accounting.
- `tests/steady_state.rs` → the DEFAULT-cadence steady state: nursery + mature capacity converge to a
  constant (233 Copying / 512 Immix).
- `tests/offheap_liveness.rs` → an off-heap handle inside a moved container: refcount stable, payload
  byte-identical, handle relocated, freed once when the container dies.
- `tests/offheap_slot_stability.rs` → a simulated on-heap `val`-slot container still resolves after a
  **middle** sweep; released slots answer `None` and are never reused; a released `RefcBinary` payload is
  freed exactly once.
- `tests/offheap_steady_state.rs` → the `live`/`live_full` contract from outside the crate: exact agreement
  with a real `collect_major` in place and on relocation; `.lines` load-bearing (`used_lines == 20` where
  `lines_for(104) == 7`); off-heap-heavy graphs counted on-heap only, an `Mso` payload never as heap; a
  240-round bounded steady state at the default cadence (constant nursery/mature capacity + resident lines).
- `tests/mature_capacity_convergence.rs` → the fullsweep mature capacity converges (no unbounded growth).
- `tests/immix_major_growth.rs` → a growable heap grows the region instead of erroring; a `max_heap_words`
  cap yields the typed `heap_exhausted` before any copy.
- **Per-shape copy coverage.** Sizing/walk is `../beam_term`'s `layout::{object_words, for_each_child_slot}`;
  direct evidence exists for every shape (cons/tuples, flat + HAMT maps, the refc pair, all three fun kinds,
  bignums, a native-record instance, an `ErlSubBits` match context, external ports/refs, a HAMT head node,
  and a mixed cross-generation corpus). `memory_overhead.rs` cross-checks `object_words` against the bump
  allocator's real consumption.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Cross-checks against the real system ERTS belong to `../tools/differential/`, not here.

## Known Gaps (intentionally NOT covered in Milestone-1)
- **The mark-region strategy is reachable in production; the DEFAULT remains the copying `Collector`.** The
  dispatch IS on the production path: `beam_process::Process` owns an `AnyCollector` built from the spawn
  policy (`AnyCollector::new(policy)`), exposes it as `collector() -> &AnyCollector`, and `beam_rt` drives it
  at its heap-admission point (`src/context/gc.rs::{ensure_heap, ensure_heap_full}` → `Process::collect` /
  `ensure_capacity`), reading the mature generation through the geometry accessors (`mature_contains` in the
  unscanned-root valve, `mature_used_words` in the memory hook). `selection::select_mature` chooses `Immix`
  only at/above `IMMIX_BREAK_EVEN_WORDS` (8 whole blocks = 2048 words of configured mature generation),
  which the default 233-word policy never reaches — so `GcConfig::default()` / `AnyCollector::new(default
  policy)` build the copying `Collector` and the default path keeps its Copying behaviour (the corpus gate
  reports 0 mismatches). Immix is reached by a policy that configures a large mature generation (the
  automatic rule — `beam_rt`'s default-OFF `BEAM_RT_GC_STRATEGY=immix` opt-in does exactly that) or by an
  explicit `GcConfig::immix` / `MatureSelection::Explicit` pin.
- **The Immix region never reallocates in place; the collector grows it by relocation.** `ImmixSpace` wraps
  a `beam_alloc::Heap` and is never resized in place. Instead a major replaces the whole region: the live set
  is relocated into a fresh `ImmixSpace` sized by the policy — the `mature_after_fullsweep` target
  (`relocate_major`), or a larger region when an in-place fullsweep's reclaim cannot fit the nursery's
  promotion (`grow_major`). A growable heap therefore never fails a collectable collection; only a
  `max_heap_words` ceiling the promotion cannot fit returns the typed `heap_exhausted` (before any copy).
- **Promotion atomicity is guaranteed only on the *total* line budget.** A minor promotes only when
  `mature.free_lines() >= needed` (the exact line count of the live nursery graph), and a major checks the
  same after reclaiming. Line-granular allocation means an object needs a *contiguous* run of its lines, so
  a pathologically fragmented mature space could still fail a per-object reservation mid-promotion.
- **The C `high_water` single-block layout, per-object tenuring (`GEN_GCS`/`MAX_GEN_GCS`) and the
  `F_NEED_FULLSWEEP` flag protocol** — replaced by the simpler two-space young/mature scheme above.
- **Dirty-scheduler escape for long collections** (`ERTS_POTENTIALLY_LONG_GC_HSIZE`, `erl_gc.h:26`).
- **Frame-aware stack relocation** (`copy_erlang_stack`), the process dictionary, on-heap message/signal
  queues and saved BIF/NIF arguments as sub-rootsets — owned by `beam_process`; this crate takes a flat
  root slice.
- **Literal-area collection** (`erts_garbage_collect_literals`, literal-area purge/`xptr_tag`) — literals
  are treated as permanent and left verbatim.
- **A.6 runtime heap options** (`+hms`, `+hmbs`, …) — this crate exposes `HeapPolicy`, not the CLI
  surface (owned by `beam_rt`).
- **`OffHeap` is on the production path as both the MSO slot store and the process's binary store; its
  liveness predicate is caller-driven and deliberately conservative.** `OffHeap` holds both MSO and
  refc-binary entries and IS the production MSO slot store inside `beam_message::OffHeapTable` (which stores
  the slot index an on-heap container names). `beam_process::Process` additionally owns a dedicated `OffHeap`
  (`owned_binaries`) for the `RefcBinary` payloads it takes ownership of (`Process::register_off_heap_binary`)
  and drives `OffHeap::sweep` as the last step of every rooted collection
  (`Process::sweep_off_heap_binaries`), with a predicate that releases only entries the caller declared dead
  and retains anything undecided (leak-not-dangle).
  **The remaining gap is the marking half, and it is this crate's:** the walk deciding whether an on-heap
  `BinRef`/`ErlSubBits` (possibly inside another off-heap payload) still names a payload lives in
  `live.rs`/`copy.rs` and is `pub(crate)`, so a public, scan-driven liveness enumeration over a heap would
  let a caller release undecided entries instead of retaining them;
  `Process::retain_off_heap_binaries(is_live)` is the seam such a host drives.

## Handoffs (owned by other nodes)
- **The strategy switch is adopted; the residual is an explicit pin on the process-construction path.**
  `beam_process::Process` holds an `AnyCollector` and exposes `collector() -> &AnyCollector`; its
  `run_rooted_collection` bound is `FnOnce(&mut AnyCollector, &mut [Eterm])`; `beam_rt` uses
  `AnyCollector::mature_contains` (the unscanned-root valve) and `AnyCollector::mature_used_words` (the
  memory hook). `ProcessTable::spawn` takes only a `HeapPolicy`, so the strategy is always resolved by the
  automatic rule (`MatureSource::Automatic`): `beam_rt`'s default-OFF `BEAM_RT_GC_STRATEGY=immix` opt-in
  reaches Immix by raising the policy's mature-generation size past the break-even, never as an override. An
  explicit `GcConfig`/`MatureSelection` pin would need a parameter on the process-construction path (owner:
  `beam_process`).
- `beam_message`: `OffHeapTable` may legitimately drive `beam_gc::OffHeap::sweep` — its module doc's
  "never sweeps because sweep compacts the slots" rationale no longer holds, since the list is slot-stable.
- **The growth seam is adopted by the runtime.** `beam_rt/src/context/gc.rs`'s shared `collect_or_grow`
  (behind both admission doors, `ensure_heap`/`ensure_heap_full`) grows the running process's nursery through
  `Process::ensure_capacity` when the policy admits the request and otherwise collects, and `Process::deliver`
  retries a refused copy the same way; a bounded `+hmax N` policy the request cannot reach still refuses, and
  that exhaustion raises `error:system_limit`. `HeapPolicy::unbounded()`/`GcConfig::growable()` express
  "no cap".
- **Test-id / MSO / `compare` caveats (cross-crate).** A pid is a heap term via
  `beam_term::thing::write_external_pid` (4 words) **and** `beam_term::compare::eq` orders it
  **structurally through its parts** (field order per the C `erts_cmp`), with `external_pid_parts`
  available to read the parts directly; `compare::eq` still **recurses forever on a cycle**. An MSO
  has no on-heap reference *term*: the safe reference is the `OffHeap::insert -> slot` index, never a
  raw pointer. Reach a refc binary's `BinRef` via the `ErlSubBits` `orig` slot, not pointer adjacency.
  Covered by `tests/stress_cross_generation.rs`.
- **`HeapInfo` is surfaced by the runtime.** `beam_rt` builds it directly (`HeapInfo::new` and
  `HeapObservable`) for `erlang:process_info(Pid, garbage_collection)` and `system_info(garbage_collection)`,
  in `GARBAGE_COLLECTION_KEYS` order (`beam_rt/src/native/memory/mod.rs`, `src/info/memory.rs`);
  `AnyCollector` is the production collector type, but `AnyCollector::heap_info()` itself is still called
  only from tests — the runtime derives `HeapInfo` from the `HeapObservable` items directly.
- **Repo-root owned** (`../../CHANGE.md`): owed entries for the heap-sizing policy (the C-faithful
  per-collection growth/shrink, the `min_bin_vheap_words`/`fullsweep_after` fields and `HeapInfo`) and the
  measured GenImmix decision.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/space.rs` → `HeapSpace` (copying semi-space over `beam_alloc::Heap`) + `FromSet`
- `src/nursery.rs` → `Nursery`: the young nursery as a **chain of spaces** — the **no-relocation** growth seam (`grow`), the collapse to one space after a collection (`collapse`), and the every-space range test / from-set (`spaces`/`contains`/`add_to_from_set`/`used_words`/`capacity_words`)
- `src/copy.rs` → the Cheney copy engine: `Copier`, `forward_roots`/`complete`, `collect`, and the copy-specific `pub(crate)` primitives; all per-object sizing/walking is delegated to `beam_term::layout`
- `src/collector.rs` → the two-generation `Collector`: minor/major driver over the nursery chain, bounded nursery collapse, mature sizing from the `live.rs` exact count, capacity pre-check, `ensure_capacity`/`grow_nursery`, `young_contains`/`young_space_count`
- `src/live.rs` → the **exact-live counter** (`live`/`live_full` + `Live { words, lines }`); its `#[cfg(test)]` module pins it against a real collection, incl. the mature generation, a relocating major and a **chained nursery**
- `src/config.rs` → GC-strategy seam: `GcConfig`, `MatureKind`, `MatureSelection`, `mature()`/`mature_choice()`, `growable`
- `src/selection.rs` → the **automatic rule**: `IMMIX_BREAK_EVEN_WORDS`/`IMMIX_BREAK_EVEN_BLOCKS`, `select_mature`, `mature_words_of`, `resolve_mature`/`choice_for`, `MatureSource`, `MatureChoice`
- `src/immix/mod.rs` → `ImmixSpace` geometry, block metadata, line-granular allocation, accounting views (`ImmixStats`/`ImmixCounters`)
- `src/immix/collect.rs` → `ImmixSpace` mark/classify/evacuate/finalize phases + its `EvacDest` impl
- `src/immix_collector.rs` → `GenImmixCollector`: `PromoteDest`, `promote`, `scan_young`, fullsweep sizing via `live_full`, `grow_major`/`relocate_major`, `ensure_capacity`
- `src/immix_collector/grow_tests.rs` → `GenImmixCollector`'s in-module growth-fallback tests (`grow_major`)
- `src/any_collector.rs` → `AnyCollector`: the strategy dispatch (`Copying`/`Immix`) over one API — the drop-in for the concrete `Collector` incl. the mature geometry accessors
- `src/policy.rs` → `HeapPolicy`, the curated `heap_sizes` table, `next_heap_size`, `from_spawn_opts`/`fullsweep_due`/`mature_after_fullsweep`/`young_after_collection`, `unbounded`
- `src/heap_info.rs` → the `garbage_collection` observables: `HeapInfo`, `HeapObservable`, `GARBAGE_COLLECTION_KEYS`
- `src/mso.rs` → re-exported `RefCount`, `Mso`, `MsoHandle`, `OffHeapEntry`, `OffHeap` — the **slot-stable** off-heap list over off-heap refcounted large objects **and** refc binaries
- `src/stats.rs` → `GcStats`; `src/error.rs` → `pub(crate)` GC allocation-failure constructors
- `tests/copy_correctness.rs` → the copier's word-level behaviour (structure preserved, roots updated, garbage dropped, off-from-space refs verbatim)
- `tests/generations.rs` → `Collector` minor/major/tenure/escalation
- `tests/gen_immix_collector.rs` → one corpus through both strategies behind the public API; `from_config(default)` → Copying
- `tests/production_dispatch.rs` → the drop-in/dispatch seam: one process-shaped workload through both strategies in **lockstep**, the accessors agreeing with the concrete spaces, `GcConfig::default()` → Copying and `>= IMMIX_BREAK_EVEN_WORDS` → Immix by the rule
- `tests/immix_parity.rs` → the full term corpus through both strategies behind one structural walker
- `tests/immix_partial_block.rs` → targeted per-object reclamation and `kept`-pinning
- `tests/immix_measure.rs` → the original measured gate (copy work vs the evacuated/pinned split; `MEASURED.md`)
- `tests/immix_break_even.rs` → the long-lived break-even sweep and its assert-backed `Law`; `MEASURED.md`
- `tests/mature_selection.rs` → the automatic rule (Copying below / Immix at-or-above the break-even) + the explicit override both ways
- `tests/map_copy.rs` → flat-map + HAMT-map (500-key trie incl. a hand-built collision node) copy correctness with independently measured footprint
- `tests/refc_binary_copy.rs` → the 8-word `[BinRef][ErlSubBits]` pair (pins the copy-size override)
- `tests/fun_copy.rs` → all three fun kinds: `1 + desc_words + num_free` sizing, the >255-arity External descriptor, descriptor opacity
- `tests/heap_sizing.rs` → the growth/shrink policy, `fullsweep_after`, `HeapInfo` derivation, the system-`erl` oracle cross-check
- `tests/gc_pressure.rs` / `gc_pressure_structures.rs` / `gc_pressure_offheap.rs` → threshold pressure, HAMT promotion, shared/cyclic graphs, off-heap freeing (both strategies)
- `tests/immix_workloads.rs` → the multi-workload copying-vs-Immix measurement (`MEASURED.md`)
- `tests/allocation_heavy_workload.rs` → the allocation-heavy Copying-vs-Immix measurement (`MEASURED.md`)
- `tests/heap_growth.rs` → on-demand growth, the mature generation's growth under a bounded nursery, the typed no-half-forward under a cap
- `tests/nursery_bounded.rs` → the bounded-nursery evidence (nursery + total heap plateau; shrink on a garbage-heavy round)
- `tests/nursery_chain.rs` → the `Nursery` chain: address-preserving `grow_nursery` (no collection), the growth-boundary-spanning graph, `young_contains`/`young_space_count` + the summed `heap_size_words`, the typed capped-growth error, and the collapse to one space
- `tests/memory_overhead.rs` → per-object overhead vs the frozen layout + the RSS/word-accounting no-leak plateau
- `tests/stress_cross_generation.rs` → the mixed-term cross-generation gate (both strategies)
- `tests/collector_adversarial.rs` → the adversarial copy-correctness / no-leak set (both strategies)
- `tests/cyclic_safety.rs` → `size_object_bounded`'s typed cyclic verdict + the collector's forwarding-pointer cycle safety
- `tests/fill_barrier.rs` → the young-heap-exhaustion / fill-barrier contract (typed error before any copy)
- `tests/retained_vs_live.rs` → GC-side retained-vs-live accounting (steady capacities, bounded surplus, RSS plateau)
- `tests/immix_selection_curve.rs` → the two-axis selection curve (object-size distribution × mature capacity; `MEASURED.md`)
- `tests/immix_invariants.rs` → the mark-region invariant probes (mutation-verified)
- `tests/immix_stress.rs` → the mark-region stress set (multi-block promotion, fragmentation/reuse, asserted bounded accounting)
- `tests/subtag_copy.rs` → per-subtag copy correctness with exact footprint accounting (bignum, record instance, `ErlSubBits` match context, external port/ref, HAMT head node)
- `tests/steady_state.rs` → the default-cadence steady-state gate (`MEASURED.md`; capacities converge to 233 Copying / 512 Immix)
- `tests/offheap_liveness.rs` → an off-heap handle held INSIDE a moved container: refcount stable, payload verbatim, freed exactly once
- `tests/offheap_slot_stability.rs` → slot stability across a releasing sweep (a `val`-slot container still resolves after a **middle** sweep; released slots answer `None`)
- `tests/offheap_steady_state.rs` → the `live`/`live_full` verification from outside the crate + 240-round bounded steady state (`MEASURED.md`)
- `tests/mature_capacity_convergence.rs` → the fullsweep mature capacity converges (no unbounded growth)
- `tests/immix_major_growth.rs` → the Immix growth fallback and the typed exhaustion before any copy
- `../../../erts/emulator/beam/erl_gc.c` → the collector being mirrored (`move_boxed`/`move_cons`/`sweep`/`do_minor`/`full_sweep_heaps`/`adjust_after_fullsweep`) (read-only)
- `../../../erts/emulator/beam/copy.c` → term traversal reference (`size_object_x`/`copy_struct_x`) (read-only)
- `../../erts/emulator/beam/erl_gc.h` → forwarding macros (`IS_MOVED_*`, `follow_moved`, `ErtsInYoungGen`) (read-only)

## See Also
- `MEASURED.md` → **all** measurement figures and caveats for the mature-generation strategies (the allocation-heavy workload, the long-lived break-even sweep, the two-axis selection curve, the multi-workload measurement, the heap-sizing measurements).
- `../CONTEXT.md` → crate-layer contracts, seam types and the wave-1 dependency position.
- `../../CONTEXT.md` → workspace contracts (§5 GC/memory model, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D ("Memory management in Rust") and §H (Stage 4).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → foundations this crate builds on (do not modify).
- `../beam_process/CONTEXT.md` → the primary consumer that drives collections at safepoints.
