# `beam_alloc` — Allocator Layer (`erts_alloc` Replacement)

## Intent
Owns the **allocation layer** of the Rust ERTS rewrite: the carrier-based multi-block allocator
framework (`erts_alloc` + `erl_alloc_util.c` equivalent), the size classes, the process-wide
allocator registry, and the **per-process heap / bump allocator** the copy engine (`beam_message`)
and the collector (`beam_gc`) construct terms in. It is the **lowest common ancestor for
allocation**: `beam_codec`, `beam_ets`, `beam_gc`, `beam_message` and `beam_process` all depend
downward on it (Stage 0 foundations, Part II §H; §D "the current collector … per-process heaps").

Two responsibilities, deliberately in one node:
1. **Byte allocation** for anything off the process heap — ETS nodes, code, binaries, carriers
   themselves (`Allocator`/`AllocRef`) — mirroring `erts_alloc(type, size)` / `erts_realloc` /
   `erts_free` and the carrier framework of `../../erts/emulator/beam/erl_alloc_util.c`.
2. **Term allocation** within a fixed per-process heap (`Heap`) — the *production* version of the
   minimal `beam_term::heap::Heap` test arena, with identical geometry (`hp`/`hend`, never
   reallocates) but backed by a real allocator and growing by *replacement* (GC), not reallocation.

Ground truth = legacy C (`../../erts/emulator/beam/erl_alloc.h`, `erl_alloc.c`,
`erl_alloc_util.{c,h}`, `erl_goodfit_alloc.c`/`erl_bestfit_alloc.c`/`erl_afit_alloc.c`/
`erl_ao_first*_alloc.c`, `erl_alloc.types`). Read it; never modify it.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_alloc::prelude::*`. The **frozen seam** for dependents is
`allocator`, `heap`, `size_class`, `registry`, `stats`; `block`/`carrier`/`free_list`/`strategy` are
implementation modules (public for testing/reuse, not part of the cross-crate contract).

### `allocator.rs` — the `Allocator` seam
- `pub trait Allocator: Send + Sync + 'static`
  - `fn alloc(&self, layout: core::alloc::Layout) -> Result<NonNull<u8>>`
  - `fn realloc(&self, ptr: NonNull<u8>, old_layout: Layout, new_size: usize) -> Result<NonNull<u8>>`
  - `unsafe fn dealloc(&self, ptr: NonNull<u8>, layout: Layout)` (`# Safety`: `ptr` must come from
    `alloc`/`realloc` on *this* allocator with `layout`)
  - `fn stats(&self) -> AllocStats`; `fn name(&self) -> &str`
  - provided (object-safe, non-generic): `fn alloc_bytes(&self, size: usize) -> Result<NonNull<u8>>`
    (default alignment), `fn alloc_zeroed(&self, layout: Layout) -> Result<NonNull<u8>>`
- `pub type AllocRef = Arc<dyn Allocator>;` — the cheaply-cloneable shared handle dependents hold.
- `pub struct AllocatorConfig` — builder: `name`, `strategy`, `carrier_size` (default 1 MiB),
  `classes: SizeClasses`, `max_footprint` (optional cap for tests), `zeroed_carriers`.
  `AllocatorConfig::default()` / `::builder()` + setters.
- `pub struct CarrierAllocator` — the concrete carrier-backed multi-block allocator.
  `CarrierAllocator::new(config: AllocatorConfig) -> Result<CarrierAllocator>`; `impl Allocator`;
  `share(self) -> AllocRef`; `carrier_count()/carrier_bytes()` (redundant with `stats`).
- `pub const WORD_ALIGN: usize = 8;` (`align_of::<Eterm>()`), `pub const MIN_ALIGN: usize = 16;`
  (`UNIT_SIZE`).

### `size_class.rs` — size classes
- `pub struct SizeClasses` — an ascending table of byte-size classes (each a multiple of
  `UNIT_SIZE`), plus a `large_threshold`.
  `SizeClasses::new(classes: impl IntoIterator<Item = usize>, large_threshold: usize) -> Result<SizeClasses>`
  (validates ascending, non-zero, `UNIT_SIZE` multiples, last ≥ threshold),
  `SizeClasses::default_small_block() -> SizeClasses` (16 … 1 KiB geometric — the M1 default),
  `class_for(size) -> Option<usize>` (`Some(ix)` for sizes ≤ `large_threshold`, else `None` ⇒
  dedicated carrier), `class_size(ix) -> Option<usize>`, `max_class()`, `large_threshold()`,
  `len()`, `is_empty()`, `iter()`.

### `registry.rs` — process-wide allocator registry (`erts_allctrs` equivalent)
- `pub enum AllocType { System, Temporary, ShortLived, Standard, LongLived, EHeap, Ets, Literal,
  Binary, Driver, FixedSize, Test }` — curated from the `allocator` set of
  `../../erts/emulator/beam/erl_alloc.types`. `as_str()`, `index()`, `ALL`.
- `pub fn allocator(ty: AllocType) -> &'static AllocRef` — lazily-initialised, one
  `CarrierAllocator` per type (M1 uses one default `AllocatorConfig` for all types).
- `pub fn default_allocator() -> &'static AllocRef` (= `allocator(AllocType::Standard)`).
- `pub fn all_stats() -> Vec<(AllocType, AllocStats)>`.

### `heap.rs` — the per-process bump heap (`Heap`)
Fixed-capacity bump arena over a carrier-provided block; same geometry as `beam_term::heap::Heap`
but the production allocator. **Never reallocates** (handed-out pointers stay valid for the heap's
lifetime); heap growth is by allocating a *new* larger heap and copying (done by `beam_gc`).
- `Heap::new(capacity_words: usize) -> Result<Heap>` (uses `default_allocator()`)
- `Heap::with_allocator(alloc: AllocRef, capacity_words: usize) -> Result<Heap>`
- `capacity() -> usize` (words), `used()`, `available()`, `peak_used()`, `reset()`,
  `hp() -> *mut Eterm`, `hend() -> *mut Eterm`, `allocator() -> &AllocRef`
- `unsafe fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm>` (uninitialised, contiguous)
- `unsafe fn alloc_tuple(&mut self, n: usize) -> Result<*mut Eterm>` (writes the arityval header)
- `cons(&mut self, car: Eterm, cdr: Eterm) -> Result<Eterm>`
- `tuple(&mut self, elems: &[Eterm]) -> Result<Eterm>`
- `impl Drop` → returns the backing block to the allocator.

### `stats.rs` — instrumentation
- `pub struct AllocStats` (`Clone + Copy + Default + PartialEq + Eq + Debug`): `alloc_calls`,
  `free_calls`, `realloc_calls`, `live_blocks: u64`, `live_bytes`, `peak_live_bytes`,
  `carriers: u64`, `carrier_bytes`, `peak_carrier_bytes`.
- `pub(crate) struct Counters` — the non-atomic counters behind the allocator mutex that produce
  `AllocStats`.

### `strategy.rs` — strategy selector (A.6 config fidelity)
- `pub enum Strategy { FirstFit, GoodFit, BestFit, AFirstFit }` (mirrors `ErtsAlcStrat_t`).
  M1 implements the **segregated first-fit** family; `FirstFit` is the implemented behaviour and the
  other variants currently select it too (plugin seam reserved — see Design Decisions).

## Constraints
- **Errors:** only `beam_common::{Result, BeamError}`. No crate-local error enum, no `panic!` on OOM
  (return `Err(BeamError::Alloc(AllocFailure::new(kind, requested)))`), no `unwrap()` on runtime
  data. Allocation classes map to `AllocKind`: carriers ⇒ `Carrier`, process/slab heaps ⇒ `Heap`,
  large off-heap objects ⇒ `LargeObject`, else `Other`. Invalid alignment/`Layout` ⇒
  `BeamError::BadArg`.
- **`unsafe` confinement:** `unsafe` is allowed **only** in `block`, `carrier`, `free_list`,
  `allocator` and `heap` (raw memory). Every `unsafe fn` carries a `# Safety` section and every
  `unsafe` block a preceding `// SAFETY:` rationale (`../beam_common/src/safety.rs` convention).
  `size_class`, `registry`, `stats`, `strategy`, `error` must be safe Rust. `unsafe_op_in_unsafe_fn`
  stays clean (workspace lint).
- **Carriers come from the OS via `libc::mmap`** (anonymous, private) and are `munmap`'d on free/drop
  — the carrier *is* the OS-level block (legacy `erts_mmap`/mseg). `mmap` failure ⇒
  `AllocFailure{kind: Carrier}`. Page-aligned carriers are relied on for block→carrier recovery.
- **Dependencies (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `libc`, `parking_lot` — and
  nothing else. `beam_term` is needed only for the `Eterm` word in `heap`; `libc` only for `mmap`.
  Adding a dependency edge requires reporting to the crate-layer node (`../`).
- **`Heap` is fixed-capacity by contract** — it must never reallocate (that would move live terms
  and break the term/ABI/inspection contracts, workspace §2). Exhaustion is a *normal* condition.
- Files target ~1000 lines; one responsibility per file.
- Licensing Apache-2.0.

## Design Decisions
- **Slab-per-size-class carrier framework (M1), not the full variable-block engine.** Legacy packs
  arbitrary-size blocks into one carrier and coalesces, with pluggable strategies
  (`goodfit`/`bestfit`/`afit`/`firstfit`) over `erl_alloc_util.c`. M1 implements the carrier
  framework + **one** strategy: each multi-block carrier is dedicated to a single size class and
  subdivided into equal blocks kept on an intrusive free list (classic segregated/slab fit); sizes
  above `SizeClasses::large_threshold()` get a **dedicated single-block carrier** returned to the OS
  on free (the legacy SBC path). This is a documented subset — correct and testable — not a claim of
  strategy parity. `Strategy` exists for the A.6 config surface; the plugin trait is reserved.
- **Every block is preceded by a 2-word `BlockHeader`** `[bhdr: u64][carrier: *mut Carrier]`
  (`block.rs`), exactly as legacy `Block_t` precedes each payload: `bhdr` packs block size, payload
  offset (padding for `align > UNIT_SIZE`), size-class index and flags; `carrier` gives O(1)
  block→carrier recovery without super-alignment. Header size = `UNIT_SIZE` (16 B), so payloads are
  `UNIT_SIZE`-aligned by construction (satisfies `align_of::<Eterm>() == 8`). `bhdr` uses fixed bit
  fields — flags 8b, class 12b (`block::CLASS_NONE` = no class), payload offset 12b and block size
  32b, both in `UNIT_SIZE` units — which bounds the largest encodable block (`block::MAX_BLOCK_SIZE`).
- **`AllocRef = Arc<dyn Allocator>`, not a borrow.** Dependents (e.g. a `beam_ets` table) need to
  outlive the call that created them and sometimes share one allocator across threads; a
  cheaply-clonable `Arc` handle does that without a lifetime parameter on every structure.
- **One thread-safe (`ts`) allocator instance per `AllocType`.** The legacy `ts`/`tspec`/`tpref`
  (thread-specific / thread-preferred) and per-scheduler instances, plus the delayed-dealloc queue
  and thread-progress barrier, are **not** in M1 (see Known Gaps); a single `parking_lot` mutex per
  allocator is the M1 concurrency model.
- **`Heap` exhaustion maps to `BeamError::Alloc(AllocFailure::new(AllocKind::Heap, bytes))`.** The
  instruction set offers no "heap full / need GC" variant and the binding rule forbids a
  crate-local error enum. The engine/GC layer treats an `AllocKind::Heap` failure *from
  `Heap::alloc_words`/`alloc_tuple`* as the signal to collect and retry — exactly how the C VM's
  bump-pointer overflow triggers GC rather than a fatal OOM. Flagged to the parent (which owns
  `beam_common`): if a dedicated `BeamError::HeapExhausted` is ever added upstream, this mapping
  becomes a one-line change here.
- **`beam_term::heap::Heap` is a test arena; this crate's `Heap` is production.** `beam_term`'s heap
  is backed by a `Box<[MaybeUninit<Eterm>]>` and returns `TermResult`; it exists only so
  `beam_term` unit tests can build terms. Dependent crates must use **this** `Heap` (production,
  allocator-backed, returns `beam_common::Result`). The `beam_term` arena is **not** re-exported.
- **`libc::mmap` for carriers, `munmap` on free/drop.** Gives page-aligned, huge-page-eligible
  carriers and a clean `Carrier` identity (`mmap` base = carrier base). Slab carriers are cached per
  size class (reused across frees) and retained for the allocator's lifetime, unmapped only when the
  allocator is dropped (`Drop for AllocatorState`); a dedicated large-block carrier is unmapped
  immediately on `dealloc` (the header's `carrier` word gives O(1) block→carrier recovery). A simple,
  honest retention policy (no `madvise`/shrink in M1).

## Test Strategy
- In-module `#[cfg(test)] mod tests`: `size_class` (lookup boundaries, validation rejects),
  `block` (header encode/decode round-trip, size/offset/class/flags, alignment padding), `carrier`
  (subdivision count, free-list push/pop, exhaustion), `allocator` (alloc/dealloc/realloc round
  trips; alignment assertions for align 8/16/64/…; thousands of mixed-size allocations forcing
  multiple carriers; dedicated-carrier path for large sizes; stats deltas; `mmap`-failure path via
  an injected size cap), `heap` (cons/tuple/alloc_words/alloc_tuple/reset/peak, pointer stability
  and non-reallocation, exhaustion → `AllocKind::Heap` error and no words consumed, `Drop` returns
  the block).
- `tests/allocator.rs`, `tests/heap.rs` — cross-module integration over the *public* API only.
- Prefer property-style loops over boundary ranges; assert **no leak** (drops free every carrier) so
  a Miri / leak-check run is meaningful (recommended in CI, not required by M1).
- Cross-checks against the real system ERTS belong to `../tools/differential/` (the compatibility
  gate), not to this crate.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_alloc`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.

## Known Gaps (intentionally NOT covered in Milestone-1)
- Thread-specific / thread-preferred allocators (`tspec`/`tpref`), per-scheduler instances, and the
  delayed-dealloc queue + thread-progress barrier for cross-scheduler frees.
- Carrier pools / carrier migration between allocator instances; `madvise`/carrier shrinking.
- The variable-block engine with coalescing and the `goodfit`/`bestfit`/`afit` strategy plugins.
- `fix` (fixed-size type) allocators and `erts_alloc_permanent_aligned` cache-line-aligned
  permanents.
- Runtime option parsing (`+MMscs`, `+Mea`, …) — M1 exposes `AllocatorConfig`, not the A.6 CLI
  surface (owned by `beam_rt`).
- Allocation alignment is capped at the OS page size: a request with `align > os_page_size()`
  (typically 4096) is rejected with `BeamError::BadArg`; super-aligned mappings are not implemented.
- The packed `bhdr` word bounds the largest encodable block (`block::MAX_BLOCK_SIZE`) and the class
  count (`block::MAX_CLASS_COUNT`); a request beyond the encodable bound is rejected, never truncated.
- The `erts_alloc`/`enif_alloc` C ABI shims (owned by `beam_abi`) and MSO/binary ref-counted
  off-heap objects (owned by `beam_gc`).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/allocator.rs` → `Allocator` trait, `AllocRef`, `AllocatorConfig`, `CarrierAllocator`
- `src/size_class.rs` → `SizeClasses` (size classes + large threshold)
- `src/registry.rs` → `AllocType` + lazily-initialised process-wide allocators (`erts_allctrs` equiv)
- `src/heap.rs` → the per-process bump `Heap` (`alloc_words`/`cons`/`tuple`/`reset`)
- `src/stats.rs` → `AllocStats` + internal `Counters`
- `src/block.rs` → block layout constants + `BlockHeader` encode/decode (implementation module)
- `src/carrier.rs` → `Carrier` (mmap-backed block, subdivision into equal class blocks) (implementation module)
- `src/free_list.rs` → segregated free-list / slab management (implementation module)
- `src/strategy.rs` → `Strategy` selector (implementation module; A.6 config fidelity)
- `src/error.rs` → `pub(crate)` allocation-failure constructors (no new error enum)
- `tests/` → crate-level integration tests (`allocator.rs`, `heap.rs`)
- `../../erts/emulator/beam/erl_alloc_util.c` → legacy carrier framework being mirrored (read-only)
- `../../erts/emulator/beam/erl_alloc.types` → allocator/type/class inventory (read-only source data)

## Notes for Agents
- **The build command needs the manifest path.** `../scripts/cargo.sh build -p beam_alloc` fails
  (no `Cargo.toml` at the repo root); use
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_alloc`
  (workspace: `... --manifest-path erts_rust/Cargo.toml --workspace`). Sandbox `$HOME` is read-only,
  so always go through the wrapper. Never `cd`; never touch `../tools/`.
- Read `../../erts/emulator/beam/erl_alloc_util.h` for the `Block_t`/`Carrier_t`/`Allctr_t` layout
  and `erl_alloc_util.c` for the alloc/free/realloc flow before changing block or carrier code.
- `AllocKind::Heap` from `Heap::alloc_words` means "collect", not "fatal OOM" — see Design Decisions.
- Interior layouts (carrier size, class spacing, retention policy) are **not** externally pinned;
  only the seam in `allocator.rs`/`heap.rs`/`size_class.rs`/`registry.rs`/`stats.rs` is frozen.

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = wave 0).
- `../../CONTEXT.md` → workspace contracts (§2 frozen term layout, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D ("Memory management in Rust") and §H Stage 0/3.
- `../beam_term/CONTEXT.md` → the frozen term representation this heap stores; `heap.rs` relationship.
- `../beam_gc/`, `../beam_message/`, `../beam_ets/`, `../beam_process/` → primary dependents.
