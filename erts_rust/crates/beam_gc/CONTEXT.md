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
option and is **out of scope** (see Known Gaps).

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

### `policy.rs` — heap-size policy (the `erts_next_heap_size` analogue)
- `pub struct HeapPolicy` — `min_heap_words`, `max_heap_words` (0 = unbounded), `fullsweep_after`
  (default 65535), `initial_young_words` (233), `initial_mature_words`, `need_reserve_words`;
  `default()`, `from_min_max(min, max)`, builder.
  - `next_words(size, offset) -> usize`; `size_after_fullsweep(live_words, need_words) -> usize`;
    `should_grow(...)`, `should_shrink(...)`.
- `pub fn heap_sizes() -> &'static [usize]` — the curated growth table (Fibonacci+1 to index 23, then
  +20%, capped at `2^53 / word`), `LazyLock`-built from `erts_init_gc` (`erl_gc.c:240-303`).
- `pub fn next_heap_size(size, offset) -> usize` — `erts_next_heap_size` over that table.

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
- `pub struct OffHeap` — the per-process list (`MSO(p)`): `push(impl Into<OffHeapEntry>)`, `len()`,
  `is_empty()`, `overhead_bytes()` (the summed off-heap payload bytes, not the `Vec` capacity), and
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
- **In-place Cheney forwarding, exactly as C.** A boxed object's **header word is overwritten** with
  the new BOXED-tagged pointer (`IS_MOVED_BOXED = !is_header`, `erl_gc.h:36`); a cons cell's car is set
  to `THE_NON_VALUE` and its cdr to the new LIST pointer (`IS_MOVED_CONS`, `erl_gc.h:37,42-55`). The
  scan phase mirrors the C `sweep()` switch verbatim: walking the to-space word by word, a `BOXED`/`LIST`
  word is a *reference* to forward, a **transparent** `HEADER` (arityval/record) is stepped over so its
  elements are scanned individually, any other **thing** header is skipped by `thing_arityval+1`
  (opaque interior; fun `env[]` follows as separately-scanned words; map keys/values follow the size
  word the same way), and `HEADER_SUB_BITS` forwards its `orig` field (`erl_gc.c:2328-2348`).
- **`xptr_tag` is not needed.** The C ORs the literal bit back in only for literal-area copies
  (`copy.c:2109`); a GC collection copies only in-heap objects, so a copied pointer never carries the
  literal bit. Literal-tagged (and every other out-of-from-space) reference fails the `FromSet.contains`
  test and is left verbatim — the C `erts_is_literal`/area-test rule (`erl_gc.c:2013,2292-2297`).
- **Local object-size traversal; `beam_term` is not modified.** `copy.rs` computes only what the copier
  needs — an *incremental, per-object* `move_size(header, ptr)`: the `nelts` of the C `move_boxed`
  (`header_arity` plus the flatmap/HAMT/fun extras), read through `beam_term`'s public accessors
  (`header_arity`, `is_map_header`, `is_flatmap_header`, `map_header_val`, `is_fun_header`, …), which
  the scan then walks word by word. It does **not** call `beam_term::size::size_object`: that is a
  whole-graph size (it cannot size the single object the copier is looking at), and it under-reports
  HAMT maps (see Known Gaps). The decision and the exact formula live here so no agent re-derives them.
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

## Constraints
- **Errors:** `beam_common::{Result, BeamError}` only. Heap exhaustion while copying a term is
  `BeamError::Alloc(AllocFailure::new(AllocKind::Heap, bytes))` (never panic, never a crate-local error
  enum, never `unwrap()` on runtime data). `beam_term::TermError` is reconciled to `BeamError` upstream,
  so `?` crosses the boundary.
- **`unsafe` in production code is confined to `copy.rs`** (the word-level copier) and the two narrow
  `NonNull`-deref / `Box::from_raw` blocks in `mso.rs`; every `unsafe fn` carries a `# Safety` section
  and every `unsafe` block a preceding `// SAFETY:` rationale (`beam_common::safety`).
  `unsafe_op_in_unsafe_fn` stays clean. `space.rs`, `policy.rs`, `stats.rs` and the `Collector` driving
  logic are safe Rust (only their `#[cfg(test)]` sections call `unsafe alloc_words`).
- **Do NOT modify `beam_term` or `beam_alloc`** — build on their public APIs only. Do not touch
  `../../../erts/` (read-only ground truth) or `../../tools/`.
- **Never break the frozen term layout or the barrier-free invariant** (a minor must not leave a
  mature→young pointer).
- Files target ~1000 lines; split by responsibility. (No file here is expected to exceed it.)
- Licensing Apache-2.0; deps `beam_common`, `beam_term`, `beam_alloc`, `libc` only.

## Test Strategy
- In-module `#[cfg(test)] mod tests` per module: `space` (geometry/range tests/`reset`/`from_heap`
  round-trip), `copy` (forward of immediates/cons/boxed/nested tuples/lists; forwarding markers;
  out-of-from-space left verbatim), `policy` (table shape: 12, 38, … 233; `next_heap_size` offset 0/1;
  growth/shrink decisions), `mso` (refcount `inc`/`dec`/`dec_and_test`; `MsoHandle` clone/drop runs the
  destructor exactly once; `OffHeap::sweep` releases dead handles).
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
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_gc`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
- Cross-checks against the real system ERTS belong to `../tools/differential/`, not here.

## Known Gaps (intentionally NOT covered in Milestone-1)
- **GenImmix / a mark-region mature generation** — measurement-gated per Part II §D; explicitly out of
  scope. The `mature` generation is a plain copying space.
- **The C `high_water` single-block layout, per-object tenuring (`GEN_GCS`/`MAX_GEN_GCS`) and the
  `F_NEED_FULLSWEEP` flag protocol** — replaced by the simpler two-space young/mature scheme above.
- **Dirty-scheduler escape for long collections** (`ERTS_POTENTIALLY_LONG_GC_HSIZE`, `erl_gc.h:26`).
- **Frame-aware stack relocation** (`copy_erlang_stack`), the process dictionary, on-heap message/signal
  queues and saved BIF/NIF arguments as sub-rootsets — owned by `beam_process`; this crate takes a flat
  root slice.
- **Heap-fragment (`ErlHeapFragment`) copy-in and the `heap_frags` rootset** (`erts_copy_one_frag`).
- **Literal-area collection** (`erts_garbage_collect_literals`, literal-area purge/`xptr_tag`) — literals
  are treated as permanent and left verbatim.
- **A.6 runtime heap options** (`+hms`, `+hmbs`, …) — this crate exposes `HeapPolicy`, not the CLI
  surface (owned by `beam_rt`).
- **`OffHeap` automatic integration with the collector's reachability** — the sweep is exposed as
  `OffHeap::sweep(is_live)`; the caller supplies the liveness test (the on-heap handle kind is the
  binary/ABI layer's concern).
- **Sizing oracle gap (upstream, `beam_term::size::size_object`).** It gates its hashmap branch on
  `is_hashmap_header`, which only matches the two *head* node types, so an inner `HAMT_NODE_BITMAP`
  node (and everything under it) is not counted — `size_object` of a 500-key map reports `4`. It also
  reports a refc binary's `ErlSubBits` alone (5) rather than the C's `ERL_REFC_BITS_SIZE` (8). The real
  C `size_object_x` (`copy.c:192-214,219-231`) handles all three node types and the BinRef case. This
  crate's copier is unaffected (`move_size` uses `map_header_val`/`header_arity` and is exact — see
  `tests/map_copy.rs`), which is why the tests measure a map's footprint directly rather than via
  `size_object`. Fixing `size_object` is `beam_term`'s job.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/space.rs` → `HeapSpace` (copying semi-space over `beam_alloc::Heap`) + `FromSet`
- `src/copy.rs` → the Cheney copy engine: `Copier`, `forward_roots`/`complete`, `move_size`/`move_*`, `collect`
- `src/collector.rs` → two-generation `Collector`, `Collection`/`CollectionKind`, minor vs major driver
- `src/policy.rs` → `HeapPolicy`, the curated `heap_sizes` table, `next_heap_size`
- `src/mso.rs` → `RefCount`, `Mso`, `MsoHandle`, `OffHeap` (off-heap refcounted large objects)
- `src/stats.rs` → `GcStats`
- `src/error.rs` → `pub(crate)` GC allocation-failure constructors
- `tests/copy_correctness.rs` → the copy-correctness evidence (structure preserved, roots updated, garbage dropped)
- `tests/generations.rs` → `Collector` minor/major/tenure/escalation integration
- `tests/map_copy.rs` → flat-map + HAMT-map (a 500-key deep trie, incl. a hand-built collision node) survive a collection; the copied map is sized exactly to its independently-measured footprint
- `tests/refc_binary_copy.rs` → the 8-word `[BinRef][ErlSubBits]` refc-binary pair survives a collection (the `orig` back-pointer is forwarded to the copied `BinRef`, off-heap pointer left verbatim)
- `../../../erts/emulator/beam/erl_gc.c` → the collector being mirrored (`move_boxed`/`move_cons`/`sweep`/`do_minor`/`full_sweep_heaps`) (read-only)
- `../../../erts/emulator/beam/copy.c` → term traversal reference (`size_object_x`/`copy_struct_x`) (read-only)
- `../../erts/emulator/beam/erl_gc.h` → forwarding macros (`IS_MOVED_*`, `follow_moved`, `ErtsInYoungGen`) (read-only)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types and the wave-1 dependency position.
- `../../CONTEXT.md` → workspace contracts (§5 GC/memory model, §3 error strategy, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §D ("Memory management in Rust") and §H (Stage 4).
- `../beam_term/CONTEXT.md`, `../beam_alloc/CONTEXT.md` → foundations this crate builds on (do not modify).
- `../beam_process/CONTEXT.md` → the primary consumer that drives collections at safepoints.
