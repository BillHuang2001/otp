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
option — implemented (`immix/` + `immix_collector.rs`) and selected through `GcConfig`/`AnyCollector` by the
**measurement-derived automatic rule** (`selection.rs`), which keeps the copying space for every
configurable heap size that is not a *large, long-lived* mature generation (see Design Decisions).

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
    — each **pre-checks capacity**: a destination that cannot hold the live set returns the typed
    `heap_exhausted` error *before* `Copier::run`, so a failed collection never leaves the from-space
    half-forwarded (the escalation in `collect` is unchanged).
  - `ensure_capacity(&mut self, need_words, roots) -> Result<Collection>` — the **on-demand growth seam**
    (see Design Decisions §On-demand growth); mirrored on `GenImmixCollector`/`AnyCollector`.
  - `minor_gcs() -> u32` — minors since the last fullsweep (`GEN_GCS`); `impl HeapObservable` (see `heap_info.rs`)
  - After **every** collection the (now empty) nursery is re-sized from `HeapPolicy::young_after_collection`
    using the round's **surviving** words (the young words a minor promoted = `GcStats::words_copied`; `0` for
    a fullsweep, which evacuates the nursery into the mature generation): it is **bounded** — the sole growth
    path is a surviving set larger than the nursery, otherwise it shrinks when the round left it mostly empty
    or keeps its size — and is never sized from stale pre-`reset()` occupancy (see Design Decisions). A
    fullsweep still sizes the fresh mature space from `HeapPolicy::mature_after_fullsweep` (the C
    `adjust_after_fullsweep`).

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
    heap exhaustion before copying anything); `ensure_capacity(need_words, roots)` — the on-demand growth
    seam (same semantics as `Collector`)

### `any_collector.rs` — the strategy-dispatching seam
- `pub enum AnyCollector { Copying(Collector), Immix(GenImmixCollector) }`
  - `from_config(&GcConfig) -> Result<AnyCollector>` — dispatches on the config's **resolved**
    `config.mature()`: `MatureKind::Copying` → `Collector`; `MatureKind::Immix` → `GenImmixCollector`
    (the resolution is the caller's explicit pin, or, by default, the automatic rule)
  - `kind() -> MatureKind`; `mature_choice() -> MatureChoice` (the resolved decision, its source recovered
    by comparing the strategy with the rule's verdict — see `selection.rs`); `policy()`; `young()`;
    `young_mut() -> &mut HeapSpace`; `stats() -> GcStats`; `collect(&mut self, roots) -> Result<Collection>`;
    `ensure_capacity(need_words, roots) -> Result<Collection>`
  - thin forwarding only — lets a caller / test drive one corpus through either strategy.

### `config.rs` + `selection.rs` — the mature-generation decision
- `pub struct GcConfig { pub policy: HeapPolicy, pub selection: MatureSelection }` — the heap-size policy
  plus **how** the mature strategy is chosen. `new`/`auto` (both = the automatic rule),
  `copying(policy)`/`immix(policy)` (explicit pins), `growable()` (= `auto(HeapPolicy::unbounded())`),
  `with_selection`/`with_mature`, `policy()`,
  `selection()`, `mature() -> MatureKind` (the resolved strategy), `mature_choice() -> MatureChoice`;
  `Default` = `GcConfig::new(HeapPolicy::default())` = `Auto`, which resolves to `Copying` for the default
  (233-word) policy — i.e. today's collector exactly.
- `pub enum MatureSelection { Auto, Explicit(MatureKind) }` (default `Auto`) — the *how*; `MatureKind` is
  the *strategy*. `Explicit` beats the rule in both directions.
- `pub enum MatureSource { Automatic, Explicit }`; `pub struct MatureChoice { kind, source, mature_words,
  break_even_words }` with `rule_kind()`/`is_override()`/`describe()` — the **observable** resolved choice.
- `pub const IMMIX_BREAK_EVEN_WORDS: usize = 8 * BLOCK_WORDS` (= 2048) and `IMMIX_BREAK_EVEN_BLOCKS` (= 8);
  `select_mature(mature_words) -> MatureKind` (Immix iff `>=` the break-even — the pure rule);
  `mature_words_of(policy)`; `resolve_mature(&GcConfig) -> MatureChoice`;
  `choice_for(kind, mature_words) -> MatureChoice`. See Design Decisions §Measured for the numbers the
  break-even is derived from.
### `policy.rs` — heap-size policy (the `erts_next_heap_size` analogue)
- `pub struct HeapPolicy` — `min_heap_words`, `max_heap_words` (0 = unbounded), `fullsweep_after`
  (default 65535), `min_bin_vheap_words` (default 46422 = `next_heap_size(32768, 0)`, the C
  `BIN_VH_MIN_SIZE`), `initial_young_words` (233), `initial_mature_words`, `need_reserve_words`;
  `default()`, `from_min_max(min, max)`, `unbounded()` (explicit `max_heap_words: 0`), builder.
  - `from_spawn_opts(min_heap_words, min_bin_vheap_words, fullsweep_after)` — the C `spawn_opt` rule
    (`erl_process.c:12321-12349`): each requested minimum is floored and then rounded through
    `erts_next_heap_size(x, 0)`, so a stored minimum is a growth-table fixed point.
  - `fullsweep_due(minor_gcs) -> bool` — `minor_gcs >= fullsweep_after` (the C `GEN_GCS >= MAX_GEN_GCS`);
    `fullsweep_after == 0` forces a fullsweep on every collection.
  - `next_words(size, offset) -> usize`; `size_after_fullsweep(live_words, need_words) -> usize`;
    `mature_after_fullsweep(capacity, live, need) -> usize` (grow / shrink / keep — the C
    `adjust_after_fullsweep`, `erl_gc.c:2061-2098`); `young_after_collection(capacity, live) -> usize`
    (the **bounded** nursery rule — grows only when the surviving `live` set exceeds the nursery, via offset-0
    `next_words`, else shrinks towards `live` when `should_shrink` or keeps the capacity; the C minor-time
    young resize, `erl_gc.c:1493`); `should_grow(...)`, `should_shrink(...)`.
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
  mature generation *first*.** Selected by `MatureKind::Immix` — an explicit pin, or, by default, the
  automatic rule of §Design Decisions for a large mature generation (`AnyCollector::from_config`). A **minor**
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
  collector, so `with_config` always builds one over `cfg.policy()`; the strategy is decided by
  `AnyCollector::from_config` (on `config.mature()`) / `GcConfig::mature_choice` — the explicit pin or the
  automatic rule. `Collector` and every existing public signature are unchanged — the new collectors are
  pure additions.
- **`AnyCollector` only forwards.** The strategy-dispatch seam carries no collection logic, so neither
  strategy depends on the other; a test drives one corpus through either behind `collect`/`young`/`stats`.
  Its `mature_choice()` derives the decision from the live collector (see `selection.rs`, which is safe Rust
  and depends only on `config`/`policy`/`immix`).
- **Heap sizing is ERTS-shaped and applied at every collection.** `HeapPolicy` carries the C minimums
  (`min_heap_words`, `min_bin_vheap_words`) and the `fullsweep_after` cadence; `from_spawn_opts` reproduces
  the C `spawn_opt` rounding through `erts_next_heap_size(x, 0)`. `Collector::collect` picks the collection
  with `fullsweep_due`; `mature_after_fullsweep` sizes the fresh mature space and still re-uses `should_grow`
  (`3*cap < 4*need`) / `should_shrink` (`cap > 4*need && cap > min`), floored by `min_heap_words` and clamped
  by `max_heap_words`.
  - **The nursery re-size (`young_after_collection`) is a BOUNDED rule, not a growth rule.** `live` is the
    words the round **left in the nursery** — a minor passes what it promoted (`GcStats::words_copied`), a
    fullsweep passes `0`. With `request = live.max(need_reserve_words)`, the *only* grow branch is
    `request > capacity_words` (a surviving set larger than the nursery), which steps up to offset-0
    `next_words(request, 0)`; otherwise `should_shrink(capacity, request)` falls it back to
    `next_words(request, 0)`, and otherwise the capacity is kept. A nursery the round left mostly empty
    therefore **shrinks** and one that turned out about right **keeps** its size — the rule the pressure
    tests depend on (a test that must keep a large nursery alive raises `min_heap_words`).
  - **Why the surviving set, at offset 0.** A nursery is only collected when it is *full*, so sizing it from
    its pre-`reset()` fill (≈ capacity) and stepping offset-1 `next_words(capacity.max(request), 1)` both fire
    on *every* collection, growing it ~20 % per collection with no bound — RSS then scales with the workload
    size. On the baseline `foreign` workload that regime gives young `heap_size` 987 → 833,026 (N=1000) →
    2,487,399 (N=5000) → 7,427,328 (N=20000) with Rust `VmHWM` 29.4 / 77.3 / 227.5 MiB against the oracle's
    flat ~60 MiB; under the bounded rule the nursery is *flat* — a 200-round growing load holds it at its
    233-word default while the total heap plateaus at 609 (Copying) / 1257 (Immix) words
    (`tests/nursery_bounded.rs`).
- **On-demand growth is an explicit seam, not a collect side effect.** Growth of the young/mature space
  normally happens *only* inside a collection (the resizes above); `ensure_capacity(need_words, roots)`
  (on `Collector`/`GenImmixCollector`/`AnyCollector`) adds the caller-driven path a runtime needs when the
  mutator demands room: it collects once to reclaim, then grows the now-empty nursery to the smallest
  curated size covering `need_words` (`grow_nursery`, one bounded step — no loop), returning the
  `Collection` it ran. It cannot "collect until it fits" because `young_after_collection` re-sizes the next
  nursery from the (now empty) *surviving* set — a further collect would shrink the nursery, never grow it;
  the explicit `grow_nursery` is the only way to reach an arbitrary request. A `max_heap_words` cap that
  cannot reach `need_words` yields a typed `heap_exhausted` before any copy.
  `HeapPolicy::unbounded()`/`GcConfig::growable()` express "no cap" (see Handoffs for the `beam_rt` adoption).

### Measured (the GenImmix gate)
`tests/immix_measure.rs` is the original single (favourable) workload — 128 long-lived one-line tuples (8
full Immix blocks) plus 16 dropped / 8 kept transients and 300 nursery garbage tuples, forced fullsweep:
copying re-copies **136 objects / 400 words** vs Immix **moves 8 objects / 128 words and pins 128**
(384 words) — a real movement win, at a footprint cost (400 live words → 2176 resident line-words).

`tests/immix_workloads.rs` generalises it to three shapes through both strategies (`-- --nocapture`):
long-lived shared subterm (copying 129 obj / 639 w vs Immix **0** moved / 129 pinned), garbage-heavy small
live set (10 obj; copying 20 w vs Immix 160 line-w, Immix faster), and large-heap promotion pressure
(copying 633 obj / 1824 w vs Immix 121 obj / 288 w, 10128 resident line-w). Both strategies find the **same**
live set (`evacuated + pinned == copied`) and reclaim identical garbage.

`tests/immix_selection_curve.rs` extends the gate to **two further axes** (deterministic counts asserted,
pauses fastest-of-9 printed only). *Axis 1 — object-size distribution* at a fixed in-place mature size, five
shapes (cons 2w / small tuple 3w / line tuple 16w / multi tuple 17w / mixed): Immix evacuates only the
trailing **partial-block** lines, so its moved words are **distribution-invariant at 96 words** for every
shape while copying's cost tracks the payload (268→2144 words); its resident footprint ranges **1.0×–8.0×**
copying's and pause immix/copy 1.83×–2.38×. *Axis 2 — configured mature capacity* 128→8192 words at a fixed
live set (occupancy 62.5%→22.7%): Immix's moved words / footprint / pause are **flat** across the whole range.
Net: the constant is a **geometry-driven copy-work** break-even (whole-block pinning + line-rounded trailing
evacuation), insensitive to occupancy and object-size distribution — no point below 2048 words becomes
favourable, none above becomes unfavourable.
**Decision — the copying space is the default; GenImmix is a large-live-set regime selected automatically.**
GenImmix wins the copying work on long-lived data, but every win is paired with a loss: (a) **footprint**
3.6–8× (line-granular internal fragmentation); (b) **pause** worse wherever it saves work (≈2.4× on the large
heap — the reclaim walks the whole root set and every line, and the major walks the nursery twice:
`scan_young` then promotion); (c) an outright loss on the common small-heap/high-churn shape (8× the moved
volume for the same 10 objects). Pause is an in-process median over 5 forced fullsweeps — a signal, not a
benchmark; every *assertion* is on the deterministic counts.

### The automatic rule (re-measured break-even, wired)
`tests/immix_break_even.rs` re-measures the gate with a **single-variable sweep**: a purely long-lived corpus
(every object a live root) at object points `[4, 8, 16, 32, 128, 512, 1024]` = 0.25 → 64 whole Immix blocks,
each forced through a fullsweep under **both** strategies, in three shapes (`Line`, `Small`, and `Multi` = a
17-word tuple spanning two lines, the multi-line analogue of the worst case). The two decisive shapes are:
* `Shape::Line` — 16-word tuples, exactly one Immix line per object, so the mark-region footprint equals the
  copying footprint (an apples-to-apples comparison of the *copy work*);
* `Shape::Small` — 3-word tuples, the line-granular worst case (3 useful words per 16-word line).

Per point: `need_reserve_words 64`, `initial_young_words = live_line_words + 4096`,
`initial_mature_words = 2*(live_line_words + 64)` (the `mature_after_fullsweep` fixed point),
`fullsweep_after 65535`, so the forced fullsweep lands in the mark-region space. Measured (counts are
**exact**; pause and the derived ratio are **fastest-of-9**, shown as the min–max over three separate runs):

Per point (moved words copying → immix; counts exact): below one whole block Immix is never fewer and is
strictly more for the small-object shape — 0.25 blk `64 → 64` (Line) / `12 → 64` (Small), 0.5 blk
`128 → 128` / `24 → 128`; from **one whole block up** Immix pins the whole live set and moves **0**
(1/2/8/32/64 blocks → 16/32/128/512/1024 pinned) while copying re-copies the entire live set. Footprint
c/i 1.0× (Line) / 0.19× (Small); pause (immix/copying) flat ≈2.2–2.3× in the selected regime; words saved
per extra µs 0 below one block, then 69→85 (Small) / 264→281 (Line), plateauing at 8 blocks. The exact
per-point tables are printed by `tests/immix_break_even.rs` (deterministic counts also asserted).

**Derived rule — `select_mature` switches at 8 whole blocks (`IMMIX_BREAK_EVEN_WORDS = 2048`).** Reading the
sweep: (1) **below one whole block Immix is never better and is strictly dominated for small objects** — it
has no full block to pin, so it moves 5.3× *more* words (line rounding), occupies 5.3× the footprint and
pauses 2.0–2.7×; the rule must never select it there. (2) **From one block up it pins the whole live set
(0 moved words) and its relative cost is flat** (pause ≈ 2.2–2.3× in the selected regime, footprint 1.0×–5.3×
depending on object size): there is no larger size at which it becomes a *time* win. (3) The worst-case
(small-object) benefit/cost ratio **plateaus at 8 blocks** — 69→75→85 words saved per extra µs at 1→2→8
blocks, then flat (80–87 out to 64 blocks) — the point past which extra size buys no further ratio. The rule
therefore switches at that measured plateau, which also (a) sits 8× above the strict-domination regime,
(b) matches the independent three-shape measurement above ("roughly ≥ 8 full blocks"), and (c) leaves the
**default policy's 233-word mature generation** (under one block) on the copying path — so `GcConfig::default()`
still reproduces today's collector exactly.
- **The rule reads the *configured* mature size and is evaluated once.** `mature_words_of(policy)` is the
  collector's construction-time `initial_mature_words`; a collector keeps its strategy for life (re-selecting
  would mean rebuilding the mature space mid-life), so a heap that *grows* past the break-even later is not
  re-evaluated — a caller that knows its mature generation will grow large pins `GcConfig::immix` up front.
- **Explicit override + observability.** `MatureSelection::Explicit(kind)` beats the rule in both directions
  (`GcConfig::copying`/`GcConfig::immix`/`GcConfig::with_mature`). `GcConfig::mature_choice()` reports the
  exact source; `AnyCollector::mature_choice()` recovers it from the live collector (a pin that agrees with
  the rule is reported as `Automatic`); `MatureChoice::describe()` renders the decision for logs /
  `system_info`-style output. Covered by `tests/mature_selection.rs`.
- **Honest caveat.** The mark-region path's *pause* is worse in the regime where it is selected (≈2.2–2.3×),
  so the rule trades latency for copy work; a pause- or memory-sensitive process should pin
  `MatureSelection::Explicit(MatureKind::Copying)` regardless of size. The break-even is a **copy-work**
  break-even, not a wall-clock one; the measurement is a single-threaded in-process micro-benchmark and every
  harness assertion is on the deterministic counts (timings are printed, never asserted).
- **Standing decision (re-confirmed by the two-axis sweep).** `Auto` **never** selects Immix for the default
  policy — the rule reads the construction-time `initial_mature_words` (default 233 < one 256-word block), so
  the first size it accepts is 2048 configured words, which a default-policy process can never reach however
  large its mature generation grows later. Where Immix *is* selected it is a **wall-clock loss** (pause
  1.24×–2.38×) while moving 2.8×–22× fewer words, so **`MatureKind::Copying` stays the default**; Immix is
  reachable only by an explicit pin or a large configured mature generation
  (`tests/immix_selection_curve.rs`).
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
- In-module `#[cfg(test)] mod tests` live in `space` (geometry/range tests/`reset`/`from_heap`
  round-trip), `policy` (table shape: 12, 38, … 233; `next_heap_size` offset 0/1; growth/shrink
  decisions), `mso` (refcount `inc`/`dec`/`dec_and_test`; `MsoHandle` clone/drop runs the destructor
  exactly once; `OffHeapEntry` accessor dispatch; `OffHeap::sweep` releases dead handles, frees a swept
  `RefcBinary`'s payload exactly once, and sweeps a mixed Mso+Binary list selectively), `heap_info`,
  `config`, `selection`, `any_collector`, `immix_collector`, and `immix/tests.rs`.
  **`copy.rs` and `collector.rs` carry no in-module tests**: the copier's word-level behaviour
  (immediates/cons/boxed/nested tuples/lists, forwarding markers, out-of-from-space left verbatim) is
  pinned by the integration test `tests/copy_correctness.rs`, and the two-generation driver by
  `tests/generations.rs` / `tests/gc_pressure*.rs`.
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
  `tests/gen_immix_collector.rs` additionally drives a **multi-block object** (larger than one Immix block)
  through `PromoteDest::reserve` and its pinning across a major (lone; mixed with small objects + garbage;
  major-first; two runs of different length). `tests/immix_partial_block.rs` proves per-object reclamation
  and pinning (a `kept`-pinned object keeps its address and a reference to it is left verbatim, not a
  spurious exhaustion). `tests/immix_measure.rs`
  is the measured gate (table in Design Decisions §Measured). `from_config(&GcConfig::default())` must
  select the Copying path.
- **`tests/immix_break_even.rs` — the assertion-backed break-even sweep** (§The automatic rule). A purely
  long-lived corpus at `[4, 8, 16, 32, 128, 512, 1024]` objects (0.25 → 64 whole Immix blocks) in three
  shapes (`Line` / `Small` = 3-word objects / `Multi` = 17-word tuples spanning 2 lines), forced through a
  fullsweep under both strategies. A `Law` predicts from corpus geometry which live objects are pinned vs
  evacuated and is asserted at every point (below one block: nothing pinned, strictly more words moved; from
  one block up: the whole live set pinned, `moved_words == 0`).
  `the_break_even_point_is_assertion_checked` pins the constant's whole-block + full-pinning-at-8-blocks
  part (and NOT one line below); pause is printed only, never asserted. Shared `static SERIAL: Mutex<()>`.
- **`tests/mature_selection.rs` — the rule, its override, and cross-threshold correctness.**
  `select_mature`/`AnyCollector::from_config` follow the rule (`GcConfig::default()` → Copying); an explicit
  override beats it **both ways** and is reported as such by `mature_choice()`; and a threshold-crossing
  corpus through **both** strategies runs the **same collection-kind sequence** with every root comparing
  **structurally equal** (`beam_term::compare::eq`) — the strategy changes no observable term semantics. A
  fourth test checks the strategy-agnostic observables (`policy`, `minor_gcs`, nursery capacity).
- **`tests/heap_sizing.rs` — the heap-size policy evidence.** Growth/shrink boundaries, min/max clamping,
  `from_spawn_opts` rounding through `next_heap_size`, the `fullsweep_after` counter incl. 0 (both
  strategies escalate `Minor,Minor,Major` at 2 and `Major,Major` at 0), an oversized nursery/mature
  shrinking and a surviving set larger than the nursery growing it one curated step, the relocating Immix
  fullsweep, the `HeapInfo` derivation + item
  order, and an oracle cross-check against the system `erl` (SKIPS LOUDLY if `erl` is absent).
- **`tests/gc_pressure.rs` / `gc_pressure_structures.rs` / `gc_pressure_offheap.rs` — correctness under
  pressure.** Threshold pressure (many minors/majors at `fullsweep_after` 1/2/default; the nursery is empty
  and every root intact after *each* collection, through both strategies); a HAMT map promoted then
  fullswept (interior nodes relocated across two disjoint spaces); shared/cyclic graphs copied once
  (`GcStats::objects_copied`) with the cycle closing on the copies; a dead off-heap refc binary swept
  (refcount → 0, payload freed exactly once, no double-free) and a fun whose env holds a refc binary kept
  while live / released once garbage. Liveness for the off-heap cases comes from the test's caller-side
  predicate (the documented contract).
- **`tests/immix_workloads.rs` — the multi-workload measurement** (table in Design Decisions §Measured;
  deterministic counts asserted, pause times only printed).
- **`tests/heap_growth.rs` — on-demand growth + half-forward evidence.** Sustained allocation grows the
  **mature** generation (the nursery stays bounded) with every live term intact; a `max_heap_words` cap below
  the live set makes `collect*` return a typed error with **nothing copied** (roots un-forwarded, counters
  unchanged, a second attempt errors identically), while `ensure_capacity` grows the nursery to cover a
  request or errors typed when capped — both strategies.
- **`tests/nursery_bounded.rs` — the bounded-nursery gate** (deterministic counts, no timing; both
  strategies). `the_nursery_and_total_heap_plateau_under_a_growing_working_load` (200 rounds of a growing
  load: the nursery holds at 233 words and the total heap plateaus at 609 Copying / 1257 Immix words),
  `a_live_heavy_round_does_not_grow_the_nursery` (a 6930-word surviving set leaves the 233-word nursery
  steady), `a_garbage_heavy_collection_shrinks_the_nursery` (a 4096-word nursery holding 9 live words falls
  to 90), and `a_larger_single_request_grows_the_nursery_to_just_cover_it` (via `ensure_capacity`).
- **`tests/memory_overhead.rs` — memory / no-leak evidence.** Per-object overhead vs the frozen layout for
  cons/tuple/float/flat-map is asserted `< 1` word (measured **0**); a 140-cycle allocate/collect run asserts
  the RSS (`/proc/self/statm`, skipped loudly off-Linux) and the word accounting plateau — no leak.
- **`tests/stress_cross_generation.rs` — the cross-generation correctness gate.** One graph mixing cons
  (shared + cyclic), nested tuples, flat + >32-key HAMT maps, on-heap binaries/bitstrings, refc binaries, an
  off-heap MSO, Local/External/Imported funs and an external pid, through 6 minors + 2 majors under **both**
  strategies; asserts structural equality after *each* collection, root relocation, single-copy sharing, a
  closing cycle, dropped garbage, and untouched off-heap payloads.
- **`tests/collector_adversarial.rs` — the adversarial copy-correctness set.** Six break-attempts, each run
  through BOTH `AnyCollector::Copying` and `AnyCollector::Immix` (explicit `GcConfig::copying`/`immix` pins,
  asserted via `kind()`) and, where meaningful, the primitive `collect`/`HeapSpace`/`Copier` path, all
  asserting the no-wrong-result / no-panic / no-leak invariants (empty nursery, roots relocated, structure +
  values preserved, exact off-heap accounting): a 100 000-cell cons list + 50 000-deep 1-tuple chain (proves
  the copier is **iterative** — the copied graph is walked by hand, never via the *recursive*
  `compare::eq`/`size_object`); a live graph whose `used_words == capacity_words` (with a one-word-short
  destination returning a typed `BeamError` and **nothing copied**); an off-heap `RefcBinary` root (refcount
  stable, freed exactly once when dead); `Local`/`External`/`Imported` funs with boxed free variables; a
  10 000-element tuple + a flat and a 500-key HAMT map; and an `Mso` (referenced only by its `OffHeap` slot)
  untouched across collections whose destructor runs exactly once when swept.
- **`tests/cyclic_safety.rs` — the cycle-safety gate.** Forges cycles by raw heap writes (self-loop cons,
  2-cycles, a long chain with a back-edge, a deep tuple nesting folding back) and pins: (a)
  `beam_term::size::size_object_bounded` (verified READ-ONLY through `beam_term`'s public API) returns the
  typed `TermError::TermGraphCyclic` — never a hang — and Cyclic wins even when the budget runs out first,
  with the distinct `TermGraphTooLarge` for a large acyclic term; (b) the collector's own per-object sizing
  (`copy.rs::move_size`) is forwarding-pointer based, so a cyclic graph collects correctly and terminates
  under both strategies + the primitive entry (cycles verified by pointer-chasing the **copies**;
  `objects_copied` counts each object exactly once).
- **`tests/fill_barrier.rs` — the young-heap-exhaustion / fill-barrier contract.** A too-small destination
  returns the typed heap exhaustion *before* any copy — roots un-forwarded, bump pointer/counters
  byte-identical, and a second identical attempt errors identically — for `collect_minor`/`collect_major` on
  both `Collector` and `GenImmixCollector` (policy-clamped fullsweep included); `ensure_capacity` fails fast
  under a `max_heap_words` ceiling (collector untouched) and otherwise runs exactly one collection and leaves
  `available_words() >= need_words`; covers boundary cases (`cap == live` / one word below, exact-fit
  nursery) and proves a term written into the grown nursery survives the next collection. Confirms a
  half-forward is never observable.
- **`tests/retained_vs_live.rs` — the GC-side retained-vs-live accounting.** A 220-round allocate/collect
  loop (fresh bounded live graph + garbage each round, `fullsweep_after == 0`) through both strategies, with
  the true live set sized independently and asserted against the words the heap spaces retain: copying
  reaches `mature_used == live` exactly (**0** occupancy surplus) while Immix stays ≤4× live (line-granular;
  measured 1.778×), capacities + occupancy are **constant** across the measured half (no growth per cycle),
  the nursery is empty after every collection, and RSS plateaus ≤1.5× (`/proc/self/statm`, skipped loudly
  off-Linux). Headline numbers are printed with `-- --nocapture`.
- **`tests/immix_selection_curve.rs` — the two-axis selection curve** (§Measured): object-size distribution ×
  mature capacity under both strategies; the "Immix evacuates only the trailing partial-block lines ⇒ its
  moved words are object-size- and occupancy-invariant" invariant is asserted at every point, pauses printed only.
- **`tests/immix_invariants.rs` — the mark-region invariant probes (mutation-verified).** Pins the load-bearing
  `run_major` ordering (reclaim mature in place → rewrite the nursery's mature-reference slots → promote) with
  five no-dangling-interior-pointer cases; `allocated == kept` line-bitmap consistency (`marked`/`kept` reset,
  `used_lines` == allocated popcount, a freed line really free); and no double-free/spurious release
  (`blocks_reclaimed` never counts a pinned `AllMarked` block; repeated collections over an unchanged live set
  are idempotent at byte-identical addresses).
- **`tests/immix_stress.rs` — the mark-region stress set.** Multi-block (257/513-word) large-object promotion +
  reclamation; fragmentation/reuse over six re-fill rounds; repeated minors preserving a promoted term until a
  major reclaims it only when dead; a mixed workload structurally equal under both strategies; and an
  **asserted** live-set-bounded memory accounting (retained `used_lines`·`LINE_WORDS` ≥ live words, slack ≤
  `live·(LINE_WORDS-1)`, and it drops when live objects are dropped).
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Cross-checks against the real system ERTS belong to `../tools/differential/`, not here.

## Known Gaps (intentionally NOT covered in Milestone-1)
- **GenImmix is integrated, measured and now reachable through the automatic rule, but no other crate drives
  it yet** (numbers in Design Decisions §The automatic rule). `selection::select_mature` chooses `Immix` only
  at/above `IMMIX_BREAK_EVEN_WORDS` (8 whole blocks = 2048 words of configured mature generation), which the
  default 233-word policy never reaches — so `GcConfig::default()`/`AnyCollector::from_config(default)` still
  build the copying `Collector`. The mark-region path is reached only by a caller that opts in
  (`GcConfig::immix` / `MatureSelection::Explicit`) or configures a large mature generation. No *outer* crate
  consumes `GcConfig`/`AnyCollector` at all yet, so the strategy rule runs only in this crate's tests: the
  runtime always builds the concrete copying `Collector` (`beam_process::Process`), which `beam_rt` **does**
  drive at its heap-admission point (`beam_rt/src/context/gc.rs::ensure_heap` → `Process::collect`), and
  `beam_rt` **does** use `HeapInfo` (in `system_info`/`process_info(garbage_collection)`). Only the
  mark-region strategy *dispatch* stays unreached in production — Stage-5 work.
- **The Immix region grows only by GC replacement, and only when the policy asks.** `ImmixSpace` wraps a
  `beam_alloc::Heap`; it never reallocates in place. A major whose `HeapPolicy::mature_after_fullsweep`
  target differs from the current capacity **relocates** the whole region into a fresh `ImmixSpace`
  (`immix_collector.rs::relocate_major`), so the region *does* resize. Only when the policy keeps the same
  capacity does an in-place major that still cannot hold the promoted nursery return a typed heap
  exhaustion (no unbounded retry, no partial promotion).
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
- **Literal-area collection** (`erts_garbage_collect_literals`, literal-area purge/`xptr_tag`) — literals
  are treated as permanent and left verbatim.
- **A.6 runtime heap options** (`+hms`, `+hmbs`, …) — this crate exposes `HeapPolicy`, not the CLI
  surface (owned by `beam_rt`).
- **`OffHeap` reachability is caller-driven, not integrated with the collector.** `OffHeap` holds both MSO
  and refc-binary entries, but `OffHeap::sweep(is_live)` only applies the caller's predicate — determining
  WHICH on-heap reference (a `BinRef` word, a fun magic ref, a sub-binary pointer) makes a registered
  off-heap object live is `beam_process`'s concern (Stage 5), not the collector's.

## Handoffs (owned by other nodes — NOT yet adopted)
- **The runtime should adopt the growth seam** (`beam_rt`): it currently hard-caps its term heap and
  translates exhaustion to `error:system_limit`; give the engine context a `beam_gc` collector and call
  `collector.ensure_capacity(heap_need, &mut roots)` from its `ensure_heap` (the root set —
  `x_regs[0..live]` ++ stack — is already threaded there), keeping the `system_limit` translation for a
  returned typed error. `HeapPolicy::unbounded()`/`GcConfig::growable()` express "no cap".
- **Test-id / MSO / `compare` caveats (cross-crate).** A pid is a heap term via
  `beam_term::thing::write_external_pid` (4 words) **and** `beam_term::compare::eq` orders it
  **structurally through its parts** (field order per the C `erts_cmp`), with `external_pid_parts`
  available to read the parts directly; `compare::eq` still **recurses forever on a cycle**. An MSO
  has no on-heap reference *term*: the safe reference is the `OffHeap::insert -> slot` index, never a
  raw pointer. Reach a refc binary's `BinRef` via the `ErlSubBits` `orig` slot, not pointer adjacency.
  Covered by `tests/stress_cross_generation.rs`.
- **`HeapInfo` is surfaced by the runtime.** `beam_rt` builds it directly (`HeapInfo::new` and `HeapObservable`)
  for `erlang:process_info(Pid, garbage_collection)` and `system_info(garbage_collection)`, in
  `GARBAGE_COLLECTION_KEYS` order (`beam_rt/src/native/memory/mod.rs`, `src/info/memory.rs`); only
  `AnyCollector::heap_info()` itself still has no production caller.
- **Repo-root owned** (`../../CHANGE.md`): owed entries for the heap-sizing policy (the C-faithful
  per-collection growth/shrink, the `min_bin_vheap_words`/`fullsweep_after` fields and `HeapInfo`)
  and the measured GenImmix decision.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/space.rs` → `HeapSpace` (copying semi-space over `beam_alloc::Heap`) + `FromSet`
- `src/copy.rs` → the Cheney copy engine: `Copier`, `forward_roots`/`complete`, `move_size`/`move_*`, `collect`
- `src/collector.rs` → two-generation `Collector`, `Collection`/`CollectionKind`, minor vs major driver, bounded per-collection nursery re-size + mature growth/shrink + `minor_gcs`, the capacity pre-check (no half-forward) and the `ensure_capacity` growth seam
- `src/config.rs` → GC-strategy seam: `GcConfig { policy, selection }`, `MatureKind::{Copying,Immix}` (the *strategy*; default `Copying`), `MatureSelection::{Auto,Explicit}` (the *how*; default `Auto` = the automatic rule). `mature()` is the resolved strategy, `mature_choice()` the observable decision, `growable`
- `src/selection.rs` → the **automatic rule**: `IMMIX_BREAK_EVEN_WORDS`/`IMMIX_BREAK_EVEN_BLOCKS`, `select_mature`, `mature_words_of`, `resolve_mature`/`choice_for`, `MatureSource`, `MatureChoice` (`rule_kind`/`is_override`/`describe`). Safe Rust; depends only on `config`/`policy`/`immix`
- `src/immix/mod.rs` → `ImmixSpace` geometry, block metadata, line-granular allocation, accounting views (`ImmixStats`/`ImmixCounters`)
- `src/immix/collect.rs` → `ImmixSpace` mark/classify/evacuate/finalize phases + its `EvacDest` impl
- `src/immix_collector.rs` → `GenImmixCollector` (copying nursery + mark-region mature), `PromoteDest`, `promote`, `scan_young`, `ensure_capacity`
- `src/any_collector.rs` → `AnyCollector` strategy dispatch (`Copying`/`Immix`) over one API; `from_config` dispatches on the resolved `config.mature()`, `mature_choice()` reports the decision, `ensure_capacity` forwards
- `src/policy.rs` → `HeapPolicy` (min/max heap, `min_bin_vheap_words`, `fullsweep_after`), the curated `heap_sizes` table, `next_heap_size`, `from_spawn_opts`/`fullsweep_due`/`mature_after_fullsweep` + the bounded `young_after_collection`, `unbounded`
- `src/heap_info.rs` → the `garbage_collection` observables: `HeapInfo`, `HeapObservable`, `GARBAGE_COLLECTION_KEYS`
- `src/mso.rs` → re-exported `RefCount`, `Mso`, `MsoHandle`, `OffHeapEntry`, `OffHeap` — the slot-indexed off-heap list (`push`/`insert`/`get`/`iter`/`sweep`) over off-heap refcounted large objects **and** refc binaries
- `src/stats.rs` → `GcStats`
- `src/error.rs` → `pub(crate)` GC allocation-failure constructors
- `tests/copy_correctness.rs` → the copy-correctness evidence (structure preserved, roots updated, garbage dropped)
- `tests/generations.rs` → `Collector` minor/major/tenure/escalation integration
- `tests/gen_immix_collector.rs` → one corpus through `AnyCollector::Copying` and `AnyCollector::Immix` (parity of collection kinds, roots relocated, live graph intact); `from_config(default)` → Copying
- `tests/immix_parity.rs` → the full corpus (nested tuples/lists/boxed leaves + garbage, flat & >32-key HAMT maps, local/external/imported funs, the refc-binary pair) through both strategies; one canonical structural walker must agree and every root must resolve in its own space
- `tests/immix_partial_block.rs` → targeted per-object reclamation: interleaved live/dead objects; live survive + roots relocated; a `kept`-pinned object keeps its address and a reference to it is left verbatim (not a spurious exhaustion); freed lines reusable
- `tests/immix_measure.rs` → the measured gate (copying copy cost vs Immix evacuated/pinned split), asserted + printed (table in Design Decisions §Measured)
- `tests/immix_break_even.rs` → the re-measured break-even sweep: a long-lived corpus at 0.25–64 whole Immix blocks in three shapes through both strategies, printing pause/footprint/moved words and asserting the deterministic counts (table in Design Decisions §The automatic rule)
- `tests/mature_selection.rs` → the automatic rule (Copying below / Immix at-or-above `IMMIX_BREAK_EVEN_WORDS`), the explicit override in both directions, and both collectors' structural agreement on a threshold-crossing corpus
- `tests/map_copy.rs` → flat-map + HAMT-map (a 500-key deep trie, incl. a hand-built collision node) survive a collection; the copied map is sized exactly to its independently-measured footprint
- `tests/refc_binary_copy.rs` → the 8-word `[BinRef][ErlSubBits]` refc-binary pair survives a collection (the `orig` back-pointer is forwarded to the copied `BinRef`, off-heap pointer left verbatim)
- `tests/fun_copy.rs` → `Local`/`External`/`Imported` funs (with compound free variables) survive a collection: metadata + env round-trip, the block is sized exactly to `1 + desc_words + num_free`, and the descriptor words are proven opaque (a descriptor aliasing a from-space pointer must NOT be followed)
- `tests/heap_sizing.rs` → the growth/shrink policy + `fullsweep_after` counter + `HeapInfo` derivation + the system-`erl` oracle cross-check
- `tests/gc_pressure.rs` / `tests/gc_pressure_structures.rs` / `tests/gc_pressure_offheap.rs` → threshold pressure, HAMT promotion, shared/cyclic graphs, dead refc binaries, fun-env refc binaries (through both strategies)
- `tests/immix_workloads.rs` → the multi-workload copying-vs-Immix measurement (Design Decisions §Measured)
- `tests/heap_growth.rs` → on-demand growth (the mature generation grows while the nursery stays bounded, live terms intact), the no-half-forward typing under a too-small cap, and the `ensure_capacity` seam
- `tests/nursery_bounded.rs` → the bounded-nursery evidence: a growing load plateaus the nursery + total heap, a live-heavy round does not grow the nursery, a garbage-heavy collection shrinks it, and a larger single request grows it via `ensure_capacity`
- `tests/memory_overhead.rs` → per-object overhead vs the frozen layout + the RSS/word-accounting no-leak plateau
- `tests/stress_cross_generation.rs` → the mixed-term cross-generation correctness gate (cons/tuples/maps/binaries/refc/MSO/funs/pids) through both strategies
- `tests/collector_adversarial.rs` → the adversarial copy-correctness set (deep nesting, exact-capacity boundary, off-heap refc binary + `Mso` freed exactly once, funs with boxed env, large tuple + flat/HAMT maps) through both strategies
- `tests/cyclic_safety.rs` → `size_object_bounded` cyclic verdict (typed `TermGraphCyclic`, no hang) + the collector's forwarding-pointer cycle-safety under both strategies
- `tests/fill_barrier.rs` → the young-heap-exhaustion / fill-barrier contract (typed error before any copy, no half-forward, `ensure_capacity` growth, boundary cases)
- `tests/retained_vs_live.rs` → GC-side retained-vs-live accounting over a long allocate/collect loop (steady-state capacities, bounded non-growing surplus, RSS plateau)
- `tests/immix_selection_curve.rs` → the two-axis mature-strategy selection curve (object-size distribution × mature capacity); deterministic counts asserted, pause printed only
- `tests/immix_invariants.rs` → the mark-region invariant probes (major reclaim→slot-rewrite→promote ordering, line-bitmap consistency, no double-free; mutation-verified)
- `tests/immix_stress.rs` → the mark-region stress set (multi-block promotion, fragmentation/reuse, repeated-minor promotion, mixed workload, asserted live-set-bounded accounting)
- `../../../erts/emulator/beam/erl_gc.c` → the collector being mirrored (`move_boxed`/`move_cons`/`sweep`/`do_minor`/`full_sweep_heaps`/`adjust_after_fullsweep`) (read-only)
- `../../../erts/emulator/beam/copy.c` → term traversal reference (`size_object_x`/`copy_struct_x`) (read-only)
- `../../erts/emulator/beam/erl_gc.h` → forwarding macros (`IS_MOVED_*`, `follow_moved`, `ErtsInYoungGen`) (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types and the wave-1 dependency position.
- `../../CONTEXT.md` → workspace contracts (§5 GC/memory model, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D ("Memory management in Rust") and §H (Stage 4).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → foundations this crate builds on (do not modify).
- `../beam_process/CONTEXT.md` → the primary consumer that drives collections at safepoints.
