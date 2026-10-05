# `beam_message` — Copy Engine (`copy_struct`) & Messaging

## Intent
Owns the **copy half of the process memory model**: moving a term graph between two process heaps
(`copy_struct`) and the data-level messaging primitives built on it — the `Message` envelope, the
per-process FIFO message **queue**, and `send`/`receive`. Implements Part II §C/§D (the
messaging/ETS copy boundary: *immutability bounds what may be shared; everything else is copied*) and
the Stage-3 row of Part II §H.
Sibling responsibilities (NOT here): the **collector** and the per-process heap (`beam_gc`,
`beam_alloc`); the **scheduler / selective-receive waiting / save queue / signal ordering**
(`beam_process`); the `send`/`receive` **BIF surface and pattern matcher** (`beam_bifs`); off-heap
binary *payloads* and the on-heap binary handle kind (`beam_gc`/`beam_abi`, still unbuilt).
Ground truth = the legacy C `erts/emulator/beam/copy.c` (`copy_struct_x` — the authoritative
iterative copier; `size_object_x`; `erts_move_multi_frags`), `erl_message.c`/`erl_message.h`
(the `ErtsMessage` envelope, `ErlHeapFragment`, the message queue) and the `OffHeap`/MSO rule of
`erl_gc.c`. Read the C; never modify it.

## Inheritance (binding)
Obeys the crate-layer `../CONTEXT.md` contracts verbatim: frozen term layout (§2 — construct/inspect
**only** through `beam_term`), return-based errors (§3 — `beam_common::Result<T>`, no panics, no
crate-local error enum), `unsafe` confinement (marked modules, `# Safety`/`// SAFETY:`),
`unsafe_op_in_unsafe_fn` clean, ~1000-line files, licensing Apache-2.0. **Do not restate them here.**

## API Surface (the seam dependents design against)
Re-exported at the crate root and via `beam_message::prelude::*`.

### `copy.rs` — the copy engine (the `unsafe` core)
- `pub struct CopyContext<'a>` — the copy boundary: the source process's off-heap table (to resolve
  off-heap references) and the destination's (where referenced off-heap objects are re-registered).
  - `CopyContext::new(src: &'a OffHeapTable, dst: &'a mut OffHeapTable) -> CopyContext<'a>`
  - `src_off_heap(&self) -> &OffHeapTable`; `dst_off_heap(&self) -> &OffHeapTable`
- `pub fn copy_struct(term: Eterm, to: &mut beam_alloc::Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm>`
  — copy `term`'s graph into `to`, returning the copied root. **Iterative** (an explicit worklist —
  deep lists never recurse on the native stack). Immediates and literal-area terms are **shared**
  (returned unchanged); cons cells, tuples/records, funs, flatmaps and off-heap containers are
  copied. Off-heap references are **refcounted**, never deep-copied.
- `pub fn copy_struct_simple(term: Eterm, to: &mut beam_alloc::Heap) -> Result<Eterm>` — the common
  case with empty off-heap tables (graphs that contain no off-heap references).
- `pub fn size_of_copy(term: Eterm) -> usize` — thin forwarder to `beam_term::size_object` (the total
  flat word count `copy_struct` preflights against `Heap::available`). Exposed so callers/tests can
  size a destination heap without re-deriving it.

### `message.rs` — the envelope and the queue
- `pub struct Message` — the copied payload plus sender identity and the off-heap objects it owns.
  - `Message::new(payload: Eterm, from: Eterm) -> Message` (token = `NIL`, no off-heap objects)
  - builder: `with_token(Eterm)`, `with_off_heap(Vec<MsoHandle>)`
  - accessors: `payload() -> Eterm`, `from() -> Eterm`, `token() -> Eterm`,
    `off_heap() -> &[MsoHandle]`
  - mirrors C `ERL_MESSAGE_TERM`/`_TOKEN`/`_FROM` (`m[0]`/`m[1]`/`m[2]`) + the message's `off_heap`.
- `pub struct Queue` — the per-process FIFO message queue (a pure data structure; **no** scheduler
  coupling). `new()`/`Default`, `enqueue(Message)`, `dequeue() -> Option<Message>`,
  `peek() -> Option<&Message>`, `len()`, `is_empty()`, `clear()`, `iter() -> impl Iterator<Item=&Message>`.

### `mailbox.rs` — send / receive primitives (data level)
- `pub fn send(from: Eterm, term: Eterm, to_heap: &mut Heap, to_queue: &mut Queue, ctx: &mut CopyContext<'_>) -> Result<()>`
  — copy `term` into `to_heap`, collect the off-heap objects it references, build the `Message` and
  enqueue it on `to_queue`. This is the copy-into-the-receiver's-heap half of the C
  `erts_queue_message`/`copy_struct` path; the scheduler signalling half is `beam_process`.
- `pub fn receive<F: FnMut(&Message) -> bool>(queue: &mut Queue, pred: F) -> Option<Message>` —
  **remove and return** the first queued message whose payload satisfies `pred` (selective receive
  skeleton). Waiting, `after` timeouts, the save queue and signal ordering live in `beam_process`.
- `pub fn receive_ref<F: FnMut(&Message) -> bool>(queue: &Queue, pred: F) -> Option<&Message>` —
  non-destructive variant.

### `off_heap.rs` — the off-heap (MSO) reference table
- `pub use beam_gc::MsoHandle;` — the off-heap object handle is **beam_gc's** (MSO object + atomic
  `RefCount`); this crate never re-implements MSO.
- `pub struct OffHeapTable` — a **slot-indexed** table of `MsoHandle`s (slot = the on-heap
  off-heap-reference's `val` word). `new()`/`Default`, `push(MsoHandle) -> usize` (returns the slot),
  `get(slot) -> Option<&MsoHandle>`, `clone_slot(slot) -> Option<MsoHandle>` (**bumps the refcount**),
  `len()`, `is_empty()`, `iter()`, `overhead_bytes()`, `into_handles() -> Vec<MsoHandle>`.

## Constraints
- **Errors: `beam_common::{Result, BeamError}` only.** Heap exhaustion mid-copy ⇒
  `BeamError::Alloc(AllocFailure::new(AllocKind::Message, bytes))` — **never panic**, never a
  crate-local error enum, never `unwrap()` on runtime data. A malformed/unsupported term layout or an
  out-of-range off-heap slot ⇒ `BeamError::BadArg`.
- **`unsafe` is confined to `copy.rs`** (raw word read/write in the copy loop) and the narrow
  `layout.rs` accessors; every `unsafe fn` carries a `# Safety` section and every `unsafe` block a
  preceding `// SAFETY:` rationale. `off_heap.rs`, `message.rs`, `mailbox.rs`, `error.rs` are safe Rust.
  `unsafe_op_in_unsafe_fn` stays clean.
- **Never assume a different `Eterm` layout** — read headers/words through `beam_term` accessors.
  The frozen word layouts (cons `[CAR,CDR]`, header words, `BinRef [thing_word,val,next]`) are the
  contract; interiors that are not externally pinned (hashmap HAMT shape) are documented approximations.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `beam_codec`, `beam_gc`,
  `libc`. The `beam_gc` edge is **new** (see Design Decisions) and must be reported to `../`.
- Files target ~1000 lines; one responsibility per file. Licensing Apache-2.0.

## Design Decisions
- **Fidelity scope: a correct, iterative *tree* copy, not the C's single-allocation `htop`/`hbot`
  layout.** The C pre-sizes (`size_object`), then bumps lists up from the heap base and boxed things
  down from the top so outer objects precede inner ones for `copy_ets_element`. This crate allocates
  each object *incrementally* from the destination heap (parent before child, so the outer-before-inner
  ordering invariant still holds) with an **explicit worklist** for fix-ups — no native recursion, so a
  `1e5+`-element list cannot overflow the stack. `size_of_copy` (= `size_object`) is still used as an
  up-front capacity preflight. This is the same incremental-copy posture as `beam_gc::copy`.
- **Sharing policy — copy trees, share exactly what the C shares.** C `copy_struct` is a *tree*
  copier: a subterm occurring twice is copied twice (sharing-preserving copy is a *separate* C
  mechanism, and is out of M1 scope). The two things it does **share** are reproduced: (1)
  **immediates** (`is_immediate`) are returned as-is; (2) **literal-area terms** (a boxed/list word
  with the `TAG_LITERAL_PTR` bit, tested *only* on compound words as C's `erts_is_literal` is) are
  returned as-is — the literal bit means the term is permanent and heap-independent. The zero-arity
  tuple (`HEADER_ARITYVAL` with arity 0) is the shared `beam_term::tuple0()` literal and is returned
  as-is.
- **`beam_gc` is a new dependency; MSO is reused, not reimplemented.** The off-heap object, its atomic
  `RefCount` and its owning `MsoHandle` are **entirely `beam_gc::mso`'s**. `beam_gc::OffHeap` is
  *push-only* (no read access), but the copy boundary must **read** the source process's off-heap
  objects to re-register (refcount) them — so `off_heap.rs` adds a minimal **slot-indexed**
  `OffHeapTable` over `MsoHandle`. It stores no new object model and no new refcounting: it only adds
  the index association the copier needs. **Dependency edge `beam_message → beam_gc` is new**
  (acyclic: `beam_gc` depends only on the wave-0/1 foundations and never on `beam_message`) and is
  reported to `../` for the routing table; `Cargo.toml` is updated here.
- **Off-heap reference term — the M1 encoding (documented substitution).** An off-heap reference is
  the frozen `BinRef` container thing `[HEADER_BIN_REF, val, next]` (`ERL_BIN_REF_SIZE = 3`). There is
  no on-heap binary handle type yet (binaries are a documented upstream gap — `beam_codec` returns
  `unsupported`), so **`val` holds the MSO slot index** into the owning process's `OffHeapTable` and
  **`next` is unused (`0`)** — the analogues of C's off-heap object pointer and `off_heap->next` link.
  Copying such a container resolves the source slot, **clones the handle** (refcount++), pushes it
  into the destination table and writes the **new** slot; the payload is never deep-copied. This
  mirrors `beam_codec::term`'s documented `node`/`creation` slot substitution. When a real on-heap
  binary handle lands (`beam_gc`/`beam_abi`), only this encoding changes.
- **The destination off-heap table *is* the message's off-heap list.** `send` allocates a fresh
  `OffHeapTable`, passes it as `ctx`'s destination, and stores its handles (`into_handles()`) as the
  `Message`'s `off_heap` — mirroring how C links copied BinRefs into the `ErlHeapFragment.off_heap`
  so the receiving process's GC can sweep them.
- **`size_object` is used, not re-derived.** `beam_term::size_object` (`size_object_x` port) gives the
  total word count for the capacity preflight; a tiny local `layout::object_words` gives the
  *single-object* word count the incremental allocator needs — the same split as `beam_gc::copy`'s
  `move_size` (a single-object size is a different function from the graph total). Both are documented
  against `copy.c`'s `move_boxed`/`size_object_x`.

## Test Strategy
- In-module `#[cfg(test)] mod tests` per module: `layout` (per-object word counts + child-slot
  enumeration for tuple/record/fun/flatmap/off-heap container), `off_heap` (slot push/get/clone bumps
  the refcount; `into_handles`), `message` (builder + accessors; `Queue` FIFO/`peek`/`len`/`clear`),
  `copy` (immediates shared; cons/tuple/record/nested copied; literal shared; off-heap container
  refcounted; exhaustion ⇒ `AllocKind::Message`).
- **`tests/copy.rs` — the required copy correctness evidence:**
  1. **Structural equality** — build a nested tuple/list/boxed graph in a source `Heap`, copy into a
     destination `Heap`, and assert `beam_term::compare::eq(original, copy, &atoms)`.
  2. **Deep list, no stack overflow** — copy a list of `100_000+` elements (built iteratively) and
     assert the copy is `eq` to the original and count-correct.
  3. **Literal sharing** — a `TAG_LITERAL_PTR`-tagged compound term is returned by identity (not
     copied) — the destination heap is untouched for it.
  4. **MSO refcount** — a term referencing an off-heap object: after `copy_struct`, the destination
     table holds a clone (refcount == 2, same payload via `downcast_ref`), the copied container's slot
     is rewritten, and the payload was **not** deep-copied.
  5. **Heap exhaustion** — copying a graph larger than the destination heap returns
     `BeamError::Alloc(AllocKind::Message)` and never panics.
- **`tests/queue.rs`** — `send` copies into the receiver heap + enqueues; FIFO order; `receive` selects
  (and removes) the first matching message and leaves non-matching ones queued; `receive_ref` borrows.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never touch `../../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness, not here.

## Known Issues / Intentional M1 gaps
- **No sharing-preserving copy.** A subterm that appears twice is copied twice (C's default
  `copy_struct` behaviour); the separate C sharing-preserving copier is out of scope. Only immediates,
  literal-area terms and the zero-arity tuple are shared.
- **Binaries / bitstrings are not first-class.** There is no on-heap binary handle and
  `beam_codec` cannot decode binaries, so a `HEAP_BITS`/`SUB_BITS` boxed thing is copied **verbatim**
  (fixed-size thing words); its interior has no navigable child on-heap terms in M1, and a `SUB_BITS`
  whose `orig` points at a source-heap object is copied as-is (a documented gap, unreachable until a
  binary constructor exists).
- **Hashmaps (HAMT) are approximated.** A non-flatmap `MAP_SUBTAG` head is copied by its head-node
  word count only (`thing_arityval + 1`), matching `beam_term::size_object`'s documented approximation;
  deep HAMT nodes are not walked. Flatmaps (`≤ MAP_SMALL_MAP_LIMIT`) are copied correctly (keys tuple +
  values). Maps are not constructible in M1 anyway.
- **External pid/port/ref are copied verbatim** (their words are raw/immediate; the node field is an
  atom). C links them into `off_heap` via a node registry that does not exist yet.
- **Fragmented messages (`ErlHeapFragment`, `erts_move_multi_frags`) are a documented follow-up.**
  Large messages that do not fit a process heap fragment stream across buffers in C; M1 assumes the
  destination heap can hold the copy (exhaustion ⇒ `AllocKind::Message`). `Fragments`/`HeapFragment`
  land when `beam_process`'s heap management and the binary layer exist.
- **`beam_gc::OffHeap` read access is missing.** The copy boundary needs to *read* the source off-heap
  objects; `beam_gc::OffHeap` is push-only. `off_heap.rs` adds the indexed `OffHeapTable` locally —
  **escalated to `../`** so a future `beam_gc::OffHeap::get`/`iter` can absorb it (Prevent-Duplication).
- **Scheduler coupling is absent by design.** `Queue` is a pure FIFO; selective-receive waiting,
  `after`, the save queue, signal/priority ordering and the message-queue length semantics of
  `ErtsSignalPrivQueues` are `beam_process`'s.
- **`Message.token` is a plain field (`NIL` when absent).** The seq-trace / distribution-token
  machinery that fills it is `beam_dist`/`beam_rt`.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/copy.rs` → the iterative `copy_struct` engine (the `unsafe` core) + `CopyContext` + `size_of_copy`
- `src/layout.rs` → per-object word counts + child-slot enumeration + the off-heap container encoding
- `src/off_heap.rs` → `OffHeapTable` (slot-indexed `beam_gc::MsoHandle` table); MSO handle re-export
- `src/message.rs` → `Message` envelope + `Queue` FIFO
- `src/mailbox.rs` → `send` / `receive` / `receive_ref` data-level primitives
- `src/error.rs` → `pub(crate)` copy-failure constructors (no new error type)
- `tests/copy.rs` → copy correctness: nested-graph equality, deep list, literal sharing, MSO refcount, exhaustion
- `tests/queue.rs` → `send`/`receive` + FIFO queue integration
- `../../erts/emulator/beam/copy.c` → the authoritative copier (`copy_struct_x`/`size_object_x`/`erts_move_multi_frags`) (read-only)
- `../../erts/emulator/beam/erl_message.h` / `erl_message.c` → the message envelope, `ErlHeapFragment`, queue layout (read-only)
- `../../erts/emulator/beam/erl_gc.c` → the MSO/`OffHeap` sweep rule (`sweep_off_heap`) (read-only)
- `../beam_gc/` → the collector + `MsoHandle`/`RefCount`/`OffHeap` this crate depends on (sibling — read-only, escalate writes to `..`)

## See Also
- `../CONTEXT.md` → crate-layer contracts, seam types, dependency waves (this crate = the
  `beam_loader`-adjacent wave-2 messaging node; consumes wave-0/1 foundations + `beam_gc`).
- `../../CONTEXT.md` → workspace contracts (§1 A.1 process isolation, §3 error strategy, §5 GC/memory
  model & the copy boundary, §6 test gate).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §C/§D → the process memory model (messaging/ETS copy boundary,
  what may be shared).
- `../beam_process/CONTEXT.md` → the scheduler/queue consumer; `../beam_bifs/CONTEXT.md` → `send`/`receive` BIFs.
- `../beam_alloc/CONTEXT.md` (the `Heap` copied into) and `../beam_gc/CONTEXT.md` (MSO + collector).
