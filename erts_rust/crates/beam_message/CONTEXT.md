# `beam_message` — Copy Engine (`copy_struct`) & Messaging
## Intent
Owns the **copy half of the process memory model**: moving a term graph between two process heaps
(`copy_struct`, plus the sharing-preserving `copy_struct_shared`) and the data-level messaging
primitives built on it — the `Message` envelope, the per-process FIFO message **queue**,
`send`/`receive`, and the **message-fragment model** (`HeapFragment`/`Fragments`) that homes a payload too large for the receiver's heap. Implements Part II §C/§D (the messaging/ETS copy boundary: *immutability bounds what may be shared; everything else is copied*) and the Stage-3 row of Part II §H.
Sibling responsibilities (NOT here): the **collector** and the per-process heap (`beam_gc`,
`beam_alloc`); the **scheduler / selective-receive waiting / save queue / signal ordering**
(`beam_process`); the `send`/`receive` **BIF surface and pattern matcher** (`beam_bifs`); the
**node registry** itself (there is none in the workspace — see Design Decisions).
Ground truth = the legacy C `erts/emulator/beam/copy.c` (`copy_struct_x` — the authoritative
iterative copier; `size_object_x`; `copy_shared_calculate`/`copy_shared_perform_x`;
`erts_move_multi_frags`), `erl_message.c`/`erl_message.h` (the `ErtsMessage` envelope,
`ErlHeapFragment`, the message queue, `erts_try_alloc_message_on_heap`/`new_message_buffer`/
`erts_link_mbuf_to_proc`), `erl_gc.c` (the `OffHeap`/MSO sweep rule and `erts_copy_one_frag`) and
`erl_node_tables.h` (`erts_ref_node_entry`/`erts_deref_node_entry`). Read the C; never modify it.
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
  - `dst_off_heap_mut(&mut self) -> &mut OffHeapTable` — the copier registers referenced off-heap
    objects here and `send` drains it into the message's off-heap lists.
- `pub fn copy_struct(term: Eterm, to: &mut beam_alloc::Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm>`
  — copy `term`'s graph into `to`, returning the copied root. **Iterative** (an explicit worklist —
  deep lists never recurse on the native stack). This is the **flat** copy: a subterm occurring twice
  is copied twice.
  Immediates and literal-area terms are **shared** (returned unchanged); cons cells, tuples/records,
  funs, flat maps and **hash maps** (the whole HAMT trie — every node kind plus its leaf cons cells and
  collision tuples) are copied.
  An off-heap reference is **refcounted, never deep-copied**, in three flavours: an **MSO container**
  (`HEADER_BIN_REF`, `val` = the MSO slot), a **refc binary** (the 8-word `[BinRef][ErlSubBits]` pair,
  the payload shared via its `RefcBinary` handle) and an **external pid/port/ref** (whose node is
  registered in the destination node list).
- `pub fn copy_struct_simple(term: Eterm, to: &mut beam_alloc::Heap) -> Result<Eterm>` — the common
  case of `copy_struct` with empty off-heap tables (graphs that contain no off-heap references).
- `pub fn size_of_copy(term: Eterm) -> usize` — thin forwarder to `beam_term::size_object` (the total
  flat word count `copy_struct` preflights against `Heap::available`). Exposed so callers/tests can
  size a destination heap without re-deriving it.
### `share.rs` — the sharing-preserving copier
- `pub fn copy_struct_shared(term: Eterm, to: &mut beam_alloc::Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm>`
  — copies exactly like `copy_struct` (same immediate/literal/off-heap-node policy, same iterative
  worklist) but materialises each compound subterm **once per source identity**: two occurrences of the
  same cons cell / tuple / boxed address become the *same* copy, and an off-heap object referenced
  several times is refcounted **once** per identity.
  The one-copy-per-identity memo is a `HashMap<usize, Eterm>` keyed on the from-space term address
  (immediates and literal-area terms are never memoised — they are shared as-is); the C's destructive
  in-place markers are replaced by that map, leaving the source heap untouched.
  The preflight is `beam_term::size_object(term)` — the **flat** word count, a safe upper bound — so a
  destination heap is sized exactly as for `copy_struct`; the saving is in the words actually written.
### `fragment.rs` — the message-fragment model (`ErlHeapFragment`/`ErtsMessage` analogue)
- `pub struct HeapFragment` — a bump-allocated heap region plus its off-heap objects and its roots,
  mirroring the C `ErlHeapFragment` (`{ next; off_heap; alloc_size; used_size; mem[1]; }`).
  - `new(capacity_words)` / `with_allocator(alloc, capacity_words)`
  - `heap() -> &Heap` / `heap_mut() -> &mut Heap` (the `mem[1..alloc_size]` region)
  - `off_heap() -> &OffHeapTable` / `off_heap_mut() -> &mut OffHeapTable` (the fragment's `off_heap`)
  - `used()` / `capacity()` / `available()` (the region's `used_size` / `alloc_size` / head-room)
  - `off_heap_overhead_bytes()` — the administrative overhead of the MSO objects (the C `ErlOffHeap.overhead`)
  - `copy_root(term: Eterm, src: &OffHeapTable) -> Result<Eterm>` — copies `term` from a process whose
    off-heap table is `src` into the fragment's heap, refcounts its off-heap references into the
    **fragment's** table (fresh slots), records the result as a root and returns it (the C
    `new_message_buffer` + copy step of `erts_try_alloc_message_on_heap`)
  - `push_root(Eterm)`, `roots() -> &[Eterm]` (the payload roots, `m[0]` pointing into `mem`)
  - `contains(term) -> bool` — whether the term's object lives in this fragment's region
- `pub struct Fragments` — an ordered chain of `HeapFragment`s (the C `ErlHeapFragment.next` chain):
  `new()`/`Default`, `is_empty()`, `len()`, `push(HeapFragment)`, `iter()`/`iter_mut()`,
  `total_used_words()` (the process message-buffer accounting — `used_size` summed) and
  `off_heap_overhead_bytes()`.
  - `move_into(dst: &mut Heap, dst_off_heap: &mut OffHeapTable) -> Result<Vec<Eterm>>` — the move
    entry point (the `erts_move_multi_frags`/`erts_copy_one_frag` analogue): for each fragment in
    order it copies each root with `copy_struct` (the fragment's off-heap table as source,
    `dst_off_heap` as destination), so the moved term references **fresh** slots in `dst_off_heap`,
    and then releases the fragment's own off-heap references (ownership handed to the destination).
    Refc binaries and MSO objects move as **handles** (same `RefcBinary::data_ptr`, refcount
    incremented, never deep-copied); roots come back in chain order. One-shot.
### `message.rs` — the envelope and the queue
- `pub struct Message` — the copied payload plus sender identity and the off-heap objects it owns.
  - `Message::new(payload: Eterm, from: Eterm) -> Message` (token = `NIL`, no off-heap objects, no fragments)
  - builder: `with_token(Eterm)`, `with_off_heap(Vec<MsoHandle>)`, `with_binaries(Vec<RefcBinary>)`,
    `with_nodes(Vec<NodeEntry>)`, `with_fragments(Fragments)`
  - accessors: `payload() -> Eterm`, `from() -> Eterm`, `token() -> Eterm`,
    `off_heap() -> &[MsoHandle]`, `binaries() -> &[RefcBinary]`, `nodes() -> &[NodeEntry]`,
    `fragments() -> Option<&Fragments>`, `fragments_mut() -> Option<&mut Fragments>`
  - mirrors C `ERL_MESSAGE_TERM`/`_TOKEN`/`_FROM` (`m[0]`/`m[1]`/`m[2]`) + the message's `off_heap`, and
    carries the two categories `beam_message` tracks beside it (refc binaries, node references).
  - a payload that fits the receiver heap lives **in that heap** with no fragments and its off-heap
    objects in `off_heap`/`binaries`/`nodes`; a payload too large lives in the message's `Fragments`
    (the `data.heap_frag`/`hfrag` of `ErtsMessage`) and `payload()` points into them.
- `pub struct Queue` — the per-process FIFO message queue (a pure data structure; **no** scheduler
  coupling). `new()`/`Default`, `enqueue(Message)`, `dequeue() -> Option<Message>`,
  `peek() -> Option<&Message>`, `len()`, `is_empty()`, `clear()`, `iter() -> impl Iterator<Item=&Message>`.
  - **positional scan cursor** (what the engine's candidate-by-candidate receive drives):
    `cursor() -> QueueCursor` (first message), `cursor_at(position) -> QueueCursor`,
    `position(QueueCursor) -> usize`, `message_at(QueueCursor) -> Option<&Message>`,
    `advance(&mut QueueCursor)` (monotonic — saturates past the end), `remove_at(QueueCursor) -> Option<Message>`.
    All bounds-safe (`VecDeque::get`/`remove`), so a past-the-end cursor or an empty queue yields
    `None` — never a panic, never an index.
  - `pub(crate) remove_index(index: usize) -> Option<Message>` — index-based removal used by
    `mailbox::receive` (the public `remove_at` name belongs to the cursor API).
- `pub struct QueueCursor` — `Copy`/`Clone`/`Debug`/`Default` (position 0)/`PartialEq`/`Eq`; a
  **monotonic scan position** over a `Queue`, not a stable handle: `remove_at` shifts every later
  position, so a cursor outstanding across a removal is stale and callers re-derive one via
  `Queue::cursor()` (the engine does exactly this: remove the match, then reset to the first message).
### `mailbox.rs` — send / receive primitives (data level)
- `pub fn send(from: Eterm, term: Eterm, to_heap: &mut Heap, to_queue: &mut Queue, ctx: &mut CopyContext<'_>) -> Result<()>`
  — copy `term` into `to_heap`, then **drain all three** destination off-heap categories into the
  `Message`: `drain_binaries()` → `with_binaries`, `drain_nodes()` → `with_nodes`,
  `drain_handles()` → `with_off_heap`. This is the copy-into-the-receiver's-heap half of the C
  `erts_queue_message`/`copy_struct` path; the scheduler signalling half is `beam_process`.
  A payload that does **not** fit fails with `BeamError::Alloc(AllocKind::Message)` and leaves
  `to_heap` and the destination table untouched (the copier preflights capacity).
- `pub fn send_fragmented(from, term, to_heap, to_queue, ctx, fragment_capacity_words: usize) -> Result<()>`
  — like `send`, but a payload that does not fit the receiver's heap is copied into a
  `HeapFragment` instead of failing (the C `erts_try_alloc_message_on_heap`/`new_message_buffer`
  sender). Only `BeamError::Alloc(AllocKind::Message)` triggers the fragment path (any other failure
  is propagated unchanged); the fragment is sized to hold the whole payload, `term` is copied into
  its heap with a `CopyContext` whose source is `ctx`'s off-heap table and whose destination is the
  **fragment's** off-heap table (so the fragment owns the payload's off-heap objects), and the
  enqueued `Message` points into the fragment and owns the `Fragments`. `ctx`'s own destination
  off-heap table is **not** used on the fragment path (a fragment carries its own `off_heap`, as C does).
- `pub fn receive<F: FnMut(&Message) -> bool>(queue: &mut Queue, pred: F) -> Option<Message>` —
  **remove and return** the first queued message whose payload satisfies `pred` (selective receive
  skeleton). Waiting, `after` timeouts, the save queue and signal ordering live in `beam_process`.
- `pub fn receive_ref<F: FnMut(&Message) -> bool>(queue: &Queue, pred: F) -> Option<&Message>` —
  non-destructive variant.
### `off_heap.rs` — the off-heap (MSO + binary + node) reference table
- `pub use beam_gc::MsoHandle;` — the off-heap object handle is **beam_gc's** (MSO object + atomic
  `RefCount`); this crate never re-implements (nor re-stores) MSO.
- `pub struct OffHeapTable` — a **slot-indexed** table of `MsoHandle`s (slot = the on-heap
  off-heap-reference's `val` word). A thin MSO-typed façade whose storage is delegated to
  `beam_gc::OffHeap` (the workspace's single owner of the off-heap list); it adds the slot↔object
  association plus the **refc-binary** and **node** lists that ride along with a copied message.
  - MSO API (unchanged): `new()`/`Default` (manual — `beam_gc::OffHeap` is not `Default`),
    `push(MsoHandle) -> usize` (returns the slot), `get(slot) -> Option<&MsoHandle>`,
    `clone_slot(slot) -> Option<MsoHandle>` (**bumps the refcount**), `len()`, `is_empty()`, `iter()`,
    `overhead_bytes()`, `into_handles() -> Vec<MsoHandle>`, `drain_handles() -> Vec<MsoHandle>`.
  - untyped escape hatch: `get_entry(slot) -> Option<&beam_gc::OffHeapEntry>`.
  - refc binaries: `push_binary(RefcBinary) -> usize`, `binary_at(slot) -> Option<RefcBinary>` (clone ⇒
    refcount++), `binary_by_data_ptr(*const u8) -> Option<RefcBinary>` (clone; resolves an on-heap
    `BinRef.val` to its owning handle), `binaries() -> &[RefcBinary]`, `drain_binaries() -> Vec<RefcBinary>`.
  - nodes: `push_node(NodeEntry) -> usize` (one entry per **distinct** node; a repeat counts another
    reference onto it), `nodes() -> &[NodeEntry]`, `node_by_identity(Eterm, u32) -> Option<&NodeEntry>`,
    `drain_nodes() -> Vec<NodeEntry>`.
  - `Drop` releases every node reference the table registered.
  - **Slots are stable; this table NEVER calls `beam_gc::OffHeap::sweep`** (which compacts and would
    shift the slot indices the copier writes into on-heap containers). Only the append-only
    `insert`/`get`/`iter` path is used.
  - `into_handles`/`drain_handles` **clone-then-drop** each handle (refcount++ then refcount-- when
    the consumed list drops), because `beam_gc::OffHeap` has no move-out API — net refcount is
    unchanged.
### `node.rs` — external-thing node entries
- `pub struct NodeEntry` — a refcounted `(node, creation)` identity, the unit of
  `erts_ref_node_entry`/`erts_deref_node_entry`.
  - `NodeEntry::new(node: Eterm, creation: u32) -> NodeEntry` (one reference)
  - `node() -> Eterm`, `creation() -> u32`, `refcount() -> usize`, `same_node(&NodeEntry) -> bool`,
    `identity_eq(Eterm, u32) -> bool`, `handle() -> NodeEntry` (a second observation handle; it does
    **not** register a reference).
  - `Clone`-free by design: the count tracks *references registered in a table*, not handles.
## Constraints
- **Errors: `beam_common::{Result, BeamError}` only.** Heap exhaustion mid-copy ⇒
  `BeamError::Alloc(AllocFailure::new(AllocKind::Message, bytes))` — **never panic**, never a
  crate-local error enum, never `unwrap()` on runtime data. A malformed/unsupported term layout, an
  out-of-range MSO slot, or a refc binary whose payload no held handle owns ⇒ `BeamError::BadArg`.
- **`unsafe` is confined to `copy.rs`** (raw word read/write in the copy loop) and the narrow
  `layout.rs` accessors; every `unsafe fn` carries a `# Safety` section and every `unsafe` block a
  preceding `// SAFETY:` rationale. `share.rs`, `fragment.rs`, `off_heap.rs`, `message.rs`,
  `mailbox.rs`, `node.rs`, `error.rs` are safe Rust. `unsafe_op_in_unsafe_fn` stays clean.
- **Never assume a different `Eterm` layout** — read headers/words through `beam_term` accessors.
  The frozen word layouts (cons `[CAR,CDR]`, header words, `BinRef [thing_word,val,next]`, the
  `[BinRef(3)][ErlSubBits(5)]` refc pair, `ExternalThing [header,node,next,data]`) are the contract;
  interiors that are not externally pinned are read through the owning `beam_term` accessors (e.g. the
  HAMT node shapes via `map_header_type`/`map_header_val`/`header_arity`), never re-derived.
- **`copy_struct_shared` reuses `copy.rs`'s `shallow` primitive** — the copy logic exists once.
- **Deps (fixed by `Cargo.toml`):** `beam_common`, `beam_term`, `beam_alloc`, `beam_gc` (the `beam_gc`
  edge is described under Design Decisions). `beam_codec` and `libc` are also declared there but are
  **unused** — no `beam_codec`/`libc` reference exists in `src/` or `tests/`. No new external dependencies.
- Files target ~1000 lines; one responsibility per file. Licensing Apache-2.0.
## Design Decisions
- **Fidelity scope: a correct, iterative *tree* copy, not the C's single-allocation `htop`/`hbot` layout.**
  The C pre-sizes (`size_object`), then bumps lists up from the heap base and boxed things down from the
  top so outer objects precede inner ones for `copy_ets_element`. This crate allocates each object
  *incrementally* from the destination heap (parent before child, so the outer-before-inner ordering
  invariant still holds) with an **explicit worklist** for fix-ups — no native recursion, so a `1e5+`-element
  list cannot overflow the stack. `size_of_copy` (= `size_object`) is still used as an up-front capacity preflight.
- **Sharing policy — copy trees, share exactly what the C shares.**
  C `copy_struct` is a *tree* copier: a subterm occurring twice is copied twice. The two things it does
  **share** are reproduced: (1) **immediates** (`is_immediate`) are returned as-is; (2) **literal-area
  terms** (a boxed/list word with the `TAG_LITERAL_PTR` bit, tested *only* on compound words) are
  returned as-is; the zero-arity tuple (`HEADER_ARITYVAL` with arity 0) is the shared
  `beam_term::tuple0()` literal and is returned as-is.
- **The sharing-preserving copier is opt-in and additive.** `copy_struct` keeps the C default's flat
  behaviour (a repeated subterm is copied twice) so existing dependents are unchanged; the
  sharing-preserving variant is `copy_struct_shared`, the analogue of the C
  `copy_shared_calculate`/`copy_shared_perform_x` pair (`copy.c:1111-1460`, state `erts_shcopy_t`,
  `global.h:1061-1081`). Where the C keys sharing on the from-space address with **destructive in-place
  markers**, Rust keeps a real `HashMap<usize, Eterm>` — no source-heap mutation.
- **MSO is reused from `beam_gc`, not reimplemented.** The off-heap object, its atomic `RefCount`, its
  owning `MsoHandle` and the per-process off-heap list are **entirely `beam_gc::mso`'s**.
  `beam_gc::OffHeap` exposes slot-indexed read access (`insert`/`get`/`iter`), so the copy boundary
  reads the source process's off-heap objects to re-register (refcount) them straight from the shared
  storage — `off_heap.rs` holds **no duplicate MSO storage**. No new object model and no new
  refcounting. The `beam_message → beam_gc` edge is acyclic (`beam_gc` depends only on the wave-0/1
  foundations and never on `beam_message`).
- **Off-heap reference term — the M1 encoding (documented substitution).** A standalone off-heap
  reference is the frozen `BinRef` container thing `[HEADER_BIN_REF, val, next]`
  (`ERL_BIN_REF_SIZE = 3`) with **`val` = the MSO slot index** into the owning process's
  `OffHeapTable` and **`next` unused (`0`)**. Copying such a container resolves the source slot,
  **clones the handle** (refcount++), pushes it into the destination table and writes the **new** slot;
  the payload is never deep-copied.
- **Refc binaries share the payload; small binaries are copied by value.** An on-heap **refc binary**
  is the frozen 8-word pair `[BinRef(3)][ErlSubBits(5)]` whose tagged root is the `ErlSubBits` and
  whose `BinRef.val` is the payload's first data byte (always distinct from the standalone MSO
  container above, which is a bare `HEADER_BIN_REF` thing). Its copy (`copy.c:812-848`, the C's
  `erts_refc_inc(&from_br->val->intern.refc, 2)`) resolves the owning `RefcBinary` handle from the
  source table by that data pointer, refcounts it into the destination binary list, allocates a fresh
  8-word pair, keeps the window (`base_flags`/`start`/`end`) verbatim, reuses the **same** payload
  pointer in the copied `BinRef.val`, and fixes the copied `ErlSubBits.orig` up to the **copied**
  `BinRef` — the fix-up an opaque-leaf treatment of a `SUB_BITS` would leave dangling.
  `beam_term::size_object` already counts the whole 8-word pair, so the preflight is coherent with no
  local size computation. An on-heap **`ErlHeapBits`** small binary has no navigable child and is
  copied verbatim (its words are opaque).
- **External pid/port/reference nodes: refcounted per identity, modeled locally.** Copying an
  external thing registers its node (`copy.c:882-909`) and copies the thing's words verbatim. There is
  **no node-registry crate anywhere in the workspace** and no `ErlNode *` to point at, so the node
  identity is modeled as [`NodeEntry`] = `(node atom term, creation)` plus an explicit atomic
  refcount, and the node list lives **on `OffHeapTable`** (carried to the `Message` by `send`, exactly
  like the off-heap list). The count is the number of references **the registering table** holds — one
  per copied external thing, deduped to one list entry per distinct node — so a term with two
  references to the same node leaves one entry with count `2`. The M1 substitution (node as an atom
  term rather than a registry pointer) is the same one `beam_term::thing` documents; the refcount is
  structural bookkeeping that future-proofs a real registry, not an observable Erlang-level value, and
  it is kept per-table because there is no global registry to share it with.
- **The destination off-heap table *is* the message's off-heap ownership.** `send` passes the
  destination table as `ctx`'s destination and then drains all three categories into the `Message`,
  mirroring how C links copied BinRefs / external things into the `ErlHeapFragment.off_heap` so the
  receiving process's GC can sweep them.
- **The message-fragment move is a copy + release, not the C's in-place move markers.** A fragment's
  payload holds its off-heap references as **slot indices** into the owning table (the M1 encoding),
  so `Fragments::move_into` cannot relink an object list the way the C `erts_move_multi_frags` does
  (C terms hold raw pointers to the objects, so only in-place move markers are needed). Instead it
  copies each root with `copy_struct` (the fragment's table → `dst_off_heap`), which re-registers —
  refcounts — every referenced object under a **fresh** destination slot, and then releases the
  fragment's own references. The **net refcount is unchanged by the move** (one increment from the
  copy, one decrement from the release), exactly as the C relink leaves it, so the ownership transfer
  is faithful even though the mechanism (copy + release) differs. `send_fragmented` is the C
  `erts_try_alloc_message_on_heap` sender: it tries the receiver-heap copy first and fragments only on
  `AllocKind::Message`; the fragment (sized to the whole payload, C `new_message_buffer(sz)`) then
  owns the off-heap objects while the message lives.
- **The fragment's backing `Heap` never reallocates, so a root's address is stable across a move of
  `Fragments`.** The envelope holds `Fragments` by value, but the heap's region is a separate,
  never-moving allocation, so `Message::payload` (which points into the region) stays valid as the
  `Message` — and therefore the `Fragments` — is moved around.
- **A refc binary whose payload no held handle owns is a malformed graph ⇒ `BeamError::BadArg`.**
  The M1 encoding has no other way to recover the handle (there is no `Binary*` back-pointer), and a
  silent deep copy would be a wrong result.
- **`size_object` is used, not re-derived.** `beam_term::size_object` (`size_object_x` port) gives the
  total word count for the capacity preflight; a tiny local `layout::object_words` gives the
  *single-object* word count the incremental allocator needs — the same split as `beam_gc::copy`'s
  `move_size`. All three size/walk every HAMT node kind with the same
  `1 + header_arity + popcount(MAP_HEADER_VAL)` formulation, so a hash map copies as a whole trie.
## Test Strategy
- In-module `#[cfg(test)] mod tests` per module: `layout` (per-object word counts + child-slot
  enumeration for tuple/record/fun/flatmap/**HAMT node**/off-heap container), `off_heap` (slot
  push/get/clone bumps the refcount; `into_handles`; `drain_handles` empties the table and leaves it
  re-usable; binaries tracked beside the MSO list; `push_node` dedups and counts references; dropping
  the table releases them), `message` (builder + accessors for all three off-heap categories;
  `Queue` FIFO/`peek`/`len`/`clear`; cursor start/`position`, FIFO `advance` saturating past the end,
  `remove_at` shifting the rest, draining via a re-derived cursor, empty-queue past-the-end,
  `cursor_at`), `node` (a new entry starts at one reference, `add_reference` counts, a handle shares
  the count; identity is node + creation), `copy` (immediates shared; cons/tuple/record/nested copied;
  literal shared; off-heap container refcounted; exhaustion ⇒ `AllocKind::Message`),
  `share` (a repeated tuple is materialised once; a distinct-subterm graph copies to the flat size;
  immediates unchanged), `fragment` (a fresh fragment's geometry; `copy_root` fills/records/contains;
  the chain's shape and summed `used` words; `move_into` copying a root onto the destination heap and
  transferring an off-heap object).
- `tests/copy.rs` — the flat-copy correctness evidence: nested-graph equality, a `100_000`-element
  deep list (no stack overflow), literal sharing by identity, MSO refcount + slot rewrite, heap
  exhaustion ⇒ `AllocKind::Message`, and a 40-key HAMT map copied as a whole trie.
- `tests/share.rs` — the sharing-preserving evidence: (a) a `1000`-element subterm repeated `N = 8`
  times ⇒ the shared destination uses `2N + 2K` words vs the flat `2N + N·2K` (a >7× saving) and every
  occurrence shares one destination address while the flat copy's are all distinct; (b) the shared copy
  is still `compare::eq` the original; (c) a graph with **no** sharing copies to exactly the flat size
  (`dst.used() == size_of_copy(original)`); (d) a shared off-heap MSO container is refcounted once per
  identity (one destination entry, one new handle) against `N` entries for the flat copy.
- `tests/binary.rs` — refc-binary sharing: one destination binary, refcount 2 (source + destination),
  the same bytes and the same `data_ptr()`, the copied `BinRef.val` == the source payload pointer, the
  copied `ErlSubBits.orig` reaching the copied `BinRef`, `is_refc_binary`/`refc_bytes`/`refc_offset`/
  `refc_size` on the copy, `dst.used() == ERL_REFC_BITS_SIZE`, dropping the destination table returning
  the source refcount to 1; two occurrences sharing one payload; a refc binary with no owning handle ⇒
  `BadArg`; and an on-heap `ErlHeapBits` small binary copied by value (equal bytes, distinct address,
  `heap_bits_size_words` words).
- `tests/external.rs` — external thing copy: words verbatim and the node atom/creation preserved (via
  `external_*_parts`), one node entry per distinct node, two references to the same node ⇒ one entry
  with refcount 2, port and reference nodes registered separately, and dropping the destination
  releasing the node. `compare::eq` is deliberately **not** used (see Known Issues).
- `tests/fragments.rs` — the message-fragment evidence: a payload larger than the receiver heap makes
  plain `send` fail with `AllocKind::Message` (leaving `to_heap.used() == 0`) while `send_fragmented`
  enqueues a structurally-equal payload that points into the fragment (`HeapFragment::contains`) and
  differs from the source address and the receiver heap; `move_into` copies the payload onto the
  receiver heap (address inside it) and transfers the fragment's off-heap MSO object into the
  destination table (source + destination, the fragment released); a two-fragment chain moves both
  roots back in order; a fragmented 256-byte `RefcBinary` moves the **handle** (same `data_ptr`, bytes
  unchanged, refcount 2 = source + receiver, the pair `ERL_REFC_BITS_SIZE` on the receiver heap); and
  fragment accounting (`used`/`capacity`/`total_used_words`, the receiver heap staying empty).
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace`.
  Always go through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never touch `../../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness, not here.
## Known Issues / Intentional M1 gaps
- **`beam_term::compare` has no structural arm for external pid/port/reference.** It falls through to
  the deterministic `fallback_cmp` (same class ⇒ order by object arity, then raw term word), so a copy
  is never `=:=` its original and `compare::eq` cannot be used to check a copied external thing. The
  crate's tests assert structural equality through the `beam_term::thing` accessors instead; a fix
  belongs to `beam_term`.
- **No sharing-preserving copy for the flat path.** `copy_struct` duplicates a repeated subterm
  (C's default); `copy_struct_shared` is the opt-in alternative. Only immediates, literal-area terms
  and the zero-arity tuple are shared outright.
- **Standalone `ErlSubBits` match contexts are copied verbatim.** A `SUB_BITS` whose `orig` is *not* a
  `BinRef` (a match context) is not a refc binary, so it is treated as an opaque leaf; its interior has
  no navigable on-heap child in M1 and a `orig` pointing at a source-heap object would be copied as-is
  (unreachable until a match-context constructor exists).
- **The node identity is an atom term, not a registry pointer.** There is no node registry in the
  workspace, so [`NodeEntry`] keys `(node atom, creation)` and its count is per-table structural
  bookkeeping (see Design Decisions); a real registry would replace the atom with an `ErlNode *` and
  make the count global across processes.
- **`copy_struct_shared` preflights the *flat* word count.** It is a safe upper bound, so a caller sizes
  the destination heap as for `copy_struct`; the memory saving is in the words written, not in the
  accepted capacity.
- **The fragment model is not yet wired into a runtime.** `HeapFragment`/`Fragments`/`send_fragmented`
  exist, but `beam_rt`/`beam_process` still call `send` (exhaustion ⇒ `AllocKind::Message`); a runtime
  must adopt `send_fragmented`, hold a received message's `Fragments` as its message-buffer model and
  later `move_into` the process heap.
- **A fragment holds the whole payload; there is no automatic multi-fragment split.** The C streams an
  oversized message across a chain of `new_message_buffer` fragments; here one fragment is sized to the
  whole payload (`fragment_capacity_words.max(size_object(term))`) and the chain is built by the caller
  (`Fragments::push`). A size-bounded split is `beam_process`'s heap-management concern.
- **`Fragments::move_into` moves by copy + release, not the C's in-place move markers.** It re-copies
  each root to fresh destination slots and then releases the fragment's own references; the net
  refcount is unchanged (see Design Decisions), but the mechanism differs and the move is one-shot.
- **Scheduler coupling is absent by design.** `Queue` is a pure FIFO; selective-receive waiting,
  `after`, the save queue, signal/priority ordering and the message-queue length semantics of
  `ErtsSignalPrivQueues` are `beam_process`'s.
- **`Message.token` is a plain field (`NIL` when absent).** The seq-trace / distribution-token
  machinery that fills it is `beam_dist`/`beam_rt`.
- **The binary/node lists are carried but not yet consumed.** `beam_process`/`beam_rt` must own a
  received message's `binaries()`/`nodes()` (they are the receiving process's off-heap references) —
  today nothing sweeps them, the `Message`'s own `Drop` releases them. A **fragmented** message instead
  leaves those lists empty: its payload's off-heap objects live in the fragment's `off_heap` table until
  `Fragments::move_into` hands them to the destination table.
- **Fun blocks are mis-sized/mis-walked: the copier uses the C `ERL_FUN_SIZE` prefix, not the frozen
  `beam_term::fun` inlined-descriptor layout.** `layout.rs` treats a fun as `[header][2 words][env]` —
  `object_words` is `ERL_FUN_SIZE + num_free` (line 45) and `for_each_child_slot` visits env slots
  `ERL_FUN_SIZE..ERL_FUN_SIZE + num_free` (lines 89-93) — i.e. the legacy C `ErlFunThing`
  (`thing_word` + one `entry` pointer). The frozen layout `beam_term::fun` (and its consumer
  `beam_gc::copy::{move_size,scan_step}`) instead places the env at `1 + desc_words(kind)` (3 for a
  `Local`/`External` fun, 11 for an `Imported` one) with block `1 + desc_words(kind) + num_free`.
  Consequence: `copy_struct` under-sizes a copied fun by `desc_words(kind) - 1` words and visits the
  wrong slots (for `num_free == 1` it reads the descriptor's 2nd word as a term and never fixes up the
  real env word, leaving the copied env pointing at the source heap). No test copies a fun, so this is
  latent; fix by routing funs through `beam_term::fun::{fun_env_offset_header, fun_block_words_header}`.
  (The `layout.rs` unit tests currently pin the wrong `ERL_FUN_SIZE` geometry.)
## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/copy.rs` → the iterative `copy_struct` engine (the `unsafe` core) + `CopyContext` + `size_of_copy`
- `src/share.rs` → the sharing-preserving `copy_struct_shared` (source-identity memo over `copy.rs`'s `shallow`)
- `src/fragment.rs` → `HeapFragment` (bump region + off-heap table + roots) and `Fragments` (the chain + `move_into`); the `ErlHeapFragment` message-buffer model
- `src/layout.rs` → per-object word counts + child-slot enumeration (tuple/record/fun/flatmap/HAMT node/off-heap container) + refc-binary/external-thing identification + the off-heap container encoding
- `src/off_heap.rs` → `OffHeapTable` (MSO-typed façade over slot-indexed `beam_gc::OffHeap`, plus the refc-binary and node lists); MSO handle re-export
- `src/node.rs` → `NodeEntry`, the refcounted `(node, creation)` external-thing node identity
- `src/message.rs` → `Message` envelope (payload/from/token + off-heap, binary and node lists + optional `Fragments`) + `Queue` FIFO
- `src/mailbox.rs` → `send` / `send_fragmented` / `receive` / `receive_ref` data-level primitives
- `src/error.rs` → `pub(crate)` copy-failure constructors (no new error type)
- `tests/copy.rs` → flat-copy correctness: nested-graph equality, deep list, literal sharing, MSO refcount, exhaustion, HAMT trie
- `tests/share.rs` → sharing-preserving evidence: N-fold subterm, structural equality, no-sharing size, shared MSO identity
- `tests/binary.rs` → refc-binary payload sharing (refcount, `data_ptr`, `orig` fix-up, size) + on-heap small-binary copy-by-value
- `tests/external.rs` → external pid/port/ref verbatim copy + node linkage/refcounting
- `tests/queue.rs` → `send`/`receive` + FIFO queue integration + the `QueueCursor` scan/remove path
- `tests/fragments.rs` → the fragment-model evidence: `send` vs `send_fragmented`, `move_into` (payload onto the receiver heap + off-heap transfer), the multi-fragment chain, refc-binary handle move, accounting
- `../../erts/emulator/beam/copy.c` → the authoritative copier (`copy_struct_x`/`size_object_x`/`copy_shared_*`/`erts_move_multi_frags`) (read-only)
- `../../erts/emulator/beam/erl_message.h` / `erl_message.c` → the message envelope, `ErlHeapFragment`, queue layout (read-only)
- `../../erts/emulator/beam/erl_node_tables.h` → `erts_ref_node_entry`/`erts_deref_node_entry` (read-only)
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
- `../beam_alloc/CONTEXT.md` (the `Heap` copied into, `RefcBinary`) and `../beam_gc/CONTEXT.md` (MSO + collector).
