# `beam_message` / `src` — the copy engine and messaging modules

## Intent
The implementation layer of `beam_message`: eleven modules, one responsibility each, owning the iterative copy engine (`copy_struct`/`copy_struct_shared`), the capacity-pricing walk, the off-heap reference table, the `Message` envelope with its FIFO `Queue`, the message-fragment model and the send/receive data-level primitives.
This node exists so the crate `CONTEXT.md` can stay a *seam* document: the crate root keeps the contract a dependent designs against, this node keeps the per-module surface, mechanism and legacy-C provenance.
Read `../CONTEXT.md` first — Intent, the frozen inheritance from `../../CONTEXT.md`, Constraints and the GC-rooting rules are stated there and are **not** restated here.

## API Surface (per module)
`copy.rs`, `preflight.rs`, `share.rs`, `fragment.rs`, `message.rs`, `mailbox.rs`, `off_heap.rs`, `node.rs` and the `lib.rs` re-export list are the crate's public seam (summarised in `../CONTEXT.md`); `layout.rs` and `error.rs` are `pub(crate)` apart from `is_message_heap_exhaustion`, and `copy_reference.rs` is `#[cfg(test)]`-only.
`lib.rs` re-exports at the crate root and in `prelude`: `copy_struct`, `copy_struct_simple`, `size_of_copy`, `CopyContext`, `is_message_heap_exhaustion`, `Fragments`, `HeapFragment`, `receive`, `receive_ref`, `send`, `send_fragmented`, `send_fragmented_after_refusal`, `Message`, `Queue`, `QueueCursor`, `NodeEntry`, `MsoHandle`, `OffHeapTable`, `shrink_warm_scratch`, `copy_struct_shared`.

### `copy.rs` — the copy engine (the `unsafe` core)
- `pub struct CopyContext<'a>` — the copy boundary: the source process's off-heap table (resolving off-heap references) and the destination's (re-registering them).
  `CopyContext::new(src: &'a OffHeapTable, dst: &'a mut OffHeapTable)`; `src_off_heap(&self)`, `dst_off_heap(&self)`, `dst_off_heap_mut(&mut self) -> &mut OffHeapTable` (the table `send` drains into the message). Exactly two fields — no source base/extent/heap identity.
- `copy_struct(term, to: &mut Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm>` — iterative **flat** copy of `term`'s graph into `to` (an explicit fix-up worklist, no native recursion, so a deep list cannot overflow the stack), returning the copied root; shares immediates and literal-area terms, copies every other class.
  **Single-pass** (`try_copy_fast`) whenever the destination off-heap table is empty (a dirty *heap* included) — one copy traversal and no preflight at all; otherwise `preflight_words` prices the graph in exactly **one** unbounded walk before any write, the count is exact, and the *caller* compares it (`copy_struct` refuses when `to.available() < needed`; the retained `limit` parameter is not consulted), so a refusal writes nothing.
- `copy_struct_simple(term, to: &mut Heap) -> Result<Eterm>` — `copy_struct` for empty off-heap tables (two throwaway tables; see the Notes rules in `../CONTEXT.md`).
- `size_of_copy(term) -> usize` — forwarder to `beam_term::size_object`, the flat **source** word count (the `litopt == NULL` answer), so a caller can size a destination. It equals the copier's own destination price for a graph with no literal-area subterm and no refc binary whose payload no source handle owns; the copier itself prices through `preflight_words`, never through this.
- `pub(crate) trait CopyTarget` + `struct TrialTarget` + `is_fast_path_eligible` / `try_copy_fast` — the destination abstraction: `Heap` reserves per object; `TrialTarget` bumps over the destination heap's unused tail `[to.hp(), hend)` (`free = to.available()`) counting `written` without touching `used`/`peak`; `is_fast_path_eligible` = `ctx.dst_off_heap().is_fully_empty()`.
- `pub(crate) fn shallow` / `shallow_unshared` — the shallow half (materialise one object, record a fix-up per non-shared child). `shallow` is the checked entry (the two root calls); `shallow_unshared` is the match body the worklist loops call, sound because a fix-up is only ever recorded for a child `push_fixup` already proved non-shared — the two must stay behaviourally identical for every term `is_shared` rejects.
- `pub(crate) fn is_shared(term) -> bool` / `identity(term) -> Option<usize>` — the one shared-term test per copied object and the from-space address a sharing memo keys on; `pub(crate) type Fixup = (*mut Eterm, Eterm)`.
- `copy_refc_binary` / `copy_match_context` — the two flattening paths (per-shape rules: `../CONTEXT.md` Design Decisions).

### `preflight.rs` — the destination-pricing gate on a warm per-thread scratch
- `pub(crate) fn preflight_words(term, limit, ctx) -> Result<usize>` — prices `term` for a copy into `ctx`'s destination in **one** unbounded walk over **this thread's** warm `beam_term::size::SizingScratch`; the figure is exact in both directions (a graph that fits *and* one that does not both come back with the true price), a forged cycle is the typed `BadArg`, and `limit` is not consulted (the caller compares). The scratch is borrowed **only for the walk**, so the copy that follows never runs with it held; a re-entrant borrow (or a destroyed scratch at thread teardown) runs the identical walk on a locally built `SizingScratch::new()` instead of panicking. The sole pricing entry point for `copy_struct` and `copy_struct_shared`.
- `pub(crate) fn preflight_words_with_scratch(term, limit, ctx, scratch: &mut SizingScratch) -> Result<usize>` — the real body over a caller-owned scratch (the cold/warm measurement uses it).
- `pub fn shrink_warm_scratch()` — re-exported at the crate root and in `prelude`: releases this thread's retained scratch buffers (`SizingScratch::shrink_to_fit`). Results are unaffected — the next preflight re-grows them — so it is a host's RSS lever after an outlier payload, and the cold-state control `tests/traversals.rs` measures with.
- `struct CopyPricing<'a>` + `impl SizingPolicy` (private) — the copier's own cost model: `is_shared` → `crate::copy::is_shared` (a literal-area subterm costs 0 words and is not descended into); `object_words` → `refc_binary_words` for the one shape `beam_term::layout::object_words` prices for the *source* heap while the copy writes something else.
- `pub(crate) fn refc_binary_words(t, ctx) -> usize` — the 8-word `[BinRef][ErlSubBits]` pair when the payload is owned by a handle in the source table, else the flattened window `heap_bits_size_words(end - start)`. It lives beside the flag it mirrors so preflight and copy never disagree; the retired reference walk prices through it too.

### `copy_reference.rs` — the test-only pricing oracle
`#[cfg(test)]`-gated: the bounded **reference** walk (`bounded_words_reference`/`is_acyclic_reference`/`preflight_words_reference`) the shipped policy walk is pinned against, plus the five equivalence tests. Its only consumer of `beam_term::layout::for_each_sizing_child_slot` is here, which is why `layout.rs` does not re-export the sizing enumerator.

### `share.rs` — the sharing-preserving copier
`copy_struct_shared(term, to, ctx) -> Result<Eterm>` — `copy_struct`'s immediate/literal/off-heap-node policy and iterative worklist, but each compound subterm is materialised **once per source identity** through a `HashMap<usize, Eterm>` memo keyed on the from-space term address (immediates and literal-area terms are never memoised).
It preflights through the same `preflight::preflight_words`, so the destination is sized as for `copy_struct` — the saving is in the words written. It reuses `copy.rs`'s `shallow` primitive, so the copy logic exists once.

### `fragment.rs` — the message-fragment model (`ErlHeapFragment`/`ErtsMessage` analogue)
- `pub struct HeapFragment` — a bump-allocated heap region plus its off-heap objects and its roots, mirroring the C `ErlHeapFragment` (`{ next; off_heap; alloc_size; used_size; mem[1]; }`).
  `new(capacity_words)` / `with_allocator(alloc, capacity_words)`; `heap()`/`heap_mut()` (the `mem[1..alloc_size]` region); `off_heap()`/`off_heap_mut()`; `used()`/`capacity()`/`available()`; `off_heap_overhead_bytes()` (the C `ErlOffHeap.overhead`); `push_root(Eterm)`, `roots() -> &[Eterm]` (the payload roots, `m[0]` pointing into `mem`), `contains(term) -> bool`.
  `copy_root(term, src: &OffHeapTable) -> Result<Eterm>` — copies `term` from a process whose off-heap table is `src` into the fragment's heap, refcounts its off-heap references into the **fragment's** table (fresh slots), records the result as a root and returns it (the C `new_message_buffer` + the copy step of `erts_try_alloc_message_on_heap`); a fragment too small for the payload is refused up front, so it performs **no partial write** (roots stay empty, `used()` stays `0`).
- `pub struct Fragments` — an ordered chain of `HeapFragment`s (the C `ErlHeapFragment.next` chain): `new()`/`Default`, `is_empty()`, `len()`, `push(HeapFragment)`, `iter()`/`iter_mut()`, `total_used_words()` (the process message-buffer accounting — `used_size` summed), `off_heap_overhead_bytes()`.
  `move_into(dst: &mut Heap, dst_off_heap: &mut OffHeapTable) -> Result<Vec<Eterm>>` — the one-shot move (the `erts_move_multi_frags`/`erts_copy_one_frag` analogue): each fragment in order copies each root with `copy_struct` (the fragment's table as source, `dst_off_heap` as destination), so the moved term references **fresh** slots; roots come back in chain order, refc binaries and MSO objects move as **handles** (the same `data_ptr`, refcount incremented, never deep-copied); the fragment's own references are released only after the **whole chain** copied, so a mid-move failure leaves every fragment intact and the move retryable with a larger `dst` + fresh `dst_off_heap`.

### `message.rs` — the envelope and the FIFO queue
- `pub struct Message` — the copied payload plus sender identity and the off-heap objects it owns (the C `ERL_MESSAGE_TERM`/`_TOKEN`/`_FROM`, `m[0]`/`m[1]`/`m[2]`, plus the message's `off_heap`).
  `Message::new(payload, from)` (token `NIL`, no off-heap objects, no fragments); builders `with_token`, `with_from`, `with_off_heap(Vec<MsoHandle>)`, `with_binaries(Vec<RefcBinary>)`, `with_nodes(Vec<NodeEntry>)`, `with_fragments(Fragments)`; accessors `payload`, `from`, `token`, `off_heap`, `binaries`, `nodes`, `fragments`, `fragments_mut`; `set_from`/`set_token` in place; rebuild-preserving `with_payload`.
  Logical-size surface: `heap_words()` (the sender copy-charge input) and `logical_binary_bytes()` (the holder's virtual-binary-heap input) — semantics in `../CONTEXT.md` API Surface.
- `pub struct Queue` — the per-process FIFO (`VecDeque<Message>`, a pure data structure, **no** scheduler coupling): `new`/`Default`, `enqueue`, `dequeue`, `peek`, `len`, `is_empty`, `clear`, `iter`, and the positional scan API `cursor`/`cursor_at`/`position`/`message_at`/`advance`/`remove_at`.
  `pub(crate) remove_index(index)` — index-based removal used by `mailbox::receive` (the public `remove_at` name belongs to the cursor API).
- `pub struct QueueCursor` — `Copy`/`Clone`/`Debug`/`Default` (position 0)/`PartialEq`/`Eq`; a **monotonic scan position** over a `Queue`, not a stable handle: `remove_at` shifts every later position down, so a cursor outstanding across a removal is stale and callers re-derive one via `Queue::cursor()` (the engine seam does exactly this: remove the match, then reset to the first message). `advance` saturates past the end; every access is bounds-safe (`VecDeque::get`/`remove`).
- `fn debug_assert_enqueue_terms(from, token)` — the debug-only enqueue tripwire (`is_value` **and** `is_immediate` on both words), compiled out under `--release`; rationale and the C nuance in `../CONTEXT.md`.

### `mailbox.rs` — send / receive primitives (data level)
- `send(from, term, to_heap: &mut Heap, to_queue: &mut Queue, ctx) -> Result<()>` — copy into the receiver heap, then drain all three destination off-heap categories into the `Message`; **atomic** on refusal.
- `send_fragmented(from, term, to_heap, to_queue, ctx, fragment_capacity_words) -> Result<()>` — the fragment fallback, sized from the refusal's own exact price; the fragment carries its own off-heap table and the enqueued `Message` owns the `Fragments` (one fragment per send, never a chain — a size-bounded split across a chain is the caller's concern).
- `send_fragmented_after_refusal(from, term, to_queue, src_off_heap, fragment_capacity_words, requested_bytes) -> Result<()>` — no receiver heap, no trial, no pricing walk; converts `requested_bytes` through the `refused_words` helper (`size_object` the non-word-multiple fallback) and calls the private `enqueue_fragmented` **once**. `send_fragmented`'s refusal arm *is* a call to it.
- `receive<F: FnMut(&Message) -> bool>(queue, pred) -> Option<Message>` (remove and return the first match — the selective-receive skeleton) and `receive_ref<F>(queue, pred) -> Option<&Message>` (non-destructive).

### `off_heap.rs` — the off-heap (MSO + binary + node) reference table
- `pub use beam_gc::MsoHandle;` — the off-heap object handle is **beam_gc's** (MSO object + atomic `RefCount`); this crate never re-implements nor re-stores MSO.
- `pub struct OffHeapTable` — a **slot-indexed** table of `MsoHandle`s (slot = the on-heap off-heap reference's `val` word), a thin MSO-typed façade over `beam_gc::OffHeap` plus the refc-binary and node lists that ride along with a copied message.
  MSO API: `new()`/`Default` (manual — `beam_gc::OffHeap` is not `Default`), `push(MsoHandle) -> usize` (returns the slot), `get(slot)`, `clone_slot(slot)` (**bumps the refcount**), `len`, `is_empty`, `iter`, `overhead_bytes`, `into_handles() -> Vec<MsoHandle>`, `drain_handles()`; untyped escape hatch `get_entry(slot) -> Option<&beam_gc::OffHeapEntry>`.
  Refc binaries: `push_binary(RefcBinary) -> usize`, `binary_at(slot)` (clone ⇒ refcount++), `binary_by_data_ptr(*const u8)` (clone; resolves an on-heap `BinRef.val` to its owning handle), `binaries()`, `drain_binaries()`.
  Nodes: `push_node(NodeEntry) -> usize` (one entry per **distinct** node; a repeat counts another reference onto it), `nodes()`, `node_by_identity(Eterm, u32)`, `drain_nodes()`; `Drop` releases every node reference the table registered.
  `pub(crate) is_fully_empty()` (the all-category test) and `rollback()` (drains all three lists; sound only on a table that started empty) are what the single-pass path needs.
- **Ownership traps**, load-bearing for any pruning argument — see `../CONTEXT.md` API Surface and Known Issues: `len`/`is_empty`/`iter`/`overhead_bytes` cover the MSO list only; `push_binary` never deduplicates and `binary_by_data_ptr` returns the first match; there is no single-entry removal; `into_handles`/`drain_handles` clone-then-drop each handle (the net refcount is unchanged) because `beam_gc::OffHeap` has no move-out API; slots are stable and this table never calls `beam_gc::OffHeap::sweep`.

### `node.rs` — external-thing node entries
`pub struct NodeEntry` — a refcounted `(node, creation)` identity, the unit of `erts_ref_node_entry`/`erts_deref_node_entry`: `NodeEntry::new(node: Eterm, creation: u32)` (one reference); `node()`, `creation()`, `refcount()`, `same_node(&NodeEntry)`, `identity_eq(Eterm, u32)`, `handle() -> NodeEntry` (a second observation handle, registering no reference); `pub(crate) add_reference()`/`release_all()`.
`Clone`-free by design (an `Arc<NodeInner>` behind the handle): the count tracks *references registered in a table*, not handles.

### `error.rs` — copy-failure classification (no crate-local error type)
`pub fn is_message_heap_exhaustion(error: &BeamError) -> bool` (re-exported at the crate root and in `prelude`), plus the `pub(crate)` constructors `heap_exhausted(words)` / `bad_term()`.

### `layout.rs` — the copier's M1 identification/encoding helpers (`pub(crate)`)
Off-heap container (`is_off_heap_container`/`is_off_heap_term`/`off_heap_slot`/`make_off_heap_container`), refc binary (`is_refc_binary_term`), match context (`is_match_context_term`) and external thing (`is_external_header`/`external_node_creation`).
The canonical per-object/child-slot walk (`object_words`, `for_each_child_slot`) is re-exported (`pub(crate) use`) from `beam_term::layout`, not defined here; the **sizing** child-slot enumerator is deliberately absent (only `copy_reference`, `#[cfg(test)]`-gated, needs it).

## Constraints (module-local)
- `unsafe` only in `copy.rs` and `preflight.rs` (raw word read/write of the copy loop; the two raw-word reads refc-binary pricing needs) and the narrow `layout.rs` accessors; every `unsafe fn` carries a `# Safety` section and every `unsafe` block a preceding `// SAFETY:` rationale.
- One responsibility per file, ~1000-line target; `copy_reference.rs` is long because it is a full second walk kept as the equivalence oracle.
- Generated data is not owned here; the frozen term layouts come from `beam_term` and are read through its accessors.

## Design rationale (detail the crate root summarises)
- **The two pricing deltas from `size_object`, one per `SizingPolicy` method.**
  `is_shared` is the copier's own predicate, so a **literal-area subterm costs 0 destination words** and is not descended into — the C `size_object_litopt` skip (`copy.c:105,119`); the message-heap charge uses that `litopt` form (`global.h:1136`, `erl_message.c:771,839`).
  `object_words` overrides the single shape `beam_term::layout::object_words` prices for the *source* heap while `copy_struct` writes something else — a **refc binary whose payload no source off-heap handle owns** costs its flattened window (`heap_bits_size_words(end - start)`), the one price that depends on `CopyContext`'s **source** table (which is why the preflight is not independent of the `CopyContext`).
- **Why the count is exact in both directions.** `preflight_words` calls `size_object_bounded_with_scratch(term, usize::MAX, &policy, &mut scratch)`; at `usize::MAX` the walk's running total **saturates** instead of passing the limit, so a graph that fits *and* one that does not both come back with the true destination price — which is what lets the typed refusal carry the exact figure (`AllocFailure::requested`), and hence what lets a fragment be reserved from it without a second walk.
- **Why `limit` survives as a parameter at all.** It is kept only because the test-only bounded reference walk (`copy_reference`) is driven at every limit from `0` to past each graph's exact count and the two walks' verdicts are compared; the shipped walk never consults it.
- **Why the scratch is a thread-local with a re-entrancy fallback.** The borrow is taken with `try_borrow_mut` and released before the walk returns; an `Err` (a policy implementation that priced a second graph, or a scratch already destroyed at thread teardown) runs the *identical* walk on a locally built `SizingScratch::new()` instead of panicking — the answer never depends on which scratch ran.
- **Why the single pass needs no heap rewind.** `TrialTarget` bumps over `[to.hp(), hend)` counting `written` without touching `used`/`peak`, and `Heap` reserves per object on the normal path, so the trial leaves no partial state; only `OffHeapTable::rollback` runs, and `rollback` drains all three lists, which is sound exactly because the table started empty. `to.reset()` would be wrong on a dirty heap (it discards live `used` words), so the gate is `is_fully_empty()`, never `to.used() == 0`.
- **Why the literal-sharing agreement matters.** The C send builds a stack `erts_literal_area_t litarea` (`erl_message.c:722-723`, `INITIALIZE_LITERAL_PURGE_AREA`; `lit_purge_ptr` is NULL outside a code purge, `global.h:1119-1131`) and calls `size_object_litopt`/`copy_struct_litopt` (`erl_message.c:771,797`, `:839,860`), so `litopt != NULL` and `copy.c:720,744,767` shares any `erts_is_literal` subterm. Only the `litopt == NULL` macros (`global.h:1135,1152`) deep-copy literals, and no send site uses them — so this crate's sharing is a *match*, not a divergence.
- **Why the `is_shared` guard exists.** `beam_term::is_boxed` mirrors the C bit trick that is also true for a bare `HEADER` word, so the predicate requires a genuine term word first (`is_boxed_term(term)`, or `is_list(term) && is_term_word(term)`); without it a bare header (`HEADER_SUB_BITS`, `THE_NON_VALUE`) would be mis-shared as a literal and cross the boundary undetected.
- **Why the refc-binary copy is not a leaf copy.** The C's `erts_refc_inc(&from_br->val->intern.refc, 2)` bumps the payload; an opaque-leaf treatment of a `SUB_BITS` would leave the copied `orig` dangling into the source heap, hence the fix-up to the **copied** `BinRef`.
- **Why the fragment move copies.** The C `erts_move_multi_frags` relinks an object list because a C term holds raw object pointers; a Rust term holds a **slot index**, so the only way to re-home the objects is a copy that re-registers them into the destination table, then a release of the fragment's own (net refcount unchanged).
- **Why `NodeEntry` is local.** `erts_ref_node_entry`/`erts_deref_node_entry` operate on a global node table; the workspace has none, so the count is per-table structural bookkeeping (one entry per distinct node, count = references that table holds) and a real registry would later replace the atom with an `ErlNode *`.
- **Files that are long on purpose.** `copy_reference.rs` (~960 lines) is a complete second walk kept solely as the equivalence oracle for `preflight_words` — do not shrink it by sharing code with the shipped walk, since an independent implementation is the point.

## Notes for Agents
- `copy.rs`'s fast path writes only the destination's **unwritten tail** and commits with one `alloc_words`; never "fix" a refusal with `to.reset()` — it would discard live `used` words on a dirty heap.
- The preflight borrow of the thread-local scratch is released before the walk returns; do not hold it across the copy (a future policy that re-enters would then silently lose the warm-scratch property).
- `off_heap.rs` never sweeps: reclamation is the collector's (`beam_gc::OffHeap::sweep`, driven by the owning process's reachability sweep). A "prune" added here would break the slot-stability invariant the copier relies on.
- The reference walk in `copy_reference.rs` is the only place that names `beam_term::layout::for_each_sizing_child_slot`; keep it that way, or re-export the enumerator through `layout.rs` deliberately.

## Test Strategy
- Every module carries `#[cfg(test)] mod tests`; the crate-level integration suites live in `../tests/` (per-file pins in `../tests/CONTEXT.md`).
- Per-crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_message` (and `… build … --workspace --all-targets` for the workspace); always through `../../scripts/cargo.sh` (`$HOME` is read-only), never `cd`, never touch `../../tools/`.
- `copy_reference.rs`'s five equivalence tests are the guard on any change to the pricing walk: they compare `Ok(n)` and the rendered refusal against the bounded walk over a 35-term corpus at every limit, plus forged-cycle refusal on both walks.

## Routing Table
- `copy.rs` → `copy_struct`/`copy_struct_simple`/`size_of_copy`/`CopyContext`, the `CopyTarget`/`TrialTarget` single-pass path, `shallow`/`shallow_unshared`, `is_shared`/`identity`, `copy_refc_binary`/`copy_match_context`
- `preflight.rs` → `preflight_words`/`preflight_words_with_scratch`, `CopyPricing`, `refc_binary_words`, `shrink_warm_scratch`, the `WARM` thread-local `SizingScratch`
- `copy_reference.rs` → the `#[cfg(test)]` bounded reference walk + its five equivalence tests
- `share.rs` → `copy_struct_shared`
- `fragment.rs` → `HeapFragment`, `Fragments`, `move_into`
- `message.rs` → `Message`, `Queue`, `QueueCursor`, `debug_assert_enqueue_terms`
- `mailbox.rs` → `send`, `send_fragmented`, `send_fragmented_after_refusal`, `receive`, `receive_ref`, `refused_words`, `enqueue_fragmented`
- `off_heap.rs` → `OffHeapTable`, the `MsoHandle` re-export, `is_fully_empty`/`rollback`
- `node.rs` → `NodeEntry`
- `error.rs` → `is_message_heap_exhaustion`, `heap_exhausted`, `bad_term`
- `layout.rs` → the M1 identification/encoding helpers + the `beam_term::layout` re-exports
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `../tests/` → the twenty integration suites (per-file pins in `../tests/CONTEXT.md`)
- `../PERF.md` → the measured record
- `../../../erts/emulator/beam/{copy.c,erl_message.c,erl_message.h,erl_node_tables.h,erl_gc.c}` → the legacy C these modules reproduce (reference — read-only)
- `../../beam_term/src/{layout.rs,size.rs}` → the per-shape word table, the child-slot enumerators and the policy-parameterised sizing walk (dependency — read-only)
- `../../beam_gc/src/{mso.rs,offheap.rs}` → `MsoHandle`/`RefCount`/`OffHeap`, the storage this crate's table is a façade over (dependency — read-only)
