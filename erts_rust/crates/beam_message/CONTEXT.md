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
  collision tuples) are copied. A `SUB_BITS` **match context** (an `ErlSubBits` over an on-heap
  bitstring) is **flattened** into a fresh on-heap bitstring for its window (never copied verbatim, so
  the destination never aliases the source heap).
  An off-heap reference is **refcounted, never deep-copied**, in three flavours: an **MSO container**
  (`HEADER_BIN_REF`, `val` = the MSO slot), a **refc binary** (the 8-word `[BinRef][ErlSubBits]` pair,
  the payload shared via its `RefcBinary` handle) and an **external pid/port/ref** (whose node is
  registered in the destination node list). A refc binary whose payload **no handle in the source
  table owns** has no payload to share and is instead flattened into a fresh on-heap bitstring —
  exactly the treatment of a `SUB_BITS` match context (`copy_match_context`), which is sound because a
  refc binary's `base_flags` word already *is* its payload data pointer.
  The copy is **single-pass for a clean destination** (an empty heap *and* an off-heap table empty in
  all three lists — the fresh-fragment / fresh-receiver-heap case): `try_copy_fast` materialises the
  whole graph through a `TrialTarget` bump pointer over the heap's unwritten words and commits it with
  one `alloc_words`, so the sizing walk is skipped entirely. Every other destination (and every
  clean-destination failure) takes the **preflight-then-write** path: the cycle-safe bounded
  `preflight_words(term, limit, ctx)` runs before any word is written, so a graph that does not fit is
  refused **before** any write (the destination is left untouched) and a fast-path failure is
  re-derived as the crate's typed outcome. Either way a non-term (a bare header word, incl. `THE_NON_VALUE`) or a forged
  **cyclic** graph is a typed `BeamError::BadArg`, never a wrong result and never a hang, and a refusal
  is always `BeamError::Alloc(AllocKind::Message)` with **no partial write**.
- `pub fn copy_struct_simple(term: Eterm, to: &mut beam_alloc::Heap) -> Result<Eterm>` — the common
  case of `copy_struct` with empty off-heap tables (graphs that contain no off-heap references).
- `pub fn size_of_copy(term: Eterm) -> usize` — thin forwarder to `beam_term::size_object` (the total
  flat word count). Exposed so callers/tests can size a destination heap without re-deriving it; the
  copier's own preflight is the cycle-safe `preflight_words`, not this.
- `pub(crate) trait CopyTarget` + `struct TrialTarget` + `fn is_clean_destination` / `fn try_copy_fast`
  (crate-internal): the `copy_struct` destination abstraction. `Heap` reserves per object; `TrialTarget`
  is the single-pass attempt's bump pointer over `[to.hp(), hend)` with `free = to.available()`, counting
  `written` without touching `used`/`peak`. `is_clean_destination` = `to.used() == 0 && ctx.dst_off_heap().is_fully_empty()`.
  A failed attempt is rolled back with `to.reset()` + `OffHeapTable::rollback` (`is_fully_empty`/`rollback`
  are `pub(crate)` on `off_heap.rs`), then the preflight path re-derives the typed outcome.
### `share.rs` — the sharing-preserving copier
- `pub fn copy_struct_shared(term: Eterm, to: &mut beam_alloc::Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm>`
  — copies exactly like `copy_struct` (same immediate/literal/off-heap-node policy, same iterative
  worklist) but materialises each compound subterm **once per source identity**: two occurrences of the
  same cons cell / tuple / boxed address become the *same* copy, and an off-heap object referenced
  several times is refcounted **once** per identity.
  The one-copy-per-identity memo is a `HashMap<usize, Eterm>` keyed on the from-space term address
  (immediates and literal-area terms are never memoised — they are shared as-is); the C's destructive
  in-place markers are replaced by that map, leaving the source heap untouched.
  It preflights through the same cycle-safe `copy::preflight_words` as `copy_struct`, so the
  destination is sized exactly as for `copy_struct`; the saving is in the words actually written.
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
    `new_message_buffer` + copy step of `erts_try_alloc_message_on_heap`). A fragment too small for
    the payload is refused up front (`copy_struct`'s preflight), so `copy_root` performs **no partial
    write** — the roots stay empty and `used()` stays `0`.
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
    keeping the fragment's own references until the *whole chain* has copied and only then releasing
    them (ownership handed to the destination). An allocation failure while copying leaves **every**
    fragment intact (a failed move gives up no ownership; the caller discards the destination words it
    did copy and retries with a larger `dst` + fresh `dst_off_heap`). Refc binaries and MSO objects
    move as **handles** (same `RefcBinary::data_ptr`, refcount incremented, never deep-copied); roots
    come back in chain order. One-shot.
### `message.rs` — the envelope and the queue
- `pub struct Message` — the copied payload plus sender identity and the off-heap objects it owns.
  - `Message::new(payload: Eterm, from: Eterm) -> Message` (token = `NIL`, no off-heap objects, no fragments)
  - builder: `with_token(Eterm)`, `with_from(Eterm)`, `with_off_heap(Vec<MsoHandle>)`,
    `with_binaries(Vec<RefcBinary>)`, `with_nodes(Vec<NodeEntry>)`, `with_fragments(Fragments)`
    (`with_from` is builder parity with `with_token`, for a message whose sender is not known at
    `Message::new` time; to write a relocated `from` into an existing envelope use `set_from`)
  - accessors: `payload() -> Eterm`, `from() -> Eterm`, `token() -> Eterm`,
    `off_heap() -> &[MsoHandle]`, `binaries() -> &[RefcBinary]`, `nodes() -> &[NodeEntry]`,
    `fragments() -> Option<&Fragments>`, `fragments_mut() -> Option<&mut Fragments>`
  - mirrors C `ERL_MESSAGE_TERM`/`_TOKEN`/`_FROM` (`m[0]`/`m[1]`/`m[2]`) + the message's `off_heap`, and
    carries the two categories `beam_message` tracks beside it (refc binaries, node references).
  - a payload that fits the receiver heap lives **in that heap** with no fragments and its off-heap
    objects in `off_heap`/`binaries`/`nodes`; a payload too large lives in the message's `Fragments`
    (the `data.heap_frag`/`hfrag` of `ErtsMessage`) and `payload()` points into them.
  - **logical-size accounting surface** (the C `size_object_litopt` / `bin_vheap` inputs a host needs):
    `heap_words() -> usize` (the sender copy-charge input — the payload graph's flat word count when
    heap-resident, the fragment chain's `total_used_words()` when fragmented; **heap words only**, so an
    off-heap refc binary contributes just its 8-word `[BinRef][ErlSubBits]` handle) and
    `logical_binary_bytes() -> usize` (the *holder's* virtual-binary-heap input — the summed `RefcBinary`
    payload lengths of `binaries()`, `0` when none).
  - **rebuild-preserving** `with_payload(Eterm) -> Message` — a new payload term keeping `from`,
    `token`, `off_heap`, `binaries`, `nodes` AND `fragments`: the relocation-safe rebuild a collector
    must use (a from-scratch `Message::new` rebuild drops the fragment chain and the binary/node lists —
    a use-after-free a sibling currently has; see Known Issues).
  - **envelope relocation invariant** — the envelope's GC-rooting (relocation) surface, field by
    field:
    | field | treatment |
    |---|---|
    | `payload: Eterm` | relocatable — rebuilt with `with_payload` |
    | `from: Eterm` | relocatable — `set_from(&mut self, Eterm)` (in-place) / `with_from` (builder) |
    | `token: Eterm` | relocatable — `set_token(&mut self, Eterm)` |
    | `off_heap: Vec<MsoHandle>` | **not a term** (a refcounted off-heap handle over a stable slot) — no relocation |
    | `binaries: Vec<RefcBinary>` | **not a term** (an off-heap refcounted payload handle) — no relocation |
    | `nodes: Vec<NodeEntry>` | the node word is a sysname **atom** ⇒ immediate — immediate-required |
    | `fragments: Option<Fragments>` | the payload points *into* the fragment region (foreign to the collected nursery) — not host-relocatable |
    A host that stores a **heap** word in `from`/`token` must gather it as a GC root and scatter it
    back through `set_from`/`set_token` on every collection of the owning process; with no such gather
    the word survives as a stale pointer into the evacuated from-space. The common immediate cases (a
    sender pid, a node atom, `NIL`) need no gather and are the only ones the enqueue tripwire admits.
- `pub struct Queue` — the per-process FIFO message queue (a pure data structure; **no** scheduler
  coupling). `new()`/`Default`, `enqueue(Message)`, `dequeue() -> Option<Message>`,
  `peek() -> Option<&Message>`, `len()`, `is_empty()`, `clear()`, `iter() -> impl Iterator<Item=&Message>`.
  - **enqueue tripwire** (debug-only, `debug_assert_enqueue_terms`): `Queue::enqueue` debug-asserts
    that the enqueued message's `from` **and** `token` are genuine values (`is_value`) and
    **immediates** (`is_immediate`), so a host that enqueues an un-relocated young-heap word — or a
    bare header word such as `THE_NON_VALUE` — fails loudly in tests instead of leaving a dangling word
    behind. Cheap and compiled out under `--release`. It is a deliberate narrowing of the C, which also
    permits a heap `token` tuple (`erl_message.c:388-390`) and a heap Info-tuple `from`
    (`erl_proc_sig_queue.c:2272`): a host opting into those must (a) add a root-gather for the two words
    to its collection walk (push them from `Message::from`/`Message::token`, scatter them back with
    `Message::set_from`/`Message::set_token`) and (b) deliberately relax this tripwire.
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
  **Atomic**: a payload that does **not** fit fails with `BeamError::Alloc(AllocKind::Message)` and
  leaves `to_heap`, the destination off-heap table and the queue untouched (the copier preflights
  capacity before writing a word).
- `pub fn send_fragmented(from, term, to_heap, to_queue, ctx, fragment_capacity_words: usize) -> Result<()>`
  — like `send`, but a payload that does not fit the receiver's heap is copied into a
  `HeapFragment` instead of failing (the C `erts_try_alloc_message_on_heap`/`new_message_buffer`
  sender). Only `BeamError::Alloc(AllocKind::Message)` triggers the fragment path (any other failure
  is propagated unchanged, the receiver heap left untouched); the fragment is sized to hold the whole
  payload (at least `fragment_capacity_words`, at least `size_object(term)`), `term` is copied into
  its heap with a `CopyContext` whose source is `ctx`'s off-heap table and whose destination is the
  **fragment's** off-heap table (so the fragment owns the payload's off-heap objects), and the
  enqueued `Message` points into the fragment and owns the `Fragments`. `ctx`'s own destination
  off-heap table is **not** used on the fragment path (a fragment carries its own `off_heap`, as C does).
  One fragment per send, sized to the whole payload; a size-bounded multi-fragment split is the
  caller's concern (see Known Issues).
- `pub fn receive<F: FnMut(&Message) -> bool>(queue: &mut Queue, pred: F) -> Option<Message>` —
  **remove and return** the first queued message whose payload satisfies `pred` (selective receive
  skeleton). Waiting, `after` timeouts, the save queue and signal ordering live in `beam_process`.
- `pub fn receive_ref<F: FnMut(&Message) -> bool>(queue: &Queue, pred: F) -> Option<&Message>` —
  non-destructive variant.
### `error.rs` — copy-failure classification (no crate-local error type)
- `heap_exhausted(words) -> BeamError` / `bad_term() -> BeamError` (`pub(crate)` constructors).
- `pub fn is_message_heap_exhaustion(error: &BeamError) -> bool` — re-exported at the crate root and
  in `prelude`. True iff `error` is `BeamError::Alloc(f)` with `f.kind == AllocKind::Message`: the
  **soft, recoverable** "the destination message heap could not hold the copied term" signal a host
  may collect/grow-and-retry, or translate to the emulator's documented `error:system_limit` at its
  hard limit (see Design Decisions).
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
  - **Slots are stable; this table NEVER calls `beam_gc::OffHeap::sweep`.** `beam_gc`'s sweep is
    itself slot-stable — it releases entries *in place* (a released slot becomes empty; it is neither
    reused nor reordered), so every survivor keeps its index — but reclamation is the collector's
    concern, not a read-only view's. Only the append-only `insert`/`get`/`iter` path is used here.
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
- **Errors: `beam_common::{Result, BeamError}` only.** A destination-heap exhaustion
  (as reported by the copier's preflight) ⇒
  `BeamError::Alloc(AllocFailure::new(AllocKind::Message, words * size_of::<Eterm>()))` — the
  **message-heap classification** (see Design Decisions), recognisable through
  `is_message_heap_exhaustion`. **Never panic**, never a crate-local error enum, never `unwrap()` on
  runtime data. A malformed/unsupported term layout, a **non-term** (bare header word incl.
  `THE_NON_VALUE`), a **cyclic** forged graph, or an out-of-range MSO slot ⇒ `BeamError::BadArg`
  (typed, never a hang, never a partial write). A refc binary whose payload no held handle owns is
  **not** malformed — it is flattened (see API Surface).
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
  list cannot overflow the stack. The up-front capacity preflight is `preflight_words` (below).
- **The capacity preflight is cycle-safe and bounded (`preflight_words`).** A bare
  `beam_term::size_object` is unbounded, so a forged cyclic graph (a raw-write back-edge) would hang or
  OOM it before the copy even starts. `preflight_words(term, limit, ctx)` first runs `bounded_words` —
  an allocation-free walk that mirrors *exactly* what `shallow`/`copy_refc_binary` will allocate per
  object (so it is the copier's own word count, and for every reachable shape it equals `size_object`)
  and returns `None` the moment it exceeds the destination's free words. It takes the `CopyContext`
  because one object's cost depends on the **source** table: a refc binary whose payload a held handle
  owns costs `ERL_REFC_BITS_SIZE`, while one the copier flattens costs `heap_bits_size_words(size)`
  (`refc_binary_words` keeps that rule beside the flag it mirrors, so the preflight and the copy never
  disagree — `beam_term::size_object` does not model the fallback). Only on `None` does it run
  `is_acyclic` (an iterative DFS, an on-path `HashSet<usize>`): cyclic ⇒ typed `BadArg`; acyclic ⇒ the
  same walk re-run **unbounded**, which is now provably terminating and gives the exact count (with
  `size_object` as the saturation fallback) — so a graph the bounded walk rejected always yields a count
  **above** `limit` and the caller's refusal is total, never a partial write. Every compound object
  contributes ≥2 words, so a cycle always overflows the bound first — the `HashSet` is never paid on
  the normal (fitting) path.
- **Sharing policy — copy trees, share exactly what the C shares.**
  C `copy_struct` is a *tree* copier: a subterm occurring twice is copied twice. The two things it does
  **share** are reproduced: (1) **immediates** (`is_immediate`) are returned as-is; (2) **literal-area
  terms** (a boxed/list word with the `TAG_LITERAL_PTR` bit, tested *only* on a genuine term word — see
  the non-term guard below) are returned as-is; the zero-arity tuple (`HEADER_ARITYVAL` with arity 0) is
  the shared `beam_term::tuple0()` literal and is returned as-is.
- **Non-term and cyclic graphs are typed errors, never wrong results or hangs.** The `is_shared` test
  requires a genuine term word before checking the literal bit (`is_boxed_term(term)`, or
  `is_list(term) && is_term_word(term)`), because `beam_term::is_boxed` mirrors the C bit trick that is
  also true for a bare `HEADER` word: without the guard a bare header word (e.g. `HEADER_SUB_BITS`, or
  `THE_NON_VALUE`) would be mis-shared as a literal and cross the boundary undetected. A non-term now
  reaches `shallow`'s `PrimaryTag::Header` arm ⇒ typed `BadArg`, writing nothing.
- **Allocation-failure classification: `AllocKind::Message` is the message-heap contract.** A copy that
  does not fit its destination is `BeamError::Alloc(AllocFailure::new(AllocKind::Message, words*8))`,
  exposed as the public predicate `is_message_heap_exhaustion`. It is **soft/recoverable**: a host may
  grow-and-retry, or raise `error:system_limit` (exception class `error`, reason atom `system_limit` —
  the atom the oracle raises for an exceeded hard runtime limit, verified against OTP 29.1.1). The kind
  stays `Message` (not `Heap`) deliberately: `beam_rt::RuntimeContext::deliver_message` keys on raw
  `AllocKind::Message` as the load-bearing oversized-message **retry trigger**, and the destination of
  this copy *is* the message/receiver heap. See the sibling-adoption note in Known Issues.
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
  `BinRef` — the fix-up an opaque-leaf treatment of a `SUB_BITS` would leave dangling. An on-heap
  **`ErlHeapBits`** small binary has no navigable child and is copied verbatim (its words are opaque).
  A **match context** `SUB_BITS` (its `orig` is *not* a `BinRef`, i.e. an on-heap bitstring) is instead
  **flattened** by `copy_match_context` into a fresh on-heap bitstring for the window `start..end`
  (`copy.c:849-863`), never copied verbatim, so the copy carries no `base_flags`/`orig` pointer into
  the source heap; `beam_term::layout::object_words` sizes it `heap_bits_size_words(end - start)` and
  the sizing child-slot enumerator yields no child, so the preflight agrees with the copy.
- **A refc binary with no owning handle flattens, exactly like a match context.** When the source
  table holds no handle for the payload pointer, `copy_refc_binary` has nothing to share, so it
  delegates to the very same `copy_match_context` and returns an on-heap bitstring of the window. That
  needs no change to the flattener: `write_refc_binary` sets the pair's `base_flags` word to the
  payload's data base (the same value `BinRef.val` holds), so `sub_bits_parts`/`bit_at` read the window
  `start..end` verbatim and the result's data lands inside the destination. Nothing is registered in
  the destination table (there is no handle to refcount). Because the copy then costs
  `heap_bits_size_words(size)` rather than the 8-word pair, `bounded_words` decides a refc binary's
  cost from the **source** table (`refc_binary_words`) — the preflight is not independent of the
  `CopyContext`.
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
  is faithful even though the mechanism (copy + release) differs. Ownership is given up only after the
  **whole chain** has copied, so a mid-move failure leaves every fragment intact and the move is
  retryable. `send_fragmented` is the C `erts_try_alloc_message_on_heap` sender: it tries the
  receiver-heap copy first and fragments only on `AllocKind::Message`; the fragment (sized to the
  whole payload, C `new_message_buffer(sz)`) then owns the off-heap objects while the message lives.
- **The fragment's backing `Heap` never reallocates, so a root's address is stable across a move of
  `Fragments`.** The envelope holds `Fragments` by value, but the heap's region is a separate,
  never-moving allocation, so `Message::payload` (which points into the region) stays valid as the
  `Message` — and therefore the `Fragments` — is moved around.
- **A refc binary whose payload no held handle owns is flattened by value.** The M1 encoding has no
  `Binary*` back-pointer to recover the handle from (see the Design Decision above), and a silent deep
  copy is not an option either — it would need a payload length the copy cannot trust. Flattening is
  the faithful reading: the term *is* an `ErlSubBits` window over a byte base, and the window's bits are
  all the copier needs. It costs the destination `heap_bits_size_words(size)` words instead of 8, which
  is why the preflight is context-aware.
- **Per-object sizing comes from the LCA; the whole-graph count is the copier's own.**
  `beam_term::layout::object_words` (the frozen term layout's owner, re-exported by `layout.rs`)
  gives the *single-object* word count the incremental allocator needs — the same split as
  `beam_gc::copy`'s `move_size` — and `preflight_words`' `bounded_words` mirrors it (walking the
  child slots with the LCA's `for_each_sizing_child_slot`) to give the whole-graph count the copier
  will actually write; the crate keeps **no** local copy of the walk. `beam_term::size_object`
  (`size_object_x` port) remains the canonical flat count and equals `bounded_words` for every shape
  that can cross a copy boundary (incl. a `SUB_BITS` match context, whose `object_words` is the
  flattened window size) — the one exception being a refc binary the source table holds no handle for,
  whose copy cost is the flattened window rather than the 8-word pair. The LCA's `object_words` and
  child-slot enumerators cover every HAMT node
  kind with the same `1 + header_arity + popcount(MAP_HEADER_VAL)` formulation, so a hash map copies
  as a whole trie. Because that single-object count *is* the LCA's `object_words`, a widened fun
  descriptor (the 3-word `External` `[module, function, arity]` descriptor ⇒ a 4-word block) is sized
  and copied automatically, with no change in this crate.
## Test Strategy
- In-module `#[cfg(test)] mod tests` per module: `layout` (the off-heap container round-trip and the
  non-container negative case — the per-object sizing/child-slot walk itself is tested in
  `beam_term::layout`), `off_heap` (slot
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
- `tests/classes.rs` — **term-class completeness for the off-heap-free classes**: immediates; proper
  and improper cons lists; tuples (nested + the shared zero-arity literal) and a native `RECORD_SUBTAG`
  record; flat map and HAMT map (head-bitmap/inner-node); all three fun kinds (the External case pins
  the widened 4-word `[header, module, function, arity]` block, incl. the true-arity word); on-heap
  bitstrings (byte-aligned + non-byte-aligned). The off-heap classes are covered by their own files:
  refc binary → `binary.rs`, MSO container → `copy.rs`/`sizing.rs`, external pid/port/ref →
  `external.rs`. A boxed `HEADER_FLONUM` float, a positive/negative bignum and a **HAMT collision tuple** are also
  covered here (each copy asserted `compare::eq` with a distinct address and an exact
  `dst.used() == size_object(term)`; the collision node is hand-laid with `make_map_header` + a
  collision tuple, since `write_map` cannot force a 64-bit hash collision).
- `tests/guards.rs` — **non-term and cycle guards + the relocation invariant**: `is_shared`/`shallow`
  reject every bare header word (incl. `THE_NON_VALUE`/`HEADER_SUB_BITS`) with a typed `BadArg` and
  write nothing; a forged cyclic graph (self-cons, self-tuple, 2-cell, 100-cell) returns a typed
  `BadArg` promptly at both tiny and huge destinations (and for `copy_struct_shared`) — never a hang;
  a deep **acyclic** graph is refused only by *capacity*, never mis-typed; and a walk of a copied graph
  proves **every** non-shared compound subterm lies inside the destination heap region (nothing still
  points into the source) — the corpus includes a `SUB_BITS` match context, which the copier flattens
  to a fresh on-heap-bitstring leaf the walk asserts lives in the destination.
- `tests/sizing.rs` — **sized-then-copied exactness**: for a 20-shape corpus (small tuple, deep list,
  improper list, flat map, 64-key HAMT map, nested graph, Local/External fun, native record, byte-aligned
  and 13-bit non-byte-aligned bitstrings, boxed float, 2-digit bignum, `SUB_BITS` match context, MSO
  container, refc binary, a graph with a shared refc binary, external pid, external port, external
  reference), a destination of exactly `size_of_copy(term)` words accepts the copy with
  `to.used() == size_object(term) <= capacity`, and a one-word-short (and zero-word) destination is
  refused with `Alloc(Message)` and `used() == peak_used() == 0` (no partial write, and no off-heap
  artifact — the MSO handle, refc-binary AND node lists all empty); immediates/`tuple0()` copy into a
  zero-capacity heap by identity. `a_one_word_short_destination_is_refused_before_any_write` is the
  **wrong-size-copy detector**.
- `tests/single_pass.rs` — the clean-destination **single-pass** contract: a faithful copy (vs the
  preflighted path) with `dst.used() == size_of_copy(term)` for several shapes; a clean one-word-short
  destination still refuses as a total no-op (`AllocKind::Message`, `used() == peak_used() == 0`, no
  off-heap artifact — send atomicity preserved); a refused clean copy gives back the off-heap references
  it took (source refcount back to 1); a forged cyclic graph / non-term on a clean destination is still a
  typed `BadArg` (never a hang or a wrong result); and a DIRTY destination (pre-charged heap or non-empty
  off-heap table) still takes the sized path untouched.
- `tests/message_size.rs` — the logical-size surface: `heap_words()` == `size_object(payload)` for a
  heap-resident message and == `fragments().total_used_words()` for fragment-resident ones (built by both
  `send_fragmented` and `HeapFragment::copy_root`); `logical_binary_bytes()` == the shared refc payload's
  length (and `0` with none, and `0` again for a **fragmented** message, whose handle belongs to the
  fragment's table until it is moved in) while `heap_words()` counts only the 8-word handle;
  `with_payload` preserves `from`/`token`/`off_heap`/`binaries`/`nodes`/`fragments`.
- `tests/envelope_relocation.rs` — the envelope's GC-rooting (relocation) surface: `set_from`/`set_token`
  replace **exactly** the two term words and leave the payload, the off-heap handle, the refc binary, the
  node reference and the fragment chain untouched (asserted through the accessors, incl.
  `HeapFragment::contains` on the payload); `with_from` re-targets the sender and keeps the rest; a
  host-style gather/relocate/scatter round-trip moves `payload`/`from`/`token`; the debug-only enqueue
  tripwire (`queue.enqueue`) panics on a non-immediate `from`, a non-immediate `token` and a bare
  `THE_NON_VALUE` word, and a well-formed immediate envelope enqueues and reads back unchanged.
- `tests/share.rs` — the sharing-preserving evidence: (a) a `1000`-element subterm repeated `N = 8`
  times ⇒ the shared destination uses `2N + 2K` words vs the flat `2N + N·2K` (a >7× saving) and every
  occurrence shares one destination address while the flat copy's are all distinct; (b) the shared copy
  is still `compare::eq` the original; (c) a graph with **no** sharing copies to exactly the flat size
  (`dst.used() == size_of_copy(original)`); (d) a shared off-heap MSO container — and a shared **refc
  binary** — is refcounted once per source identity (one destination entry/handle, one new reference)
  against `N` entries for the flat copy.
- `tests/binary.rs` — refc-binary sharing: one destination binary, refcount 2 (source + destination),
  the same bytes and the same `data_ptr()`, the copied `BinRef.val` == the source payload pointer, the
  copied `ErlSubBits.orig` reaching the copied `BinRef`, `is_refc_binary`/`refc_bytes`/`refc_offset`/
  `refc_size` on the copy, `dst.used() == ERL_REFC_BITS_SIZE`, dropping the destination table returning
  the source refcount to 1; two occurrences sharing one payload; a refc binary with no owning handle
  **flattening** to an on-heap `HEAP_BITS_SUBTAG` bitstring of the whole window, byte-equal to the
  source payload, data inside the destination and outside the source heap and payload, registering
  nothing (and a one-word-short dirty destination still refused as `AllocKind::Message`, sized by the
  flattened block, with no partial write); and an on-heap `ErlHeapBits` small binary copied by value
  (equal bytes, distinct address,
  `heap_bits_size_words` words); and a `SUB_BITS` **match context** over an on-heap bitstring is
  **flattened** — the copy is a fresh on-heap bitstring (`HEAP_BITS_SUBTAG`, not a `SUB_BITS`) whose
  bits equal the source window (a non-byte-aligned `start` exercises the bit shift) and whose data base
  lies inside the destination region, not the source.
- `tests/external.rs` — external thing copy: words verbatim and the node atom/creation preserved (via
  `external_*_parts`), one node entry per distinct node, two references to the same node ⇒ one entry
  with refcount 2, port and reference nodes registered separately, and dropping the destination
  releasing the node, plus an exact `dst.used() == size_object(term)` (`== external_*_size_words()`)
  assertion on the pid and on the port/reference tests. `compare::eq` is deliberately **not** used (see Known Issues).
- `tests/fun.rs` — a **closure crossing a copy boundary** is sized/walked from the frozen
  `beam_term::fun` layout (`env[0]` at `1 + desc_words(kind)`, block `1 + desc_words(kind) + num_free`),
  never the legacy C `ERL_FUN_SIZE`: a 0-free-variable (`make_fun`) closure and a 5-free-variable
  (`make_fun3`) closure with boxed free variables copy to exactly their block + environment graphs
  (`dst.used() == fun_words(Local, n) + size_object(env graphs)`, no trailing/duplicated word), compare
  `=:=` to the original, encode to byte-identical `NEW_FUN_EXT`, still resolve to a callable lambda
  (a test lambda table stands in for `beam_engine`'s, since the wave DAG forbids
  `beam_message → beam_engine`), and `env[0]` is fixed up **into the destination heap** while the raw
  `uniq`/`index` descriptor words are copied verbatim. The same frozen layout makes an `External`
  (`EXPORT_EXT`) fun the 4-word block `fun_words(External, 0) == 4` (`fun_desc_words(External) == 3`,
  descriptor `[module, function, arity]`), copied verbatim — true-arity word included, read back with
  `fun_true_arity` (the External case itself is exercised in `tests/classes.rs`).
- `tests/fragments.rs` — the message-fragment evidence: a payload larger than the receiver heap makes
  plain `send` fail with `AllocKind::Message` **and leave `to_heap.used() == 0` and the destination
  table empty**; the same `CopyContext` then drives `send_fragmented` (proving the retry needs no
  cleanup) into a structurally-equal payload that points into the fragment (`HeapFragment::contains`)
  and owns the `Fragments`; `move_into` moves the payload onto the receiver heap and transfers the
  fragment's off-heap objects (MSO slot, a refc-binary handle — same `data_ptr`, refcount 2 =
  source + receiver — and an external thing's **node reference**, which the receiver's table then holds
  as its own entry while the fragment's is released); a multi-fragment chain moves roots in chain order
  with consistent accounting;
  fragments of a **refc binary**, a **40-key HAMT map** and a **`make_fun3` closure** all move off the
  fragment and stay `compare::eq`; `a_failed_move_into_leaves_the_chain_intact` pins the
  ownership-after-whole-chain rule; a fragment too small for the payload rejects before writing.
- `tests/queue.rs` — `send`/`receive` + FIFO queue integration + the `QueueCursor` scan/remove path;
  `a_rejected_send_is_a_total_no_op` pins send atomicity.
- `tests/alloc.rs` — the classification contract: a real `copy_struct`/`copy_struct_simple` into a
  too-small `beam_alloc::Heap` yields `Alloc(Message)` with `requested == words * size_of::<Eterm>()`
  and leaves the heap untouched (`used() == 0`); `is_message_heap_exhaustion` accepts it and rejects
  `BadArg`, the other `AllocKind`s and non-`Alloc` variants; the flat-word-count boundary is pinned.
- `tests/perf.rs` — the committed, dependency-free **copy-cost measurement** (see Performance below),
  including the **preflight-share probe** (clean vs pre-charged destination); asserts correctness only,
  never a timing threshold.
- Gate (GREEN required): `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message`
  and `erts_rust/scripts/cargo.sh build --manifest-path erts_rust/Cargo.toml --workspace --all-targets`.
  Always go through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never touch `../../tools/`.
- Cross-checks against the real system ERTS belong to the sibling `../../tools/differential/` harness,
  not here; the full gate stays `mismatch=0`. This crate's own tests are **all self-checks**
  (structural `compare::eq`, word-count arithmetic, refcounts) — no test in `src/` or `tests/`
  invokes `erl`/`erlc`.
- The workspace `cargo test` gate **runs in CI**: `.github/workflows/erts_rust.yml` (path-triggered on
  any `erts_rust/**` change or PR) builds the workspace (`cargo build --workspace --all-targets`) and
  runs `cargo test --workspace --no-fail-fast`, a non-blocking clippy, and the differential gate
  (`erts_rust/tools/differential/gate.sh`).
## Performance (measured)
`tests/perf.rs` (dependency-free `std::time::Instant`, `--nocapture`; dev/`opt-level=1` profile,
20 000 iters, HAMT 5 000). Repro: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message --test perf -- --nocapture`.

| shape | words | objects | ns/iter | ns/word | ns/object |
|---|---|---|---|---|---|
| small tuple | 4 | 1 | 61.5 | 15.4 | 61.5 |
| deep list (2000 cons) | 4000 | 2000 | 37 103 | 9.3 | 18.6 |
| flat map (8 keys) | 20 | 1 | 172 | 8.6 | 172 |
| HAMT map (64 keys) | 234 | 65 | 2 157 | 9.2 | 33.2 |
| refc binary reuse (512 B) | 8 | 1 | 61.3 | 7.7 | 61.3 (payload shared) |

Per-word cost is flat across shapes (~7.7–9.3 ns/word; the homogeneous cons chain is **not** the
worst). (Sub-100 ns fixed cost for one object dominates the small shapes.)

**The clean-destination copy is now single-pass.** A `copy_struct` into a clean destination (an empty
heap *and* a fully empty off-heap table) runs materialise-then-commit (`try_copy_fast`) and skips the
sizing walk entirely; a dirty destination still preflights before writing. The `preflight-share` probe
in `tests/perf.rs` measures the saving as `best_ns(clean)` vs `best_ns(pre-charged)` (best-of-100 000,
dev/`opt-level=1`, host at `/proc/loadavg` 199–286): small tuple (4 words) `best_ns` **78 → 62**
(pre-charged 74.5), a Pass-1 share of ≈ 13 ns (~17 %); flat map, 8 keys (20 words) `best_ns`
**155 → 101.5** (pre-charged 151.5), a Pass-1 share of ≈ 48.5 ns (~32 %). `tests/perf.rs` asserts
correctness only, never a timing threshold; absolute numbers are inflated under the
shared/oversubscribed host — prefer ratios.
## Known Issues / Intentional M1 gaps
- **`copy_struct` is two-pass only for a DIRTY destination; a clean one is now single-pass.** The
  Stage-3 "bounded single-pass small copy" from `../ERTS_RUST_REWRITE_DEEPDIVE.md` §D/§H and
  `../ERTS_RUST_REWRITE_EVALUATION.md` §D/E is landed for the clean case (`try_copy_fast`). Generalising
  it to a dirty destination needs an **off-heap checkpoint API** — `OffHeapTable::rollback` drains all
  three lists (so it would discard pre-existing entries) and there is no
  `OffHeapTable::checkpoint()`/`rollback_to(...)`; a `beam_alloc::Heap::truncate(mark)` would only help an
  alternative design that writes straight into the heap. Both live outside this crate. The other Stage-3
  item, **message-queue auto-tuning** (replacing the static `SPO_ON_HEAP_MSGQ`/`+hmqd` on-/off-heap choice
  through `erts_change_message_queue_management`), is a per-process placement flag — a `beam_process`/
  `beam_rt` concern this crate does not model (its `send` vs `send_fragmented` choice is the placement
  decision itself). In `../tools/bench` the only `Category::Messaging` ("messaging/copy-bound") workload
  is `emqx`, which reports `skipped(emqx-requires-otp-28)`; the runnable copy-cost signal is
  `tests/perf.rs`.
- **`beam_term::compare` has no structural arm for external pid/port/reference (nor for
  `RECORD_SUBTAG`).** It falls through to the deterministic `fallback_cmp` (same class ⇒ order by
  object arity, then raw term word), so a copy is never `=:=` its original for those two shapes and
  `compare::eq` cannot be used to check a copied external thing / native record. The crate's tests
  assert structural equality through the `beam_term::thing` accessors (`external_*_parts`,
  `record_*`) instead; a fix belongs to `beam_term`.
- **`beam_term::size_object` is unbounded on a cyclic/forged graph** (and reports `0` for
  `THE_NON_VALUE` without panicking). The `beam_message` boundary is guarded by the cycle-safe
  `preflight_words`, but any other caller sizing a term with `size_object` (beam_gc, beam_rt, tools) is
  exposed; a bound/cycle guard belongs in `beam_term` (`src/size.rs`).
- **A flattened no-handle refc binary becomes an on-heap `ErlHeapBits` that may exceed
  `ERL_ONHEAP_BITS_LIMIT`.** `copy_match_context` writes the window with `write_heap_bits` and does not
  enforce the on-heap limit (the C's `HEAP_BITSTRING` only asserts it), so a long off-heap payload
  whose handle no source table holds lands as an on-heap bitstring larger than ERTS would ever build
  there. The copy stays correct and self-contained — the bits and their data are in the destination,
  nothing aliases the source — but the term's shape is not one the emulator produces. A caller that
  cares keeps the payload's handle registered in the off-heap table, which takes the ordinary
  share-the-payload path.
- **No sharing-preserving copy for the flat path.** `copy_struct` duplicates a repeated subterm
  (C's default); `copy_struct_shared` is the opt-in alternative. Only immediates, literal-area terms
  and the zero-arity tuple are shared outright.
- **The node identity is an atom term, not a registry pointer.** There is no node registry in the
  workspace, so [`NodeEntry`] keys `(node atom, creation)` and its count is per-table structural
  bookkeeping (see Design Decisions); a real registry would replace the atom with an `ErlNode *` and
  make the count global across processes.
- **`copy_struct_shared` preflights the *flat* word count.** It is a safe upper bound, so a caller sizes
  the destination heap as for `copy_struct`; the memory saving is in the words written, not in the
  accepted capacity.
- **The fragment path is driven by `beam_rt`, and untested.** `beam_process::deliver` still calls
  `send` (a payload over its heap ⇒ `AllocKind::Message`); `beam_rt`'s `send_fragmented_message`
  (`src/context/mod.rs`) catches that and calls `beam_message::send_fragmented` through the split
  borrow `beam_process::Process::heap_and_mailbox_mut` (`process.rs`), enqueuing a message owning the
  chain. No `beam_rt/tests/*` nor differential case exercises fragmentation, and a received message's
  `Fragments` are never `move_into`'d (`beam_rt`'s `MailboxState` retains them instead).
- **A fragment holds the whole payload; there is no automatic multi-fragment split.** The C streams an
  oversized message across a chain of `new_message_buffer` fragments; here one fragment is sized to the
  whole payload (`fragment_capacity_words.max(size_object(term))`) and the chain is built by the caller
  (`Fragments::push`). A size-bounded split is `beam_process`'s heap-management concern.
- **`beam_process` has no fragment-aware message-buffer model.** `HeapFragment`/`Fragments`/
  `send_fragmented` support a no-partial-write oversized-message retry; but `beam_rt`/`beam_process` do
  not yet hold a received message's `Fragments` as its message-buffer model nor call
  `Fragments::move_into` to move a payload onto the process heap — a host must.
- **Sibling adoption needed (reported to the parent, `beam_rt` NOT edited here).** `beam_rt::error::enforce_term_heap_limit`
  (beam_rt/src/error.rs) translates only `AllocKind::Heap` → `SystemLimit`, so `Runtime::stabilize_term`'s
  message-heap exhaustion surfaces the internal `allocation failure: message (N bytes)` diagnostic
  instead of `error:system_limit`. It should also map this crate's message-heap exhaustion — via
  `beam_message::is_message_heap_exhaustion` — to `BeamError::SystemLimit`. **Ordering hazard:** the
  translation must NOT be applied before `deliver_message`'s raw `AllocKind::Message` retry trigger (it
  isn't today: only `stabilize_term` routes through `enforce_term_heap_limit`).
- **Scheduler coupling is absent by design.** `Queue` is a pure FIFO; selective-receive waiting,
  `after`, the save queue, signal/priority ordering and the message-queue length semantics of
  `ErtsSignalPrivQueues` are `beam_process`'s.
- **The logical-size surface is exposed but nothing charges it yet.** `Message::heap_words()` supplies
  the sender's copy-charge input (the C `msize = size_object_litopt(message)`; charge
  `4 + msize / ERTS_MSG_COPY_WORDS_PER_REDUCTION` (64 release) capped at `CONTEXT_REDS` — `bif.c:2742`/
  `:2862`, `erl_message.c:881-886`) and `Message::logical_binary_bytes()` the *holder's* virtual
  binary-heap input (`bin_vheap += orig_size / sizeof(Eterm)`, surfaced as `virtual_reds` —
  `erl_gc.c:3046,3051`, `erl_process.c:9678`); a host must apply both. Charging the holding process's
  virtual binary heap remains a separate mechanism this crate does not model. `message_queue_len` (the
  message COUNT) and `message_queue_data` (on_heap/off_heap) are unaffected.
- **`Message.token` is a plain field (`NIL` when absent).** The seq-trace / distribution-token
  machinery that fills it is `beam_dist`/`beam_rt`.
- **Cross-crate handoff: the host must gather `from`/`token` and scatter them back.** The relocation
  surface (`with_payload`, `set_from`/`with_from`, `set_token`) and the enqueue tripwire live here, but
  the *collection walk* that drives them is the host's — `beam_process::Process::run_rooted_collection`
  (`src/process.rs`): its gather step (~lines 949-956) pushes **only** each queued message's
  `payload()`, and its scatter step (~lines 980-983) re-installs the envelope with `with_payload`
  alone, so a heap `from`/`token` is never relocated. The host must also push `message.from()` /
  `message.token()` as roots and write them back through `Message::set_from` / `Message::set_token`.
  `beam_process`'s own producers all store immediates (its `CONTEXT.md` pins each), so this is the
  general hardening, not a live corruption today; a caller that hands `Process::deliver` an on-heap
  `from` is the residual case the host gather closes.
- **The binary/node lists are carried but not yet consumed.** `beam_process`/`beam_rt` must own a
  received message's `binaries()`/`nodes()` (they are the receiving process's off-heap references) —
  today nothing sweeps them, the `Message`'s own `Drop` releases them. A **fragmented** message instead
  leaves those lists empty: its payload's off-heap objects live in the fragment's `off_heap` table until
  `Fragments::move_into` hands them to the destination table.
- **Cross-crate: a received message's fragments/binaries/nodes survive a mailbox rebuild (sibling
  `beam_process`).** `beam_process::Process::run_rooted_collection` re-installs each queued message with
  `Message::with_payload` (`src/process.rs`), which keeps the fragment chain and the
  off-heap / refc-binary / node lists. The one remaining gap in that rebuild is the `from`/`token`
  gather (above): the root set holds only the payload word, so the host must add the two term words and
  scatter them back with `set_from`/`set_token`. `beam_rt`'s `MailboxState` only retains fragments for
  messages it removes.
## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude` (incl. `is_message_heap_exhaustion`)
- `src/copy.rs` → the iterative `copy_struct` engine (the `unsafe` core) + `CopyContext` + the cycle-safe, context-aware `preflight_words`/`bounded_words`/`refc_binary_words`/`is_acyclic` + `size_of_copy` + the refc-binary and match-context flattening (`copy_refc_binary`/`copy_match_context`) + the clean-destination single-pass path (`CopyTarget`/`TrialTarget`, `is_clean_destination`, `try_copy_fast`)
- `src/share.rs` → the sharing-preserving `copy_struct_shared` (source-identity memo over `copy.rs`'s `shallow`; preflights via `copy::preflight_words`)
- `src/fragment.rs` → `HeapFragment` (bump region + off-heap table + roots) and `Fragments` (the chain + `move_into`, ownership released only after the whole chain moves); the `ErlHeapFragment` message-buffer model
- `src/layout.rs` → the beam_message-specific M1 **identification + encoding** helpers for the copier: off-heap container (`is_off_heap_container`/`is_off_heap_term`/`off_heap_slot`/`make_off_heap_container`), refc binary (`is_refc_binary_term`), match context (`is_match_context_term`) and external thing (`is_external_header`/`external_node_creation`). Per-object word counts and child-slot enumeration are **not** defined here — the canonical walk (`object_words`, `for_each_child_slot`, `for_each_sizing_child_slot`) is re-exported (`pub(crate) use`) from the LCA `beam_term::layout`
- `src/off_heap.rs` → `OffHeapTable` (MSO-typed façade over slot-indexed `beam_gc::OffHeap`, plus the refc-binary and node lists); MSO handle re-export + the `pub(crate)` `is_fully_empty`/`rollback` used by the single-pass rollback
- `src/node.rs` → `NodeEntry`, the refcounted `(node, creation)` external-thing node identity
- `src/message.rs` → `Message` envelope (payload/from/token + off-heap, binary and node lists + optional `Fragments`) + the envelope relocation surface (`with_payload`, `set_from`/`with_from`, `set_token`) + the `Queue` FIFO with its debug-only enqueue tripwire (`debug_assert_enqueue_terms`) + the logical-size surface (`heap_words`/`logical_binary_bytes`)
- `src/mailbox.rs` → `send` / `send_fragmented` / `receive` / `receive_ref` data-level primitives
- `src/error.rs` → `pub(crate)` copy-failure constructors + the public `is_message_heap_exhaustion` predicate (no new error type)
- `tests/copy.rs` → flat-copy correctness: nested-graph equality, deep list, literal sharing, MSO refcount, exhaustion, HAMT trie
- `tests/classes.rs` → term-class completeness across every copyable class
- `tests/guards.rs` → non-term/cycle guards + the destination-relocation invariant (incl. a `SUB_BITS` match context)
- `tests/sizing.rs` → sized-then-copied exactness (incl. the `SUB_BITS` match-context shape) + too-small-reservation refusal
- `tests/single_pass.rs` → the clean-destination single-pass contract (faithfulness, no-op refusal, refcount rollback, cycle→BadArg, dirty path untouched)
- `tests/message_size.rs` → the logical-size surface (`heap_words`/`logical_binary_bytes`, incl. a fragmented message's `0`) + `with_payload` envelope preservation
- `tests/share.rs` → sharing-preserving evidence: N-fold subterm, structural equality, no-sharing size, shared MSO/refc-binary identity
- `tests/binary.rs` → refc-binary payload sharing (refcount, `data_ptr`, `orig` fix-up, size) + on-heap small-binary copy-by-value + `SUB_BITS` match-context flattening (no source aliasing) + the no-owning-handle refc binary flattening and its flattened-size preflight
- `tests/external.rs` → external pid/port/ref verbatim copy + node linkage/refcounting
- `tests/queue.rs` → `send`/`receive` + FIFO queue integration + the `QueueCursor` scan/remove path + send atomicity
- `tests/envelope_relocation.rs` → the envelope relocation surface (`set_from`/`set_token`/`with_from`) + the debug-only enqueue tripwire
- `tests/fun.rs` → closure copy correctness: `make_fun`/`make_fun3` blocks sized/walked from the frozen `beam_term::fun` layout, `=:=` equality, byte-identical `NEW_FUN_EXT`, callability, `env[0]` fix-up
- `tests/fragments.rs` → the fragment-model evidence: `send` vs `send_fragmented` (atomicity), `move_into` (payload onto the receiver heap + off-heap transfer, incl. an external thing's node reference), the multi-fragment chain, refc-binary/map/fun fragment moves, chain-intact-on-failure, accounting
- `tests/alloc.rs` → the allocation-failure classification contract (`AllocKind::Message` + predicate + no partial write)
- `tests/perf.rs` → the dependency-free copy-cost measurement (ns/word, ns/object)
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
