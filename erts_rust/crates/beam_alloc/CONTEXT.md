# `beam_alloc` — Allocator Layer (`erts_alloc` Replacement)

## Intent
Owns the **allocation layer** of the Rust ERTS rewrite: the carrier-based multi-block allocator
framework (`erts_alloc` + `erl_alloc_util.c` equivalent), the size classes, the process-wide
allocator registry, the full allocation-*type* inventory, the mmap-backed **mseg** segment allocator,
the per-allocator **carrier policy** (`+M<alloc><opt>` thresholds) and the **per-process heap / bump
allocator** the copy engine (`beam_message`) and the collector (`beam_gc`) construct terms in.

It is the **lowest common ancestor for allocation**: `beam_codec`, `beam_ets`, `beam_gc`,
`beam_message` and `beam_process` all depend downward on it (Stage-0 foundations, Part II §H; §D).
Wave 0 — it may depend only on `beam_common` / `beam_term`.

Responsibilities (deliberately one node):
1. **Byte allocation** for anything off the process heap — ETS nodes, code, binaries, carriers
   (`Allocator`/`AllocRef`) — mirroring `erts_alloc(type, size)` / `erts_realloc` / `erts_free` and
   the carrier framework of `../../erts/emulator/beam/erl_alloc_util.c`.
2. **Term allocation** inside a per-process term heap — the *production* bump arena the copy engine
   (`beam_message`) and the collector (`beam_gc`) construct terms in. Two shapes: `Heap`
   (fixed-capacity; it never reallocates, so growth is by *replacement* (GC), exactly as
   `beam_term::heap::Heap` does) and `ReservedHeap` (reserves a large address range once, commits a
   slice of it, and grows by raising the committed limit **in place** — no move — until the
   reservation itself is exhausted).
3. **Refcounted off-heap objects** — the shared atomic `RefCount` (`erts_refc_*` semantics) and the
   refcounted off-heap byte buffer `RefcBinary` built on it.
4. **The `erts_alloc` inventory and its tuning/statistics surface** — the parsed
   `erl_alloc.types` table (`types`), the `erts_alloc:info/0`-shaped reporting (`stats`), and the
   `+M<alloc><opt>` carrier policy (`policy`).

Ground truth = legacy C (`../../erts/emulator/beam/erl_alloc.h`, `erl_alloc.c`, `erl_alloc_util.{c,h}`,
`erl_alloc.types`, `erl_mseg.c`, `erl_goodfit_alloc.c`/`erl_bestfit_alloc.c`/`erl_afit_alloc.c`). Read
it; never modify it.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_alloc::prelude::*`. The **frozen seam** for dependents is
`allocator`, `heap`, `reserved_heap`, `size_class`, `registry`, `stats`, `types`, `refcount`, `binary`,
`policy`; `block`/`carrier`/`free_list`/`mbc`/`strategy`/`mseg`/`bump` are implementation modules
(public for testing/reuse, **not** part of the cross-crate contract).

### `allocator` — the `Allocator` seam (facade + submodules)
Module layout: `allocator.rs` (facade: docs, `WORD_ALIGN`/`MIN_ALIGN`, `AllocRef`, re-exports),
`allocator/trait.rs` (`Allocator`), `allocator/config.rs` (`AllocatorConfig`/builder),
`allocator/state.rs` (`AllocatorState`), `allocator/core.rs` (`CarrierAllocator`), `allocator/tests*.rs`.
- `pub trait Allocator: Send + Sync + 'static` (object-safe; `alloc`/`realloc`/`dealloc`/`stats`/`name`
  + provided `alloc_bytes`, `alloc_zeroed`) — the untyped base.
- `pub type AllocRef = Arc<dyn Allocator>;` — the cheaply-clonable shared handle.
- **Typed / sized entry points (interposition-friendly, `erts_alloc`-shaped).** Provided on the trait
  so every implementor gets them; defaulted implementations forward to the untyped methods:
  `alloc_typed(TypeId, Layout)`, `realloc_typed(TypeId, ptr, old_layout, new_size)`,
  `dealloc_typed(TypeId, ptr, Layout)`, `alloc_unsized(TypeId, size)` (recovers the alignment on
  free — the legacy `erts_alloc(type,size)`/`erts_free(type,ptr)` pair), `dealloc_unsized(TypeId, ptr)`,
  `type_stats(TypeId) -> TypeStats`, `allocator_info() -> AllocatorInfo`.
- `pub struct AllocatorConfig` — builder: `name`, `strategy`, **`policy: CarrierPolicy`**, `carrier_size`
  (default 1 MiB), `classes: SizeClasses`, `max_footprint`, `zeroed_carriers`. `AllocatorConfig::builder()`.
- `pub struct CarrierAllocator` — the concrete carrier-backed allocator. `new(config)`, `share()`,
  `carrier_count()`, `carrier_bytes()`, `impl Allocator`.
- `pub const WORD_ALIGN: usize = 8;` (`align_of::<Eterm>()`), `pub const MIN_ALIGN: usize = UNIT_SIZE;`.

### `types` — the full `erts_alloc.types` inventory (243 types)
A hand-maintained **source table** transcribed from `../../erts/emulator/beam/erl_alloc.types` (the
file is legitimately long: it *is* the data). Exposes:
- `pub enum AllocatorId` (12: systemA/temp/sl/std/ll/eheap/ets/binary/literal/fix/driver/test — `ALL`,
  `as_str`, `index`); `pub use crate::types::AllocatorId as AllocType;` (backwards-compat alias).
- `pub enum ClassId` (6: mseg/sys_alloc/sys_fix_alloc/ets_alloc/sl_alloc/fix_alloc — `ALL`, `as_str`, `index`).
- `pub struct TypeFlags(u32)` — `MSEG`, `SUPER_ALIGNED`, `FIXED_SIZE`, `HEAP`, `SBC`, `MBC` (`union`,
  `contains`, `intersects`, `from_bits_truncate`, const `for_allocator(AllocatorId)`).
- `pub struct TypeEntry` (`name`, `allocator`, `class`, `flags`, `alloc_size`).
- `pub const ALLOC_TYPES: [TypeEntry; TYPE_COUNT]`, `pub const TYPE_COUNT`, `pub const EXPECTED_TYPE_COUNT: usize = 243`;
  `TypeId` (`index`/`entry`/`name`/`allocator`/`class`/`flags`/`from_index`/`from_name`),
  `pub fn type_by_name`, `pub fn types_for_allocator`.

### `registry` — process-wide allocator registry (`erts_allctrs` equivalent)
- `pub use crate::types::AllocatorId as AllocType;`
- `pub fn allocator(ty: AllocType) -> &'static AllocRef` — lazily-initialised, one `CarrierAllocator`
  per type; `default_allocator()`, `all_stats() -> Vec<(AllocType, AllocStats)>`.
- `pub fn alloc_util_allocators() -> Vec<&'static str>` — the `system_info(alloc_util_allocators)`
  list; `pub fn types_for(AllocatorId) -> Vec<TypeId>`.

### `size_class` — size classes (sbc/mbc ladders)
`SizeClasses` (ascending, `UNIT_SIZE`-multiples). Constructors: `new(classes, large_threshold)`,
`default_small_block()`, `default_sbcs()`, `default_mbcs()`, `default_legacy()`. Accessors:
`class_for`, `class_size`, `max_class`, `large_threshold`, `is_small`, `sbc_threshold`, `mbc_threshold`,
`sbc_classes`, `mbc_classes`, `len`, `is_empty`, `iter`. Constants `SBC_THRESHOLD_DEFAULT = 512 KiB`,
`SMALLEST_MBC_SIZE_DEFAULT = 1 MiB`, `MBC_THRESHOLD_DEFAULT = 10 MiB`.

### `policy` — legacy carrier policy (`+M<alloc><opt>`)
`CarrierPolicy` with all **12** legacy options, in the oracle's item order
`sbct asbcst rsbcst rsbcmt mmmbc mmsbc lmbcs smbcs acful acul acnl acfml`:
`PolicyOptionKind::ALL`, `CARRIER_POOL_THRESHOLDS`, `UTILIZATION_LIMITS`; `for_allocator(AllocatorId)`
+ the per-allocator const presets (`ALLCTR_INIT`, `TEMP_ALLOC`, `SL_ALLOC`, `STD_ALLOC`, `LL_ALLOC`,
`EHEAP_ALLOC`, `ETS_ALLOC`, `FIX_ALLOC`, `BINARY_ALLOC`, `DRIVER_ALLOC`, `LITERAL_ALLOC`, `TEST_ALLOC`);
`get`/`set`/`set_option`/`with_option` (invalid input ⇒ `BeamError::BadArg`); derived accessors
`small_block_carrier_threshold()` (= `sbct`), `single_block_carrier_cap()`, `multi_block_carrier_cap()`,
`minimum_multi_block_carrier_size()`, `maximum_multi_block_carrier_size()`,
`effective_largest_mbc_size()`, `uses_single_block_carrier(size)`, `is_unlimited`. `UNLIMITED = usize::MAX`.

### `stats` — instrumentation (`erts_alloc:info`-shaped)
The file also carries a large in-module test module (in-module because the counter internals are only
exercisable there).
- `pub struct TypeStats { blocks, bytes, peak_bytes }` + `pub type TypeCounters = TypeStats`.
- `pub const STATS_TYPE_SLOTS: usize = 512;` (invariant `types::TYPE_COUNT <= STATS_TYPE_SLOTS`).
- `pub struct MsegStats { segments, peak_segments, bytes, peak_bytes, cached, reused, mapped_segments,
  mapped_bytes, peak_mapped_bytes, unmaps, calls }` — built from the **process-global**
  `crate::carrier::mseg().stats()` via `impl From<MsegInfo> for MsegStats` (a *live view*, not zeros).
- `pub struct AllocStats { alloc_calls, free_calls, realloc_calls, live_blocks, live_bytes,
  peak_live_bytes, carriers, carrier_bytes, peak_carrier_bytes, sbc_carriers, mbc_carriers, mseg,
  types: [TypeCounters; STATS_TYPE_SLOTS] }` — `Copy` (inline type table ⇒ ~12 KiB snapshot; taken by
  introspection, never on the fast path). `AllocStats::type_stats(slot)` is bounds-safe.
- `pub struct AllocatorInfoItem { key, value }`, `pub enum InfoValue`,
  `pub struct AllocatorInfo` — `from_stats(name, &AllocStats)`, `push_policy(&CarrierPolicy)` (the 12
  legacy items in oracle order), `push_type(name, TypeStats)`.

### `heap` — the per-process bump heap (`Heap`)
Fixed-capacity bump arena over a carrier-provided block; **never reallocates** (handed-out pointers
stay valid; growth is a new heap + copy by `beam_gc`). `new(capacity_words)`,
`with_allocator(alloc, capacity_words)`, `capacity`, `used`, `available`, `peak_used`, `reset`,
`reset_to(used) -> Result<()>`, `hp`, `hend`, `allocator`, `unsafe alloc_words`,
`unsafe alloc_tuple`, `cons`, `tuple`, `Drop`
returns the block. Exhaustion ⇒ `BeamError::Alloc(AllocKind::Heap)` (= "collect", not fatal OOM).
`reset()` rewinds to empty and `reset_to(used)` to a **mark** (the exact inverse of allocating `used`
words, `used` in *term words*): the first `used` words stay owned and their contents are not touched,
everything after them is only freed-and-reused (never wiped), and `peak_used` is preserved by both
(the high-water mark is live-word history, not the offset) — `reset_to(0)` is therefore exactly
`reset()`. A `used` **above** the current `used()` is the typed `BeamError::BadArg` with the heap left
completely unchanged (never a silent clamp, never a panic): rewinding *forward* would claim words the
heap never handed out — the same refusal rule [`ReservedHeap::relocate`] applies to an
undersized reservation. This is what lets a *pooled* heap that reaches a caller already owning words
handed over to another caller be rewound precisely instead of only when it is empty.

### `reserved_heap` — the reservation-capable per-process term heap (`ReservedHeap`)
The **frozen seam** for hosts that must grow a live process heap. One backing block of
`reserved_words` term words, obtained exactly once from an `AllocRef` (mseg/`mmap`-backed, so reserved
pages fault lazily and reserving address space does not commit RSS) and released exactly once in
`Drop`. `committed_words` (alias `capacity()`, the `Heap::capacity` meaning) is the part currently
handable out, so `hp() == base() + used()`, `limit() == base() + committed_words()` and
`reservation_end() == base() + reserved_words()`. Constructors `new(reserved_words, initial_words)` /
`with_allocator(alloc, reserved_words, initial_words)` (`initial_words > reserved_words` ⇒ `BadArg`;
a `reserved_words * size_of::<Eterm>()` overflow ⇒ `BadArg`, never a wrap or a silent saturation).
`try_grow(need_words) -> Result<GrowOutcome>`; `relocate(new_reserved_words) -> Result<isize>` (the
byte delta the live block moved by); `reset`; `alloc_words`/`alloc_tuple`/`cons`/`tuple` with
`Heap`'s exact exhaustion convention; `base`, `hp`, `limit`, `reservation_end`, `reserved_words`,
`committed_words`, `capacity`, `used`, `available`, `peak_used`, `allocator`. Plus
`pub enum GrowOutcome { GrewInPlace, MoveRequired }`. Re-exported at the crate root and in the prelude.

**Host-adoption contract (the point of the type):**
1. On an `AllocKind::Heap` exhaustion the host calls `try_grow(n)` and retries. `GrewInPlace` ⇒ the
   committed limit was raised (to exactly `used + n`), the bump pointer did **not** move — a cached
   `hp()` stays valid, but a cached `limit()` **must be re-read**. `try_grow` applies no growth policy;
   the host owns it.
2. `MoveRequired` ⇒ the request does not fit the reservation, and `try_grow` changed **nothing**. The
   host must supply more address space: `relocate` to a larger reservation, or a fresh larger
   `ReservedHeap` plus a pointer-aware copy.
3. After any move every cached raw term pointer is invalid. The whole live block shifts by a
   **constant delta** (the `relocate` return), so the caller re-bases by adding that delta to its
   external roots (`x_regs[0..live]` ++ the Erlang stack — the same root-set shape the collector takes,
   supplied by `beam_engine`'s `Machine::{x_mut, fr_mut, stack_mut}`) **and** to the pointer words
   inside the moved region.
4. `relocate` cannot fix the interior pointers itself — telling a pointer word from raw
   bitstring/refc-binary payload bytes needs term-structure knowledge owned by
   `beam_term`/`beam_gc` — so the fully-consistent move remains the collector's pointer-aware copy
   (`copy_struct`/Cheney), i.e. growth by *replacement*. Never cache a raw term pointer across a move.

### `bump` — the shared bump-pointer term-construction primitives (private, crate-internal)
`alloc_words_at`/`alloc_tuple_at`/`cons_at`/`tuple_at`: pure `(hp, available, …)` helpers that
reserve-or-reject and write, with no state and no allocation. Both `Heap` and `ReservedHeap` delegate
to them, so the term-writing logic (arityval header, `CAR`/`CDR` offsets, the exhaustion error) exists
exactly once and each heap keeps only its own `used`/`peak` bookkeeping. A rejected request **writes
nothing** and reports `error::heap_exhausted` (`AllocKind::Heap`).

### `refcount` — the shared atomic reference count (`erts_refc_*`)
`#[repr(transparent)] pub struct RefCount { count: AtomicUsize }` — `new`, `count`, `inc`, `dec`,
`dec_and_test(min)`. Safe Rust; `Send + Sync`. The **single** refcount primitive for the workspace;
`beam_gc` re-exports this one rather than defining its own.

### `binary` — off-heap refcounted large binary (`RefcBinary`)
- `pub struct RefcBinary` — `new(bytes)`, `with_allocator(alloc, bytes)`, `len`, `is_empty`,
  `as_bytes`, `refcount`, `data_ptr`. `Clone` bumps the refcount; last `Drop` frees once through the
  **same `AllocRef` + exact `Layout`**.
- Allocates through **`alloc_typed(TypeId::from_name("BINARY"), layout)`** / `dealloc_typed(...)`, so
  payload bytes are attributed to the **`BINARY`** type of the **binary_alloc** allocator in the
  per-type stats (`binary_alloc` is pinned to exactly `["BINARY","DRV_BINARY"]`). The public API is
  unchanged. Private `#[repr(C)] BinaryHeader { refc, len }` sits immediately before the payload
  (`data_ptr` = first payload byte; `data_ptr - HEADER_SIZE` recovers the base). `Send` but not `Sync`.

### `mseg` — the mmap-backed segment allocator (`erl_mseg.c`)
Facade + submodules `mseg/{mapping,header,config,state,allocator}.rs`.
- Constants: `MSEG_VERSION = "0.9"`, `MSEG_HEADER_SIZE = 64`, `MAX_CACHE_SIZE = 30`,
  `SUPER_ALIGN_BITS = 14`, `SUPER_ALIGNED_SIZE = 1<<14`; `page_size()`, `is_super_aligned(addr)`.
- `pub enum SegmentKind { SingleBlock, MultiBlock }` (`as_str` = `"sbc"`/`"mbc"`).
- `MsegConfig` + `MsegConfigBuilder` (`max_carriers`, `max_sbc_carriers`, `max_mbc_carriers`, `cache`;
  `mcs` clamped to `MAX_CACHE_SIZE`); `MsegCalls`; `MsegInfo`; `Segment`; `MsegAllocator`
  (mmap-backed, reuse cache, `+MM` limits/counters).

## Carrier / size-class machinery
All mutation is serialised through one `parking_lot::Mutex`. Carriers come from the OS (`mmap`) via
`mseg`, page-aligned. **Routing** (`CarrierAllocator::alloc_impl`):
- `size <= classes.large_threshold() && align <= UNIT_SIZE` → **`alloc_small`** (slab / MBC-class).
- `align > UNIT_SIZE || policy.uses_single_block_carrier(size)` (i.e. `size >= sbct`, default 512 KiB)
  → **`alloc_large`** (dedicated **SBC**, unmapped on free).
- otherwise → **`alloc_mid`** (shared variable-size **MBC** pool — split on demand, coalesce on free).
- **Slab.** Each multi-block carrier is dedicated to one class, subdivided into equal blocks (each
  preceded by a 2-word `BlockHdr`) threaded on that class's intrusive `FreeList`; a new carrier is
  mapped when the class list runs dry. Drains retain the carrier (documented retention policy).
- **MBC (`mbc.rs`).** `MbcCarrier` = variable-size carrier with **boundary tags** (legacy
  `SET_BLK_SZ_FTR`): `FLAG_PREV_FREE = 1<<7` + a trailing footer word holding the predecessor's size.
  `unsafe fn dealloc` coalesces previous-then-next; `split-on-alloc` keeps a minimum residue;
  `into_carrier()`, `is_fully_free()`, `free_bytes()`, `live_bytes()`, `allocated_blocks()`,
  `largest_free()`. `MBC_CLASS = 0`, `MIN_MBC_BLOCK_SIZE = 48` (MBC-local; slab `MIN_FREE_BLOCK_SIZE = 32`).
  Only fully-free MBC carriers are released. The pool scan is a linear first-fit over `AllocatorState::mbcs`.
- **`free_list.rs`** — per-class intrusive free lists with variable-size buckets (`insert`/`take_fit`/
  `remove`/`refile`/`largest_free`), good-fit depth `MAX_FIT_SEARCH = 3`, in-block links
  (`payload+0 = next, payload+8 = prev`). Carries a large in-module test module (the structure is only
  exercisable in-module).
- **`block.rs`** — the 2-word `BlockHdr` (`bhdr` packs flags/size/class/payload-offset; `carrier` word
  gives O(1) block→carrier recovery) and `BlockFlags` (`is_free`/`is_large`/`last_in_carrier`/`is_sbc`/
  `is_mbc`/`is_mseg`/`is_fixed_size`/`is_super_aligned`/`is_prev_free`). `ALL_FLAGS == 0b0111_1111`.
- **`strategy.rs`** — `Strategy { FirstFit, GoodFit, BestFit, AFirstFit }` (the `ErtsAlcStrat_t` surface;
  `legacy_value`/`legacy_name`/`option_name`/`from_legacy_value`/`from_option_name`/`default_for_allocator`/
  `supports_carrier_migration`). M1 implements the segregated first-fit family; the other variants
  currently select it too (plugin seam reserved).

## Performance (micro-benchmark `tests/hot_path.rs`)
`... test -p beam_alloc [--release] --test hot_path -- --nocapture` (release numbers recorded here):
- size-class lookup — `default_small_block` **3.58 ns/op** (lookup share 2.22), `default_legacy` **5.55 ns/op**
  (share 4.20).
- alloc+free hot path — 1 live × 64 B **871.14 ns/op**; 256 live × 64 B **616.16 ns/op**;
  256 live over 7 classes **3031.34 ns/op**.
- bulk mixed rounds 0/1/2 = **1062 / 4290 / 4283 ns/op** per alloc+free pair (65536 blocks, 256 sizes).
- The alloc+free numbers are **memory/syscall-bound, not CPU-bound** (release ≈ debug; the 1-class vs
  7-class spread tracks fresh-carrier page faults). The 1029→4311 bulk jump is legacy-faithful:
  `get_next_mbc_size` clamps fresh MBC sizes to `>= smbcs` (1 MiB), so a 6 KiB mid request legitimately
  gets a 1 MiB carrier. Do not chase these as "regressions".
- Steady state / no leak: `leak summary: 4 cycles x 28 blocks, steady state at peak_carrier_bytes=2555920
  across 7 carriers (458752 bytes)`.

## Constraints
- **Errors:** only `beam_common::{Result, BeamError}`. No crate-local error enum, no `panic!` on OOM
  (`AllocFailure{kind, requested}`: carriers ⇒ `Carrier`, process/slab heaps ⇒ `Heap`, large off-heap
  ⇒ `LargeObject`, else `Other`; invalid alignment/`Layout` ⇒ `BadArg`), no `unwrap()` on runtime data.
- **`unsafe` confinement:** allowed only in `block`, `carrier`, `free_list`, `allocator` (incl. its
  `core`/`state` submodules), `mbc`, `mseg`, `heap`, `binary`. Every `unsafe fn` has a `# Safety`
  section and every `unsafe` block a `// SAFETY:` rationale. `size_class`, `registry`, `stats`,
  `strategy`, `policy`, `refcount`, `error` are safe Rust. `unsafe_op_in_unsafe_fn` stays clean.
- **Carriers come from the OS via `mseg` (mmap)**, are page-aligned, and `munmap`'d on free/drop.
- **Dependencies (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `libc`, `parking_lot` — nothing
  else. Adding a dependency edge requires reporting to `../`.
- **`Heap` is fixed-capacity by contract** — never reallocate (workspace §2).
- Files target ~1000 lines; split by responsibility. Two files exceed it: `types.rs` (~727 lines) *is* the
  `erl_alloc.types` **data table** (its invariants are only reviewable with the whole table in one place),
  and `policy.rs` (~1370 lines, of which ~550 are its in-module test module) documents all 12 `+M<alloc><opt>`
  options against the C ground truth. The raw-memory
  modules keep their correctness-evidence tests in a sibling `tests` submodule
  (`free_list/tests.rs`, `stats/tests.rs`, `types/tests.rs`, `allocator/tests*.rs`, `mseg/tests.rs`).
- Licensing Apache-2.0.

## Design Decisions
- **Slab-per-size-class carrier framework + a real variable-size MBC pool.** Slab carriers are
  segregated (one class each, equal blocks, intrusive free list); sizes at/above `sbct` get a dedicated
  single-block carrier returned to the OS on free; the middle band shares coalescing variable-size MBC
  carriers (`mbc.rs`). This is a documented subset — the variable-block engine's pluggable
  `goodfit`/`bestfit`/`afit` strategies are still collapsed to first-fit (plugin seam reserved).
- **MBC interior layout = boundary tags**, not a side structure: pure address geometry, zero extra
  allocation, O(1) predecessor lookup; cost is one word per *free* block. `FLAG_PREV_FREE` sits outside
  `ALL_FLAGS`, so it cannot disturb the slab path.
- **`AllocRef = Arc<dyn Allocator>`, not a borrow** — dependents outlive the creating call and share
  across threads without a lifetime.
- **The MBC free path reads `header.size` *before* `MbcCarrier::dealloc`** (coalescing rewrites the
  header), so accounting owes the block's own size, never the merged extent; the MBC lookup is keyed by
  exact carrier base (never by `class`, since `MBC_CLASS == 0` is also a valid slab class index).
- **Over-aligned large blocks zero their alignment padding.** `dealloc_unsized` probes candidate
  alignments smallest-first; a reused (unzeroed) segment could otherwise resurrect a stale plausible
  header in the padding. Zero bytes never pass the header-plausibility check.
- **`AllocStats::mseg` is a live view of the process-global `mseg` counters**, not a per-allocator
  local copy (there is one segment allocator per process, as in `erl_mseg.c`).
- **One thread-safe allocator instance per `AllocType`**, one `parking_lot` mutex each; the legacy
  `ts`/`tspec`/`tpref`, per-scheduler instances and the delayed-dealloc queue are not in M1.
- **`beam_term::heap::Heap` is a test arena; this crate's `Heap`/`ReservedHeap` are production**
  (allocator-backed, `beam_common::Result`). The `beam_term` arena is not re-exported.
- **Reservation + in-place growth, not reallocation.** `ReservedHeap` takes one large reservation and
  raises its *committed* limit inside it, so growth never invalidates a handed-out pointer — which is
  exactly what a plain reallocating arena cannot promise, and why the fixed `Heap` could only ever be
  grown by replacement. Only exhausting the reservation itself needs a move.
- **`try_grow` executes the host's policy; it does not invent one.** It commits exactly the range asked
  for (no doubling heuristics) and answers `MoveRequired` — with **no** state change — instead of
  allocating or moving anything itself. The host decides between `relocate` and a
  growth-by-replacement collect.
- **`relocate` is a rigid constant-delta move, deliberately not pointer-aware.** It allocates the
  replacement *first* (so any failure leaves the heap and every pointer into it fully valid), copies
  the live words verbatim, adopts the new block (`committed_words == new_reserved_words`, `peak`
  preserved) and frees the old one. Fixing pointers needs term-structure knowledge this layer does not
  own (see the host-adoption contract), and the delta is computed with a wrap-free `wrapping_sub`.
- **`Heap` and `ReservedHeap` share one term-writing implementation** (`bump`), so the two arenas can
  never drift apart on term layout, header encoding or the exhaustion error; each supplies only its own
  bookkeeping.
- **The refcount lives here, not in `beam_gc`.** `RefCount` is the single `erts_refc_*` primitive; the
  crate-layer contract forbids duplicating a type, and `beam_alloc` is the LCA.

## Test Strategy
- In-module `#[cfg(test)] mod tests` for the raw-memory internals (`block`, `free_list`, `mbc`,
  `mseg`, `carrier`, `allocator::{core,state}`, `stats`) — these use heap memory and are only
  exercisable in-module. Cross-module behaviour is tested through the **public** API.
- Integration: `tests/allocator.rs` (alloc/dealloc/realloc round trips, alignment, dedicated-carrier
  path, stats deltas, `max_footprint` cap, typed/unsized entry points, `allocator_info`), `tests/heap.rs`
  (`cons`/`tuple`/`alloc_words`/`alloc_tuple`/`reset`/peak, pointer stability, `Drop`),
  `tests/reserved_heap.rs` (in-place growth across the committed limit with a byte-identical `hp` and no
  extra allocation, reservation exhaustion ⇒ `MoveRequired` with no state change and still-usable old
  limit, the host move path with its constant delta and byte-identical live region, zero/small
  reservation and hostile-input edges, `Drop` accounting incl. the real mseg-backed allocator),
  `tests/hot_path.rs` (the micro-benchmark + a steady-state/no-leak test).
- A **leak/steady-state** test drives alloc/free cycles and asserts the carrier/free-block counters
  return to baseline (no leak on the tested paths).
- Gate (GREEN required, current): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml
  -p beam_alloc --no-fail-fast` → **237 lib + 18 (`tests/allocator.rs`) + 13 (`tests/heap.rs`) + 4
  (`tests/hot_path.rs`) + 12 (`tests/reserved_heap.rs`) = 284 tests, 0 failures**;
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace --all-targets` is
  warning-free; and `... clippy -p beam_alloc --all-targets` reports nothing for this crate.
- Cross-checks against the real system ERTS belong to `../tools/differential/` (the compatibility gate).

## Known Gaps (intentionally NOT covered in Milestone-1)
- Thread-specific / thread-preferred allocators (`tspec`/`tpref`), per-scheduler instances, and the
  delayed-dealloc queue + thread-progress barrier for cross-scheduler frees.
- The variable-block engine's **strategy plugins** (`goodfit`/`bestfit`/`afit`) — `AllocatorConfig::strategy`
  is stored and reported but **never consulted by the alloc/free routing** (`core.rs`), so every `Strategy`
  variant behaves identically (the plugin seam is reserved). The legacy `mbcgs` carrier-pool **size-growth
  ladder** is not modelled (the MBC pool scan is a linear first-fit).
- The per-allocator policy presets in `policy.rs` (`TEMP_ALLOC`/`SL_ALLOC`/`STD_ALLOC`/…) are reference
  data: `registry.rs` builds every allocator from the one default `AllocatorConfig`, so those presets are
  not installed and the policy is the same default for all 12 allocators.
- `free_list::FreeList::refile` is dead (carries `#[allow(dead_code)]`): MBC coalescing (`mbc.rs::dealloc`)
  unlinks neighbours with `remove` and re-files with `insert`, so `refile` is exercised only by its own tests.
- Carrier pools / carrier migration between allocator instances; `madvise`/carrier shrinking.
- `fix` (fixed-size type) allocators and `erts_alloc_permanent_aligned` cache-line-aligned permanents.
- Runtime option parsing (`+MMscs`, `+Mea`, …) — `AllocatorConfig`/`CarrierPolicy` expose the knobs,
  but the A.6 CLI surface is owned by `beam_rt`.
- Allocation alignment is capped at the OS page size; super-aligned mappings are not carried through
  end-to-end (the `mseg` super-align constant/`is_super_aligned` predicate exist, the caller does not
  request them yet). The packed `bhdr` word bounds the largest encodable block (`block::MAX_BLOCK_SIZE`).
- `erts_alloc`/`enif_alloc` **C ABI shims** are out of scope (owned by `beam_abi`); bit-level
  `SubBits`/`Bitstring` and the on-heap `BinRef` layout are out of scope (owned by `beam_term`).

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/allocator.rs` → facade: `WORD_ALIGN`/`MIN_ALIGN`, `AllocRef`, module wiring, re-exports
- `src/allocator/trait.rs` → the `Allocator` trait (incl. the typed/unsized/`allocator_info` methods)
- `src/allocator/config.rs` → `AllocatorConfig` + `AllocatorConfigBuilder`
- `src/allocator/state.rs` → `AllocatorState` (carriers, MBC pool, free lists, counters) + `Drop`
- `src/allocator/core.rs` → `CarrierAllocator`: routing (`alloc_small`/`alloc_mid`/`alloc_large`),
  `locate`/`plausible`, `realloc_impl`, `mbc_size`, `impl Allocator`
- `src/allocator/tests*.rs` → the allocator's in-module test module (basic/typed/unsized_free/info)
- `src/types.rs` → `erl_alloc.types` inventory: `AllocatorId`/`ClassId`/`TypeId`/`TypeFlags`/
  `ALLOC_TYPES`/`TYPE_COUNT`/`EXPECTED_TYPE_COUNT`, `type_by_name`/`types_for_allocator`
- `src/registry.rs` → `AllocType` + lazily-initialised process-wide allocators; `alloc_util_allocators`
- `src/size_class.rs` → `SizeClasses` (sbc/mbc ladders + thresholds)
- `src/policy.rs` → `CarrierPolicy`/`PolicyOptionKind` (the 12 `+M<alloc><opt>` options)
- `src/stats.rs` → `AllocStats`/`TypeStats`/`MsegStats`/`AllocatorInfo`/`InfoValue`/`STATS_TYPE_SLOTS`
- `src/heap.rs` → the fixed-capacity per-process bump `Heap` (`alloc_words`/`cons`/`tuple`/`reset`)
- `src/reserved_heap.rs` → `ReservedHeap`/`GrowOutcome`: the reservation-capable per-process term heap (in-place growth, `relocate`/delta) + the host-adoption contract
- `src/bump.rs` → `pub(crate)` bump-pointer term-construction primitives shared by `Heap` and `ReservedHeap`
- `src/refcount.rs` → `RefCount`: the shared atomic reference count
- `src/binary.rs` → `RefcBinary`: off-heap refcounted large binaries (attributed to the `BINARY` type)
- `src/block.rs` → block layout constants + `BlockHdr`/`BlockFlags` (implementation module)
- `src/carrier.rs` → `Carrier` (mseg-backed block, subdivision into equal class blocks) + the
  process-global `mseg()` handle
- `src/free_list.rs` → per-class intrusive free lists + variable-size buckets (implementation module)
- `src/mbc.rs` → `MbcCarrier`: variable-size coalescing carrier with boundary tags (implementation module)
- `src/strategy.rs` → `Strategy` selector (implementation module; A.6 config fidelity)
- `src/mseg.rs` → facade for the mmap segment allocator; `src/mseg/{mapping,header,config,state,allocator}.rs`
- `src/error.rs` → `pub(crate)` allocation-failure constructors (no new error enum)
- `tests/` → crate-level integration tests (`allocator.rs`, `heap.rs`, `reserved_heap.rs`, `hot_path.rs`)
- `../../erts/emulator/beam/erl_alloc_util.c` → legacy carrier framework being mirrored (read-only)
- `../../erts/emulator/beam/erl_alloc.types` → the type/class/allocator inventory source (read-only)
- `../../erts/emulator/beam/erl_mseg.c` → legacy segment allocator being mirrored (read-only)

## Notes for Agents
- **The build command needs the manifest path.** `erts_rust/scripts/cargo.sh build -p beam_alloc` fails
  (no `Cargo.toml` at the repo root); use
  `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml -p beam_alloc`
  (workspace: `... --manifest-path erts_rust/Cargo.toml --workspace`). Sandbox `$HOME` is read-only, so
  always go through the wrapper. Never `cd`; never touch `../tools/`.
- Read `../../erts/emulator/beam/erl_alloc_util.h` for the `Block_t`/`Carrier_t`/`Allctr_t` layout and
  `erl_alloc_util.c` for the alloc/free/realloc flow before changing block or carrier code.
- `AllocKind::Heap` from `Heap::alloc_words` means "collect", not "fatal OOM" (see Design Decisions).
- Interior layouts (carrier size, class spacing, retention policy, the `RefcBinary` off-heap header)
  are **not** externally pinned; only the seam in `allocator`/`heap`/`reserved_heap`/`size_class`/
  `registry`/`stats`/`types`/`refcount`/`binary`/`policy` is frozen.
- **Hostile-input arithmetic is saturating/checked by contract.** `error::heap_exhausted` saturates the
  word→byte conversion (an `unsafe fn`'s word count is unbounded by its safety contract) and
  `alloc_small` saturates `BLOCK_HEADER_SIZE + payload_capacity` (a hostile `SizeClasses` table is a
  legal public input), so both yield a typed error instead of a debug panic or a silent wrap;
  `SizeClasses` itself cannot overflow on any input. Keep that property when touching those paths.
- **The flag byte is fully accounted for.** `ALL_FLAGS` (bits 0–6) plus the `prev_free` state bit
  (bit 7) cover all 8 bits — compile-time asserted in `block.rs` — so `plausible_header` performs no
  unknown-bit rejection; `BlockFlags::has_unknown_bits` is the predicate for the taxonomy union alone
  (masking against `ALL_FLAGS` alone would reject a legitimately live MBC block and leak it).
- `... clippy -p beam_alloc --all-targets` reports nothing for this crate; keep it that way.

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = wave 0).
- `../../CONTEXT.md` → workspace contracts (§2 frozen term layout, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D (memory management) and §H Stage 0/3.
- `../beam_term/CONTEXT.md` → the frozen term representation this heap stores.
- `../beam_gc/`, `../beam_message/`, `../beam_ets/`, `../beam_process/` → primary dependents.

## Handoffs (owned by other nodes — NOT adopted here)
- **`beam_rt` `system_info` wiring (partially adopted):** `beam_rt`'s `info.rs`/`info/memory.rs`
  already answer `system_info(alloc_util_allocators)` and `system_info({allocator,A})` from
  `registry::alloc_util_allocators()` + `AllocatorInfo::from_stats(...)` — the `{allocator,A}` rows
  are flat (the oracle wraps them in one `{instance,N,…}` entry per allocator instance, a documented
  divergence in `beam_rt`'s `info/CONTEXT.md`). `AllocatorInfo::push_policy(...)`/`push_type(...)`
  and `erts_alloc:info/0` are **not** wired yet.
- **`../../CHANGE.md` entries owed to the repo-root owner:** the type-table inventory, the mseg layer,
  the typed allocator surface, the stats layer, the MBC/coalescing wiring, and the `policy.rs` thresholds.
- **`beam_term` (reported read-only, not fixable from here):** the crate-layer audit of
  `size_object`/`size_class` found no overflow path (`size_object` accumulates with `saturating_add`
  and its real failure modes — `TermGraphCyclic`, `TermGraphTooLarge` — already surface as typed
  `TermError`s). One non-arithmetic caveat was reported for `../beam_term`: for a *forged* header with
  a huge arity field, `describe_object` builds `Children::Slots { count: arity }` and
  `push_children` reads `*base.add(i)` for `i in 0..count`, guarded only by the documented "live word"
  caller invariant (an OOB read/hang, not a wrap) — the `beam_term` owner may want an arity
  plausibility bound.
