# `beam_gc` — Per-Process Garbage Collector (Stage 4)

## Intent
Owns the **per-process collector** of the Rust ERTS rewrite: a **generational semi-space Cheney
copying** collector over the per-process heap, **barrier-free by construction** (Erlang terms are
immutable ⇒ *no term on the mature heap may refer to a term on the young heap*, so the mutator pays
zero per-store overhead), plus the off-heap **refcounted large-object (MSO)** interface and the
process **heap-size policy**. Implements the GC half of `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D and
the Stage-4 row of §H; reproduces the collector semantics of the legacy C (`../../erts/emulator/beam/erl_gc.c`
+ `copy.c`) **without** its C structure.
It is consumed by `beam_process`, which drives collections at safepoints, and depends downward only on
`beam_common`, `beam_term`, `beam_alloc`, `libc` (crate-layer wave 1).

Contracts honoured here (crate-layer `../CONTEXT.md`): the **root-set contract** (`x_reg_array[0..Live)`
+ the whole stack, **no frame liveness maps**), the **frozen term layout** (§2 — *never* assume a
different `Eterm` layout; build/inspect only through `beam_term`), and the **return-based error
strategy** (§3). MMTk and Boehm are **rejected**; GenImmix is a **measurement-gated** mature-generation
option — implemented (`immix/` + `immix_collector.rs`) and selectable through `GcConfig`/`AnyCollector`,
but never on the default path (see Design Decisions).

## API Surface (the seam dependents design against)
Re-exported at the crate root and via `beam_gc::prelude::*`.

### `space.rs` — copying semi-spaces
- `pub struct HeapSpace` — one copying semi-space: a thin wrapper over `beam_alloc::Heap` adding the
  *space* semantics (range tests + reset). Never reallocates (growth is by GC, not reallocation).
  - `new(capacity_words) -> Result<HeapSpace>`; `with_allocator(alloc: AllocRef, capacity_words) -> Result<HeapSpace>`
  - `from_heap(Heap) -> HeapSpace` / `into_heap(self) -> Heap`
  - `capacity_words()`, `used_words()`, `available_words()`, `base() -> *mut Eterm`, `top() -> *mut Eterm`
    (= bump pointer `hp`), `limit() -> *mut Eterm` (= `hend`), `reset()`
  - `contains(*const Eterm) -> bool` (in the live region `[base, top)`), `contains_term(Eterm) -> bool`
  - `heap() -> &Heap` / `heap_mut() -> &mut Heap` (term construction via the `beam_alloc::Heap` API)
- `pub struct FromSet` — the set of from-space ranges a collection evacuates (one range for a minor,
  two — nursery + mature — for a fullsweep): `new()`, `add(&HeapSpace)`, `contains(*const Eterm) -> bool`.

### `copy.rs` — the Cheney copy engine (the unsafe core)
- `pub struct Copier<'a>` — copies live terms from a `FromSet` into a `HeapSpace`, mirroring the C
  `move_boxed`/`move_cons`/`sweep` (`erl_gc.c:2280-2358`).
  - `new(from: &FromSet, to: &'a mut HeapSpace) -> Copier<'a>` (records the to-space scan origin)
  - `forward_roots(&mut self, roots: &mut [Eterm]) -> Result<()>` — forward each root word in place.
  - `complete(self) -> Result<GcStats>` — Cheney scan of the freshly-copied to-space to completion.
  - `run(self, roots: &mut [Eterm]) -> Result<GcStats>` — `forward_roots` + `complete`.
- `pub fn collect(from: &HeapSpace, to: &mut HeapSpace, roots: &mut [Eterm]) -> Result<GcStats>` — the
  primitive young-generation entry over a single from-space (objective §1).
- The shared `pub(crate)` evacuation primitives every other module builds on (so the word-level logic
  lives in exactly one place): `Target` (`Foreign`/`Live`/`Forwarded`), the `EvacDest` trait (a
  destination's `classify_boxed`/`classify_cons`/`can_reserve`/`reserve`), the generic
  `forward_slot<D: EvacDest>`, `move_size`, `ScanStep`/`scan_step`, and `for_each_ref` (`for_each_ref`
  walks from word 0 **including the header**, because the header selects the payload layout). `Copier`,
  `immix/`, and `immix_collector`'s `PromoteDest` all consume them.
- Fun blocks are sized and scanned from `beam_term::fun`'s inlined-descriptor layout
  (`1 + desc_words(kind) + num_free`, env at `fun_env_offset_header`), **never** via
  `header_arity`/`thing_arityval` — see Design Decisions.

### `collector.rs` — the two-generation collector
- `pub enum CollectionKind { Minor, Major }`
- `pub struct Collection { pub kind: CollectionKind, pub stats: GcStats }`
- `pub struct Collector` — owns `young` + `mature` `HeapSpace`s and the `HeapPolicy`.
  - `new(policy) -> Result<Collector>`; `with_allocator(alloc, policy) -> Result<Collector>`
  - `policy() -> &HeapPolicy`; `young() -> &HeapSpace`; `young_mut() -> &mut HeapSpace` (the mutator
    allocates here); `mature() -> &HeapSpace`; `stats() -> GcStats`
  - `should_collect(need_words) -> bool` — true when the nursery cannot satisfy `need_words` more
  - `collect(&mut self, roots: &mut [Eterm]) -> Result<Collection>` — picks minor vs major (policy),
    escalating a would-not-fit minor to a major
  - `collect_minor(&mut self, roots) -> Result<GcStats>`; `collect_major(&mut self, roots) -> Result<GcStats>`
  - `minor_gcs() -> u32` — minors since the last fullsweep (`GEN_GCS`); `impl HeapObservable` (see `heap_info.rs`)
  - After **every** collection the (now empty) nursery is re-sized from `HeapPolicy::young_after_collection`
    (the C minor-time young-heap resize, `erl_gc.c:1493`) and a fullsweep sizes the fresh mature space from
    `HeapPolicy::mature_after_fullsweep` (the C `adjust_after_fullsweep`) — a small nursery grows under
    allocation pressure and an oversized one shrinks toward its live set (see Design Decisions).

### `immix/mod.rs` + `immix/collect.rs` — the mark-region mature space (`ImmixSpace`)
- `pub struct ImmixSpace` — line/block mark-region mature generation. Geometry: `LINE_WORDS` 16,
  `LINES_PER_BLOCK` 16, `BLOCK_WORDS` 256. Line bitmaps per block (`allocated`/`marked`/`kept`),
  `BlockState::{Free,Partial,AllMarked}`.
  - `new(capacity_words)` / `with_allocator(alloc, capacity_words)`; `base`/`contains`/`contains_term`;
    `capacity_words`/`block_count`/`line_count`/`used_lines`/`free_lines`/`used_words`/`available_words`;
    `counters() -> ImmixCounters`; `reset()`
  - `unsafe alloc_words(words) -> Result<*mut Eterm>` (line-aligned); `cons`/`tuple`; `can_reserve(words)`
  - `mark(roots, promoted) -> Result<()>`; `collect(&mut self, roots: &mut [Eterm], promoted: &mut [Eterm])
    -> Result<ImmixStats>` — mark, classify, release, evacuate `Partial` blocks, pin `AllMarked`, finalize
  - `impl EvacDest for ImmixSpace` — so the shared `copy.rs` forwarding protocol can evacuate *into* it
- `pub struct ImmixStats` (`objects_evacuated`/`lines_evacuated`/`objects_pinned`/`lines_pinned`/
  `blocks_reclaimed`/`words_reclaimed`/`words_pinned`) and `pub struct ImmixCounters`.

### `immix_collector.rs` — `GenImmixCollector` (copying nursery + mark-region mature)
- `pub struct GenImmixCollector` — pairs the copying young `HeapSpace` with an `ImmixSpace` mature.
  - `with_config(&GcConfig) -> Result<GenImmixCollector>` (uses `cfg.policy()`; the `MatureKind` is
    ignored — this type *is* the Immix collector); `from_policy(policy)`; `with_allocator(alloc, policy)`
  - `policy() -> &HeapPolicy`; `young() -> &HeapSpace`; `young_mut() -> &mut HeapSpace`;
    `mature() -> &ImmixSpace`; `stats() -> GcStats`; `last_immix_stats() -> Option<ImmixStats>`
    (the most recent major's in-place reclaim accounting)
  - `should_collect(need_words) -> bool`; `collect(&mut self, roots: &mut [Eterm]) -> Result<Collection>`
    (reuses `Collection`/`CollectionKind`; no new variant) — plus `collect_minor`/`collect_major`
    (`-> Result<GcStats>`) to force a specific collection (a too-large `collect_minor` reports the typed
    heap exhaustion before copying anything)

### `any_collector.rs` — the strategy-dispatching seam
- `pub enum AnyCollector { Copying(Collector), Immix(GenImmixCollector) }`
  - `from_config(&GcConfig) -> Result<AnyCollector>` (`MatureKind::Copying` → `Collector`, the default;
    `MatureKind::Immix` → `GenImmixCollector`); `kind() -> MatureKind`; `policy()`; `young()`;
    `young_mut() -> &mut HeapSpace`; `stats() -> GcStats`; `collect(&mut self, roots) -> Result<Collection>`
  - thin forwarding only — lets a caller / test drive one corpus through either strategy.

### `policy.rs` — heap-size policy (the `erts_next_heap_size` analogue)
- `pub struct HeapPolicy` — `min_heap_words`, `max_heap_words` (0 = unbounded), `fullsweep_after`
  (default 65535), `min_bin_vheap_words` (default 46422 = `next_heap_size(32768, 0)`, the C
  `BIN_VH_MIN_SIZE`), `initial_young_words` (233), `initial_mature_words`, `need_reserve_words`;
  `default()`, `from_min_max(min, max)`, builder.
  - `from_spawn_opts(min_heap_words, min_bin_vheap_words, fullsweep_after)` — the C `spawn_opt` rule
    (`erl_process.c:12321-12349`): each requested minimum is floored and then rounded through
    `erts_next_heap_size(x, 0)`, so a stored minimum is a growth-table fixed point.
  - `fullsweep_due(minor_gcs) -> bool` — `minor_gcs >= fullsweep_after` (the C `GEN_GCS >= MAX_GEN_GCS`);
    `fullsweep_after == 0` forces a fullsweep on every collection.
  - `next_words(size, offset) -> usize`; `size_after_fullsweep(live_words, need_words) -> usize`;
    `mature_after_fullsweep(capacity, live, need) -> usize` (grow / shrink / keep — the C
    `adjust_after_fullsweep`, `erl_gc.c:2061-2098`); `young_after_collection(capacity, used) -> usize`
    (the C minor-time young resize, `erl_gc.c:1493`); `should_grow(...)`, `should_shrink(...)`.
- `pub const DEFAULT_MIN_HEAP_WORDS: usize = 233` — the C `H_DEFAULT_SIZE`.
- `pub fn heap_sizes() -> &'static [usize]` — the curated growth table (Fibonacci+1 to index 23, then
  +20%, capped at `2^53 / word`), `LazyLock`-built from `erts_init_gc` (`erl_gc.c:240-303`).
- `pub fn next_heap_size(size, offset) -> usize` — `erts_next_heap_size` over that table.

### `heap_info.rs` — the `garbage_collection` observables
- `pub struct HeapInfo { minor_gcs, fullsweep_after, min_heap_size, min_bin_vheap_size, max_heap_size,
  heap_size, total_heap_size }` — the `erlang:process_info(Pid, garbage_collection)` item
  (`ERTS_PI_IX_GARBAGE_COLLECTION`, `erl_bif_info.c:2073`).
  - `new(...)`; `values() -> [usize; 5]`; `items() -> [(&'static str, usize); 5]`;
    `of<C: HeapObservable + ?Sized>(collector: &C) -> HeapInfo` (derived from any collector).
- `pub trait HeapObservable` — `minor_gcs()`/`heap_size_words()`/`total_heap_size_words()`/`heap_policy()`;
  implemented by `Collector`, `GenImmixCollector` and `AnyCollector` (`AnyCollector::heap_info()` too).
- `pub const GARBAGE_COLLECTION_KEYS: [&str; 5]` — the oracle's emitted key order
  (`max_heap_size, min_bin_vheap_size, min_heap_size, fullsweep_after, minor_gcs`); defaults
  `min_heap_size 233`, `min_bin_vheap_size 46422`, `fullsweep_after 65535`, `max_heap_size 0`.
- Word accounting: `heap_size` = the nursery capacity (C `HEAP_SIZE`); `total_heap_size` = nursery + mature
  capacity (C `heap_sz + old heap`); the message buffer is `beam_message`'s (not counted here).

### `fragment.rs` — heap fragments (collector-side support)
A fragment is a bump-allocated `beam_alloc::Heap` region **outside** every process `HeapSpace`
(the C `ErlHeapFragment`); the collector cannot relocate a term it does not own, so the contract is
**"materialize then collect"** (see Design Decisions).
- `pub struct HeapFragment` — one fragment + its own `OffHeap` list + `FragmentCounters`.
  - `new`/`with_allocator`/`with_counters`/`with_allocator_and_counters`;
    `capacity_words`/`used_words`/`available_words`/`base`/`top`/`limit`/`contains`/`contains_term`;
    `heap()`/`heap_mut()`; `off_heap`/`off_heap_len`/`off_heap_bytes`;
    `register_off_heap(impl Into<OffHeapEntry>) -> slot`; `sweep_off_heap`; `move_off_heap_into(&mut OffHeap)`.
  - `copy_term(&self, root, &mut HeapSpace) -> Result<Eterm>` (shares off-heap — the fragment keeps
    ownership) and `copy_term_into(&mut self, root, to, dst_off_heap) -> Result<Eterm>` (transfers off-heap
    ownership: clone into `dst_off_heap` + release the fragment's ref). A term already in the destination, an
    immediate, or a literal is returned unchanged.
- `pub struct FragmentChain` — the C `next`/`last` list: `push`/`first`/`last`/`get`/`iter`/`len`/`clear`,
  aggregate views (`used_words`/`capacity_words`/`off_heap_*`), `move_off_heap_into`, and the chain-level
  copy-in `copy_roots_into(&mut [Eterm], to, dst_off_heap) -> Result<usize>` (one memo spans the whole chain,
  so a graph spanning fragments copies correctly).
- `pub struct FragmentRoots` — the additive rootset hook: `push`/`as_slice`/`as_mut_slice`/`clear`;
  `materialize(&mut chain, &mut HeapSpace, &mut OffHeap) -> Result<usize>` (relocate fragment-resident roots
  into the process heap **in place**); `gather_roots(&mut Vec<Eterm>) -> start` / `scatter_roots(&[Eterm], start)`.
- `pub struct FragmentCounters` — clonable shared counters (`created`/`dropped`/`live`/`words`/
  `off_heap_bytes`/`off_heap_bytes_peak`) — the no-leak observability.
- The copier is a private iterative, sharing-preserving, **non-destructive** graph copier built on
  `copy.rs`'s `pub(crate)` `move_size`/`for_each_ref` (the frozen layout rules stay in one place); there is
  no `ERTS_HOLE_MARKER` support and no in-place offset rewrite (see Known Gaps).

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
- `pub struct OffHeap` — the per-process list (`MSO(p)`): `push(impl Into<OffHeapEntry>)` (a
  slot-discarding append), `insert(impl Into<OffHeapEntry>) -> usize` (append returning the slot — the
  index an on-heap off-heap reference stores in its `val` word), `get(slot) -> Option<&OffHeapEntry>`,
  `iter() -> impl Iterator<Item = &OffHeapEntry>`, `len()`, `is_empty()`, `overhead_bytes()` (the summed
  off-heap payload bytes, not the `Vec` capacity), and
  `sweep<F: FnMut(&OffHeapEntry) -> bool>(is_live) -> usize` (retains the entries a collection found live,
  releasing the rest — dropping a `Binary` decrements its refcount and frees the payload at zero; dropping
  an `Mso` runs the MSO destructor at zero). The caller supplies the liveness predicate (see Known Gaps).

### `stats.rs` — `GcStats`
`words_copied`, `words_reclaimed`, `objects_copied`, `minor_collections`, `major_collections`
(derives `Clone + Copy + Default + PartialEq + Eq + Debug`).

### `error.rs` — `pub(crate)` constructors only (no new error enum)

## Design Decisions
- **Two generations: `young` nursery + `mature`; minor tenures young→mature (immediate promotion).**
  A **minor** collection Cheney-copies every live young term straight into the `mature` space and
  resets the nursery; because terms are immutable a mature object can never acquire a pointer to a
  later-allocated young object, so the *no-mature→young-pointer* invariant holds after every minor and
  mature objects are **never rescanned** — the generational payoff. A **major** (fullsweep) collection
  evacuates `young` **and** `mature` into one freshly-sized space (the new `mature`), resetting the
  nursery — matching `major_collection`/`full_sweep_heaps` (`erl_gc.c:1854,1962`). The C's finer
  `high_water` splitting of a single block and its per-object tenuring are deliberately replaced by
  this simpler but equally-correct scheme (see Known Gaps).
- **`Collector` sizes the destination up front — no partial-copy rollback.** A minor runs only when
  `mature.available_words() >= young.used_words()` and the policy allows it; otherwise `collect`
  escalates to a major, which allocates a fresh space sized from `size_after_fullsweep` *before*
  copying (the C's `heap_sizes`/`next_heap_size` estimate). A collection therefore never leaves the
  from-space half-forwarded.
- **`GenImmixCollector` = the same copying nursery + a mark-region mature space; its major reclaims the
  mature generation *first*.** Selected by `MatureKind::Immix` (`AnyCollector::from_config`). A **minor**
  promotes the nursery's live terms into the `ImmixSpace` via the shared `copy.rs` forwarding protocol
  over a private `PromoteDest` (`EvacDest` classifying against the nursery's `FromSet`, reserving from
  `ImmixSpace::alloc_words`) — the barrier-free invariant holds because promotion only ever moves young
  objects *into* mature. A **major** is the prescribed `ImmixSpace::collect`-then-promote order, made
  sound for the young→mature edge (which the copying space also has, but which an in-place reclaim cannot
  ignore): both collections first run a read-only `scan_young` that reports the promotion's exact **line**
  count (`needed`) and every nursery reference word pointing into mature (`(slot, term)` pairs). The major
  passes those terms as `promoted` roots to `ImmixSpace::collect` (so a mature object reachable only
  through the live nursery survives), then rewrites each recorded nursery slot to the object's final
  address (the aligned `promoted` entry `collect` updated), and only then promotes — by which point the
  nursery's mature references are `Foreign` and final, so no stale pointer can be produced. If, after the
  reclaim, `free_lines() < needed`, it returns `heap_exhausted` (no panic, no half-forwarded heap). Custom
  `words_reclaimed = mature reclaim + nursery garbage`. Why reclaim-first rather than promote-first: it is
  what lets a would-not-fit minor escalate to a major that *makes room*; promote-first would need the same
  room the escalation is meant to create.
- **`GenImmixCollector` ignores the `MatureKind` in `with_config`.** The type *is* the Immix-backed
  collector, so `with_config` always builds one over `cfg.policy()`; `AnyCollector::from_config` is the
  dispatcher that honours the configured strategy. `Collector` and every existing public signature are
  unchanged — the new collectors are pure additions.
- **`AnyCollector` only forwards.** The strategy-dispatch seam carries no collection logic, so neither
  strategy depends on the other; a test drives one corpus through either behind `collect`/`young`/`stats`.
- **Heap sizing is ERTS-shaped and applied at every collection.** `HeapPolicy` carries the C minimums
  (`min_heap_words`, `min_bin_vheap_words`) and the `fullsweep_after` cadence; `from_spawn_opts` reproduces
  the C `spawn_opt` rounding through `erts_next_heap_size(x, 0)`. `Collector::collect` picks the collection
  with `fullsweep_due`, a minor re-sizes the (now empty) nursery from `young_after_collection`, and a
  fullsweep sizes the fresh mature space from `mature_after_fullsweep` — both re-using the existing
  `should_grow` (`3*cap < 4*need`) / `should_shrink` (`cap > 4*need && cap > min`) rules, floored by
  `min_heap_words` and clamped by `max_heap_words`. The C's minor-time young resize is
  `next_heap_size(stack + MAX(size_before, need), 0)` with `size_before` the **pre-collection usage**
  (`erl_gc.c:1493`), so a nursery a round left mostly empty **shrinks** — the rule the pressure tests
  depend on (a test that must keep a large nursery alive raises `min_heap_words`).
- **Fragments are collector-side and additive — "materialize then collect".** A `HeapFragment` is a bump
  region outside every process space plus its own `OffHeap`. The collector cannot relocate a term it does
  not own, so `FragmentRoots::materialize` / `FragmentChain::copy_roots_into` first copy fragment-resident
  terms into the process heap (a process-space destination, so the flat `&mut [Eterm]` root-slice contract
  can then relocate them normally), after which `gather_roots` → `collect` → `scatter_roots` is an ordinary
  collection. `copy_term` **shares** off-heap payloads (the fragment keeps ownership) while
  `copy_term_into`/`copy_roots_into` **transfer** them (clone + release the fragment's ref ⇒ net refcount
  unchanged), mirroring the C `erts_copy_one_frag` `OH_OVERHEAD` hand-off. The collector still never walks an
  off-heap payload (liveness stays the caller's concern).
### Measured (the GenImmix gate)
`tests/immix_measure.rs` is the original single (favourable) workload — 128 long-lived one-line tuples (8
full Immix blocks) plus 16 dropped / 8 kept transients and 300 nursery garbage tuples, forced fullsweep:
copying re-copies **136 objects / 400 words** vs Immix **moves 8 objects / 128 words and pins 128**
(384 words) — a real movement win, at a footprint cost (400 live words → 2176 resident line-words).

`tests/immix_workloads.rs` generalises it to three shapes through both strategies (`-- --nocapture`):

| workload (forced fullsweep) | strategy | objects moved | words moved | resident mature | pause (median) |
|---|---|---|---|---|---|
| long-lived shared subterm | Copying | 129 | 639 | 639 words | 8–10 µs |
| | Immix | **0** (129 pinned) | **0** | 2304 line-words | 11–12 µs |
| garbage-heavy small live set | Copying | 10 | **20** | **20 words** | 3–5 µs |
| | Immix | 10 | 160 | 160 line-words | **2–3 µs** |
| large heap + promotion pressure | Copying | 633 | 1824 | **1824 words** | **19–27 µs** |
| | Immix | **121** | **288** | 10128 line-words | 47–55 µs |

Both strategies find the **same** live set (`evacuated + pinned == copied`) and reclaim identical garbage.

**Decision — `MatureKind::Copying` remains the default; GenImmix is an opt-in large-heap regime.** GenImmix
wins on the one axis it was designed for — copying work on long-lived data (0 moved vs 129; 5.2× fewer
objects / 6.3× fewer words under large-heap promotion pressure) — but every win is paired with a loss:
(a) **footprint** 3.6–8× (line-granular internal fragmentation: 20 live words occupy 160 line-words on the
garbage-heavy shape); (b) **pause** worse wherever it saves work (2.4× on the large heap — the reclaim walks
the whole root set and every line of the region, and the major walks the nursery twice: `scan_young` then
promotion); (c) an outright loss on the common small-heap/high-churn shape (8× the moved volume for the same
10 objects). **Documented threshold:** Immix pays off only on a *large, long-lived* mature generation
(roughly ≥ 8 full blocks of long-lived data, i.e. thousands of live words) in a memory-unconstrained process;
below that, and for high-churn or memory-tight processes, the copying space wins on both footprint and pause
and stays the default. This matches Part II §D ("worse for small heaps"). Pause numbers are an in-process
median over 5 forced fullsweeps — a direction-of-magnitude signal, not a benchmark; every *assertion* is on
the deterministic counts. (Immix's own reclaim could be sped up by removing the double nursery walk and the
whole-region scan.)
- **In-place Cheney forwarding, exactly as C.** A boxed object's **header word is overwritten** with
  the new BOXED-tagged pointer (`IS_MOVED_BOXED = !is_header`, `erl_gc.h:36`); a cons cell's car is set
  to `THE_NON_VALUE` and its cdr to the new LIST pointer (`IS_MOVED_CONS`, `erl_gc.h:37,42-55`). The
  scan phase mirrors the C `sweep()` switch verbatim: walking the to-space word by word, a `BOXED`/`LIST`
  word is a *reference* to forward, a **transparent** `HEADER` (arityval/record) is stepped over so its
  elements are scanned individually, any other **thing** header is skipped by `thing_arityval+1`
  (opaque interior; map keys/values follow the size word the same way), and `HEADER_SUB_BITS` forwards
  its `orig` field (`erl_gc.c:2328-2348`). A **fun** header is the one exception to the
  `thing_arityval+1` step — see the fun bullet below.
- **Funs are sized and scanned from the inlined-descriptor layout in `beam_term::fun`, *not*
  `header_arity`/`thing_arityval`.** `beam_term::boxed::header_arity` (and therefore `thing_arityval`)
  deliberately returns the C `ERL_FUN_SIZE - 1 == 1` for a fun — the old assumption that a fun block is
  `[header][entry-pointer][env...]` with a single descriptor word. This runtime instead **inlines** the
  C `entry` as descriptor words, so a fun block is `[header][desc_words(kind)][env[0..num_free]]`, where
  `desc_words` = 2 (`Local`), 2 (`External`), 10 (`Imported`), and `env[i]` is at
  `hp[1 + desc_words(kind) + i]`. The copier therefore never uses `header_arity` for a fun:
  `move_size` sizes it as `1 + fun_desc_words(fun_kind_of_header(hdr)) + fun_num_free(hdr)` (==
  `fun_block_words_header`, but computed infallibly since `is_fun_header(hdr)` already held), and the
  Cheney scan advances a fun header by `fun_env_offset_header(hdr)` (== `1 + desc_words`, so the whole
  descriptor is skipped opaquely) instead of `thing_arityval(hdr) + 1`, after which the word-by-word
  loop scans exactly the `num_free` env words. Using `header_arity`/`thing_arityval` for a fun would
  **under-size** the copy by `desc_words(kind) - 1` words (1 for `Local`/`External`, 9 for `Imported`)
  and land the scan on a raw descriptor word (`index` for `Local`, `uniq` bytes for `Imported`),
  following it as a bogus term reference and corrupting the heap. Every fun offset/width comes from
  `beam_term::fun`; `copy.rs` defines none of its own.
- **Object sizing/walking is shared, not duplicated.** `copy.rs` exposes the word-level evacuation
  primitives (`pub(crate)` `EvacDest`, `forward_slot`, `move_size`, `scan_step`, `for_each_ref`, `Target`)
  and both the copying `Copier` and the mark-region `immix/` mark/evacuation phases consume them, so the
  fun-layout sizing rules live in one place. `space.rs`, `collector.rs`, `policy.rs`, `stats.rs`,
  `config.rs` and `error.rs` deal in space geometry, collection policy, statistics and error construction
  and never inspect a term's interior; `mso.rs` mentions "the magic reference of a fun" only in a doc
  comment about off-heap liveness.
- **`xptr_tag` is not needed.** The C ORs the literal bit back in only for literal-area copies
  (`copy.c:2109`); a GC collection copies only in-heap objects, so a copied pointer never carries the
  literal bit. Literal-tagged (and every other out-of-from-space) reference fails the `FromSet.contains`
  test and is left verbatim — the C `erts_is_literal`/area-test rule (`erl_gc.c:2013,2292-2297`).
- **Local object-size traversal; `beam_term` is not modified.** `copy.rs` computes only what the copier
  needs — an *incremental, per-object* `move_size(header, ptr)`: the `nelts` of the C `move_boxed`.
  A **fun** is `1 + desc_words(kind) + num_free` (`fun_desc_words`/`fun_num_free`, see the fun bullet);
  every other boxed object is `header_arity` plus the flatmap/HAMT extras, read through `beam_term`'s
  public accessors (`header_arity`, `is_map_header`, `is_flatmap_header`, `map_header_val`). The scan
  then walks the block word by word. It does **not** call `beam_term::size::size_object`: that is a
  whole-graph size (it cannot size the single object the copier is looking at). The decision and the
  exact formulas live here so no agent re-derives them.
- **Root set = a flat `&mut [Eterm]`.** The C scans the whole stack *word by word* with no frame
  metadata (a word is a term iff its primary tag is BOXED/LIST and it points into a swept area —
  `erl_gc.c:1677-1717`), so a flat slice of root words (live x registers + live stack cells) is
  **semantically faithful**, not a shortcut. Frame-aware stack relocation (`copy_erlang_stack`) and the
  process dictionary / message-queue / saved-argument sub-rootsets belong to `beam_process`.
- **`HeapSpace` wraps `beam_alloc::Heap`; it is not a new allocator.** `beam_alloc::Heap` is the
  production bump heap (its `AllocKind::Heap` exhaustion means "collect", per that crate's
  Design Decisions). `HeapSpace` adds only space semantics (`contains`, `reset`, `base`/`top`/`limit`),
  so the collector and the mutator share one allocation implementation (Prevent-Duplication).
- **The off-heap list is `Mso`/`MsoHandle` + `beam_alloc::RefcBinary` + `OffHeap`, not bolted onto the
  heap.** Both payload kinds live **outside** every process heap; the collector **copies only the small
  heap-resident handle's scalar words** and leaves every off-heap pointer verbatim (the C
  `sweep_off_heap` rule, `erl_gc.c:2995-3110`). Refcount changes are the **off-heap API's** responsibility
  (`inc` on a new handle, `dec`/destructor when the sweep drops a dead one), so the collector never has to
  walk off-heap payloads. The count is `beam_alloc::RefCount` — the workspace's single `erts_refc_*`
  primitive, **re-exported** here (no duplicate) — so a refc binary and an MSO share one implementation.
  The on-heap `BinRef`/`ErlSubBits` *handle term* itself is `beam_term`'s job; this crate owns the
  refcounted off-heap objects, the shared count, and the per-process list.
- **`OffHeap` owns the slot↔object index; a copy boundary only reads it.** An on-heap off-heap reference
  stores a *slot number* into the owning list, so that index association is a property of the list itself
  and lives here (the LCA that owns the MSO model): `insert -> slot`, `get(slot)`, `iter()`. `push` stays
  the slot-discarding append for callers that do not need the index. A slot is stable across `insert`s but
  **not** across a `sweep` (it `retain`s, shifting later slots down), so a caller that keeps raw slot
  numbers must re-resolve them after any sweep.

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only. Heap exhaustion while copying a term is
  `BeamError::Alloc(AllocFailure::new(AllocKind::Heap, bytes))` (never panic, never a crate-local error
  enum, never `unwrap()` on runtime data). `beam_term::TermError` is reconciled to `BeamError` upstream,
  so `?` crosses the boundary.
- **`unsafe` in production code is confined to the word-level code** — `copy.rs` (the Cheney copier),
  `fragment.rs` (the fragment→heap graph copier) and `immix/` (the mark-region space's line/mark/evacuation
  walks) — plus the two narrow `NonNull`-deref / `Box::from_raw` blocks in `mso.rs`; every `unsafe fn`
  carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale
  (`beam_common::safety`). `unsafe_op_in_unsafe_fn` stays clean. `space.rs`, `policy.rs`, `heap_info.rs`,
  `stats.rs`, `config.rs` and the `Collector`/`GenImmixCollector`/`AnyCollector` driving logic are safe Rust
  (only their `#[cfg(test)]` sections call `unsafe alloc_words`).
- **Do NOT modify `beam_term` or `beam_alloc`** — build on their public APIs only. Do not touch
  `../../../erts/` (read-only ground truth) or `../../tools/`.
- **Never break the frozen term layout or the barrier-free invariant** (a minor must not leave a
  mature→young pointer).
- Files target ~1000 lines; split by responsibility. `src/fragment.rs` is marginally over (~1090 lines:
  module-level contract docs plus the in-module fragment-bookkeeping tests; its logic is small).
- Licensing Apache-2.0; deps `beam_common`, `beam_term`, `beam_alloc`, `libc` only.

## Test Strategy
- In-module `#[cfg(test)] mod tests` per module: `space` (geometry/range tests/`reset`/`from_heap`
  round-trip), `copy` (forward of immediates/cons/boxed/nested tuples/lists; forwarding markers;
  out-of-from-space left verbatim), `policy` (table shape: 12, 38, … 233; `next_heap_size` offset 0/1;
  growth/shrink decisions), `mso` (refcount `inc`/`dec`/`dec_and_test`; `MsoHandle` clone/drop runs the
  destructor exactly once; `OffHeapEntry` accessor dispatch; `OffHeap::sweep` releases dead handles, frees
  a swept `RefcBinary`'s payload exactly once, and sweeps a mixed Mso+Binary list selectively).
- **`tests/copy_correctness.rs` — the required copy-correctness evidence.** Build a real term graph in
  a `HeapSpace` (nested tuples + cons lists + boxed leaves, plus deliberate garbage), place tagged
  pointers to live objects into a root slice, run `collect`/`Copier`, then assert: (1) **structural
  preservation** — walking the *copied* graph (from the updated roots) yields the same shape and leaf
  values as the original; (2) **roots updated** — every root word points into the to-space, not the
  from-space; (3) **garbage dropped** — an unreferenced object is absent from the to-space and the
  to-space word count equals the live size. A second case confirms an off-from-space (literal/other)
  reference is left byte-identical.
- **`tests/generations.rs`.** Drive a `Collector` through several minors and a forced major; assert
  `CollectionKind`, that live data survives across generations, that the nursery is empty after a minor,
  and that a too-small mature escalates to a major.
- **`tests/map_copy.rs` / `tests/refc_binary_copy.rs` — the new-shape evidence.** Prove the copier
  preserves the *recently added* interiors: a flat map, a 500-key HAMT (head + inner nodes + collision
  nodes) and the 8-word `[BinRef][ErlSubBits]` refc-binary pair. Each is built in a `HeapSpace` with the
  public `beam_term` writers (`write_map`/`write_refc_binary`), collected as the sole root, and then
  checked *from the updated root* for root relocation, full key/pair (or `orig`) round-trip, and exact
  size accounting. The expected size is measured **independently** of any size heuristic: a `MARKER`
  is pre-written over the reservation and the map's contiguous footprint is recovered by scanning back
  (the copier's `to.used_words()` must equal it), while the pair's size is the frozen `ERL_REFC_BITS_SIZE`.
- **`tests/fun_copy.rs` — the fun inlined-descriptor evidence.** Build `Local`, `External` and `Imported`
  funs (with `beam_term::fun::{write_fun_local, write_fun_external, write_fun_imported}`), each reserved
  to exactly `fun_words(kind, num_free)` words, plus deliberate garbage, then collect all three as roots.
  Assert root relocation, that kind/arity/num_free/module/function/uniq/index/creator-pid all round-trip,
  that boxed free variables are relocated into the to-space (checked from the copied `fun_env`), and that
  `to.used_words()`/`words_copied` equal the exact layout-formula footprint (`1 + desc_words + num_free`
  per fun plus the relocated compound env objects) — proving no under/over-sizing. A fourth test pins the
  per-kind block size (`fun_words == fun_block_words == 1 + desc + num_free`) and **a dedicated test
  proves the descriptor words are opaque**: it plants an `Imported` fun whose `uniq[0..8]` is a
  BOXED-tagged pointer into the from-space, so a scan that stepped `thing_arityval + 1` would follow that
  raw word and corrupt the term, while the correct `fun_env_offset_header` step leaves it verbatim.
- **`immix/*` in-module tests + `tests/immix_{parity,partial_block,measure}.rs` + `tests/gen_immix_collector.rs`
  — the integrated evidence.** The `immix/*` in-module tests cover block/line geometry, line-granular
  allocation + hole reuse after a collection, mark/classify, `AllMarked` pinning, partial-block
  evacuation, out-of-space-reference passthrough, and the accounting. The `immix_collector`/`any_collector`
  in-module tests build a graph through `young_mut()`, drive minors and majors, and check the graph
  survives (root relocation, structure, values) — including the hard major cases (fullsweep cadence, dead
  mature objects reclaimed, a would-not-fit minor escalating to a major, a **young→mature reference
  surviving a major's in-place evacuation**). `tests/gen_immix_collector.rs` and `tests/immix_parity.rs`
  drive the SAME corpus through `AnyCollector::Copying` and `AnyCollector::Immix` behind the public API:
  identical collection-kind sequences, an intact relocated live graph, and — in `immix_parity.rs` over the
  full corpus (maps/funs/refc binaries) — one canonical structural walker that must agree for both.
  `tests/immix_partial_block.rs` proves per-object reclamation and pinning (a `kept`-pinned object keeps
  its address and a reference to it is left verbatim, not a spurious exhaustion). `tests/immix_measure.rs`
  is the measured gate (table in Design Decisions §Measured). `from_config(&GcConfig::default())` must
  select the Copying path.
- **`tests/heap_sizing.rs` — the heap-size policy evidence.** Growth/shrink boundaries, min/max clamping,
  `from_spawn_opts` rounding through `next_heap_size`, the `fullsweep_after` counter incl. 0 (both
  strategies escalate `Minor,Minor,Major` at 2 and `Major,Major` at 0), a tiny nursery growing and an
  oversized nursery/mature shrinking, the relocating Immix fullsweep, the `HeapInfo` derivation + item
  order, and an oracle cross-check against the system `erl` (SKIPS LOUDLY if `erl` is absent).
- **`tests/gc_pressure.rs` / `gc_pressure_structures.rs` / `gc_pressure_offheap.rs` — correctness under
  pressure.** Threshold pressure (many minors/majors at `fullsweep_after` 1/2/default; the nursery is empty
  and every root intact after *each* collection, through both strategies); a HAMT map promoted then
  fullswept (interior nodes relocated across two disjoint spaces); shared/cyclic graphs copied once
  (`GcStats::objects_copied`) with the cycle closing on the copies; a dead off-heap refc binary swept
  (refcount → 0, payload freed exactly once, no double-free) and a fun whose env holds a refc binary kept
  while live / released once garbage. Liveness for the off-heap cases comes from the test's caller-side
  predicate (the documented contract).
- **`tests/fragments.rs` — the fragment evidence.** A fragment-built graph (incl. a graph spanning two
  fragments) copies into a `HeapSpace` intact with the root relocated and the shared list copied once
  (`used_words`); an off-heap refc binary transfers with correct refcounts and is freed exactly once; a
  dropped fragment reclaims its words and off-heap entries (`FragmentCounters`); edge cases (immediate /
  already-destination / foreign term shared; too-small destination → typed `BeamError::Alloc`); and the
  documented rootset contract end-to-end (`materialize` → `gather_roots` → `collect` → `scatter_roots`).
- **`tests/immix_workloads.rs` — the multi-workload measurement** (table in Design Decisions §Measured;
  deterministic counts asserted, pause times only printed).
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Cross-checks against the real system ERTS belong to `../tools/differential/`, not here.

## Known Gaps (intentionally NOT covered in Milestone-1)
- **GenImmix is integrated and measured, but stays off the default path** (decision + numbers in Design
  Decisions §Measured): it wins on copying work only, and only on a large long-lived mature generation,
  while losing on footprint and (there) pause. `MatureKind::Copying` remains the default.
- **The Immix region is fixed-size and never grows.** `ImmixSpace` wraps a `beam_alloc::Heap` and, like
  every semi-space here, grows by GC *replacement* rather than reallocation. A major reclaims in place,
  but if the live nursery still does not fit afterwards the collector returns a typed heap exhaustion
  rather than resizing the region (a `beam_rt`/policy decision, like the copying `Collector`'s fresh-space
  sizing).
- **Promotion atomicity is guaranteed only on the *total* line budget.** A minor promotes only when
  `mature.free_lines() >= needed` (the exact line count of the live nursery graph), and a major checks the
  same after reclaiming. Line-granular allocation means an object needs a *contiguous* run of its lines,
  so a pathologically fragmented mature space could still fail a per-object reservation mid-promotion —
  the same fragmentation trade-off the space documents (`immix/mod.rs`, "line-granular allocation").
- **The C `high_water` single-block layout, per-object tenuring (`GEN_GCS`/`MAX_GEN_GCS`) and the
  `F_NEED_FULLSWEEP` flag protocol** — replaced by the simpler two-space young/mature scheme above.
- **Dirty-scheduler escape for long collections** (`ERTS_POTENTIALLY_LONG_GC_HSIZE`, `erl_gc.h:26`).
- **Frame-aware stack relocation** (`copy_erlang_stack`), the process dictionary, on-heap message/signal
  queues and saved BIF/NIF arguments as sub-rootsets — owned by `beam_process`; this crate takes a flat
  root slice.
- **The fragment copier is non-destructive and has no `ERTS_HOLE_MARKER` / in-place offset rewrite.**
  `fragment.rs` reads the fragment and copies into the destination, where the C `erts_copy_one_frag`
  destructively forwards in place; the equivalent in-place root rewrite is `copy_roots_into` /
  `copy_term_into`. `FragmentChain` uses a `Vec` where the C links with `next`/`last`, and its off-heap list
  is the union of the per-fragment lists (the same set of objects the C stores in the first fragment). A
  failure mid-copy leaves already-copied words as garbage in the destination (still a valid heap —
  immutability means the collector never needs to know).
- **Literal-area collection** (`erts_garbage_collect_literals`, literal-area purge/`xptr_tag`) — literals
  are treated as permanent and left verbatim.
- **A.6 runtime heap options** (`+hms`, `+hmbs`, …) — this crate exposes `HeapPolicy`, not the CLI
  surface (owned by `beam_rt`).
- **`OffHeap` reachability is caller-driven, not integrated with the collector.** `OffHeap` holds both MSO
  and refc-binary entries, but `OffHeap::sweep(is_live)` only applies the caller's predicate — determining
  WHICH on-heap reference (a `BinRef` word, a fun magic ref, a sub-binary pointer) makes a registered
  off-heap object live is `beam_process`'s concern (Stage 5), not the collector's.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/space.rs` → `HeapSpace` (copying semi-space over `beam_alloc::Heap`) + `FromSet`
- `src/copy.rs` → the Cheney copy engine: `Copier`, `forward_roots`/`complete`, `move_size`/`move_*`, `collect`
- `src/collector.rs` → two-generation `Collector`, `Collection`/`CollectionKind`, minor vs major driver, per-collection growth/shrink + `minor_gcs`
- `src/config.rs` → GC-strategy seam: `GcConfig` + `MatureKind::{Copying,Immix}` (default `Copying`; `Immix` = the measurement-gated GenImmix option). `AnyCollector::from_config` is the dispatcher that honours it
- `src/immix/mod.rs` → `ImmixSpace` geometry, block metadata, line-granular allocation, accounting views (`ImmixStats`/`ImmixCounters`)
- `src/immix/collect.rs` → `ImmixSpace` mark/classify/evacuate/finalize phases + its `EvacDest` impl
- `src/immix_collector.rs` → `GenImmixCollector` (copying nursery + mark-region mature), `PromoteDest`, `promote`, `scan_young`
- `src/any_collector.rs` → `AnyCollector` strategy dispatch (`Copying`/`Immix`) over one API
- `src/policy.rs` → `HeapPolicy`, the curated `heap_sizes` table, `next_heap_size`
- `src/mso.rs` → re-exported `RefCount`, `Mso`, `MsoHandle`, `OffHeapEntry`, `OffHeap` — the slot-indexed off-heap list (`push`/`insert`/`get`/`iter`/`sweep`) over off-heap refcounted large objects **and** refc binaries
- `src/stats.rs` → `GcStats`
- `src/error.rs` → `pub(crate)` GC allocation-failure constructors
- `tests/copy_correctness.rs` → the copy-correctness evidence (structure preserved, roots updated, garbage dropped)
- `tests/generations.rs` → `Collector` minor/major/tenure/escalation integration
- `tests/gen_immix_collector.rs` → one corpus through `AnyCollector::Copying` and `AnyCollector::Immix` (parity of collection kinds, roots relocated, live graph intact); `from_config(default)` → Copying
- `tests/immix_parity.rs` → the full corpus (nested tuples/lists/boxed leaves + garbage, flat & >32-key HAMT maps, local/external/imported funs, the refc-binary pair) through both strategies; one canonical structural walker must agree and every root must resolve in its own space
- `tests/immix_partial_block.rs` → targeted per-object reclamation: interleaved live/dead objects; live survive + roots relocated; a `kept`-pinned object keeps its address and a reference to it is left verbatim (not a spurious exhaustion); freed lines reusable
- `tests/immix_measure.rs` → the measured gate (copying copy cost vs Immix evacuated/pinned split), asserted + printed (table in Design Decisions §Measured)
- `tests/map_copy.rs` → flat-map + HAMT-map (a 500-key deep trie, incl. a hand-built collision node) survive a collection; the copied map is sized exactly to its independently-measured footprint
- `tests/refc_binary_copy.rs` → the 8-word `[BinRef][ErlSubBits]` refc-binary pair survives a collection (the `orig` back-pointer is forwarded to the copied `BinRef`, off-heap pointer left verbatim)
- `tests/fun_copy.rs` → `Local`/`External`/`Imported` funs (with compound free variables) survive a collection: metadata + env round-trip, the block is sized exactly to `1 + desc_words + num_free`, and the descriptor words are proven opaque (a descriptor aliasing a from-space pointer must NOT be followed)
- `../../../erts/emulator/beam/erl_gc.c` → the collector being mirrored (`move_boxed`/`move_cons`/`sweep`/`do_minor`/`full_sweep_heaps`) (read-only)
- `../../../erts/emulator/beam/copy.c` → term traversal reference (`size_object_x`/`copy_struct_x`) (read-only)
- `../../erts/emulator/beam/erl_gc.h` → forwarding macros (`IS_MOVED_*`, `follow_moved`, `ErtsInYoungGen`) (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types and the wave-1 dependency position.
- `../../CONTEXT.md` → workspace contracts (§5 GC/memory model, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D ("Memory management in Rust") and §H (Stage 4).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → foundations this crate builds on (do not modify).
- `../beam_process/CONTEXT.md` → the primary consumer that drives collections at safepoints.
