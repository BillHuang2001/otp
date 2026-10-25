# `beam_rt::eval::reclaim` — the `-eval` route's reclamation: one descent, two questions (`../reclaim.rs` · `resource.rs`)
## Intent
The file reclamation for the two stores a `-eval` run owns that a *scheduled* process's prunes cannot serve:
`Runtime::eval_off_heap` (the off-heap `RefcBinary` payload handles) and the NIF-resource terms `beam_abi` mints
(`enif_make_resource`).
Both are answered by **one reachability descent** (`../reclaim.rs::walk`) with a `Reach` sink per question — the refc
payloads (`Live`) and the resource terms (`resource::ResourceSink`) — so the guards, the visit budget and the
"doubt ⇒ decline" rule cannot drift between them.
`resource.rs` (this directory) is the **NIF-resource half**; the parent module owns the descent, the payload half, the
region scan and the trigger.
The parent `../CONTEXT.md` states the rules a `-eval` run must obey and points here for this node's detail.
## API Surface
- `../reclaim.rs` → `walk<R: Reach>(roots: &[Eterm], sink: &mut R) -> Walk` (the descent; `Walk::is_complete`),
  the `Reach` sink seam (`word(&mut self, term: Eterm)`, `bin_ref`), `Live` (the reachable `RefcBinary::data_ptr`
  set), `collect_region_payloads` (the word-pattern region scan), `prune(runtime, extra: &[Eterm]) -> Prune` (the
  store's one call point) and `prune_scoped` (the nested-run scoped form), `WALK_VISIT_BUDGET = 1 << 20`,
  `STORE_BUDGET_WORDS = 46_422`.
- `resource.rs` → `prune_at_statement_boundary(runtime, extra)` (the `-eval` boundary site),
  `prune_beyond(runtime, extra, frontier)` (a nested run's epilogue, called from
  `RuntimeContext::adopt_nested_payloads`), `resources_pending`, `Frontier { snapshot }` (the sorted raw words of
  every live resource term) and `ResourcePruneState { frontier }` (the runtime-held observation).
## The store: publish, then prune
`eval_off_heap` is filled by `Runtime::adopt_escaped_binaries` (a nested machine's reservoir, adopted before that
machine drops) and by every copy onto `eval_heap` (`CopyContext`'s destination table). **It is not pid-attributable**:
`run_export` is blocking-aware for every caller, so a scheduled `run`-route process calling e.g. `base64:encode/1`
grows it too — "no per-pid figure" is about `process_info(P, binary)` attribution, not about who grows the table.
The prune's argument has four parts, and each one is load-bearing:
1. **Every region that can hold a store-owned pair is read word by word, as words — never as terms.**
   `Runtime::for_each_off_heap_region` hands over the whole written prefix of `eval_heap`, its retired generations, the
   pooled BIF heaps and the `system_info` scratch heap, and `collect_region_payloads` reads each word's **pattern**
   (`beam_term::boxed::HEADER_BIN_REF`, whose neighbour is a payload pointer). A `[BinRef][ErlSubBits]` pair is
   therefore found wherever it stands, by its own header, and cannot hide behind a container the walk cannot read.
   **Region words are never pushed as roots**: a region also holds the payload *bytes* of on-heap binaries, and a data
   word whose low bits look like a pointer would be dereferenced.
2. **Every root family that holds a term no region holds is enumerated** — the evaluator's `Env` + reduced value (the
   caller's `extra`), `eval_closures[].env`, the `seq_trace` tokens, every live process's dictionary/mailbox/registers/
   saved arguments/signals, `pending_spawns`, an engine's un-consumed arguments and a parked machine's root set, and
   every pending timer delivery (`Runtime::gather_off_heap_roots`). Every one is a real term, so the walk may
   dereference what it reaches from them.
3. **The walk is conservative.** Any doubt — an exhausted `WALK_VISIT_BUDGET`, a boxed header it does not know —
   **declines the whole prune** and retains everything. That is deliberately the *opposite* of
   `RuntimeContext::graph_holds_reference`'s exhaustion answer ("assume a reference", whose only consequence is a
   declined move): a wrong answer here frees a payload a live term names.
4. **A shared payload is retained.** A handle whose `RefCount > 1` is owned outside the enumeration (a mailbox's
   retained binaries, a machine's reservoir, a copy's source table) and is kept. A false retain is a bounded leak, a
   false release a dangle.
Every residual is therefore a **leak, never a dangle**.
**The descent is a copy of `RuntimeContext::graph_holds_reference`'s shape** (an inline worklist, the same word guards,
`beam_term::layout::for_each_child_slot`) because it answers a different question with a different exhaustion answer;
de-duplicating the two is a deferral owned by `../../context/calls.rs`.
## The trigger
`O(1)` per call: the store's handle count is compared with the count the last prune or observation saw, and the
`O(handles)` byte figure is re-derived **only** when it moved (the shape `context/gc/bin_vheap.rs` uses). Past that,
the summed payload bytes must exceed `STORE_BUDGET_WORDS` (the BEAM default `min_bin_vheap_words`); only then is the
walk run and the store rewritten — `drain_binaries()` → retain live-or-shared → `push_binary`.
## The refusal rule, and the two guards that are deliberately not in it
The rule is exactly: *spend the visit budget or meet an unknown boxed header ⇒ decline*; *`RefCount > 1` ⇒ retain that
payload*. Two candidate guards were considered and rejected:
- **the global collection valve** (`Runtime::gc_enabled`): it says "this runtime stopped *moving* process heaps", and
  this prune moves nothing (a retained payload keeps its `data_ptr`); the `-eval` route does not trip it at all
  (`reclaim::tests::the_eval_route_does_not_trip_the_collection_valve`);
- **"an engine has ever existed"** (`Runtime::any_engine_ever`): sticky and set on every route (boot registers
  `{kernelProcess,…}` engines), so declining on it would make the prune permanently dead; a term an engine holds
  always brings its own handle, which part 4's belt retains. It stays a **diagnostic**.
## `OffHeapTable` traps (for anything rewriting the store)
- **No per-entry removal** — only `drain_binaries()` (all of them), so a prune is drain → filter → re-`push_binary`.
- `push_binary` **never de-duplicates**, and `binary_by_data_ptr` resolves only the **first** match; the `data_ptr`
  de-duplication lives in `Runtime::adopt_escaped_binaries`.
- `len`/`is_empty`/`iter`/`overhead_bytes` are **MSO-only** — after a `push_binary`, `is_empty()` is still `true`; a
  binary-side gate must read `binaries()` (`is_fully_empty` is the all-categories answer).
- A `beam_alloc::binary::growth::GrowOutcome::Moved` reallocation invalidates every previously handed-out `data_ptr`;
  re-read it from the live handles.
## Why the runner's park prune cannot be reused here
`context/gc/bin_vheap.rs::reclaim` needs `&mut ProcessTable` + a `Process` (for `collector().scanned_bounds()`) +
`&mut Machine`, and its gate `context/gc_walk.rs::reaches_an_unscanned_word` uses `Criterion::PayloadPrune`, which hits
the **first pointer-bearing word in `WordRegion::Foreign`** — and every `-eval` root region is `Foreign` by
construction. So a prune against the walker's roots always declines there, and
`beam_gc::for_each_reachable_binary_payload` (whose walk only enters a from-space word) can never see them either.
This node's descent is region-agnostic — it never consults the collector's scanned bounds — which is exactly why it is
a separate mechanism. The one region-agnostic walker that already existed is
`RuntimeContext::graph_holds_reference` (see above).
## The NIF-resource half (`resource.rs`)
`enif_make_resource` mints a reference block that owns one reference on the resource; C frees it when the term becomes
unreachable and the zero count runs the type's `dtor`. `beam_abi` has no GC, so the seam is a caller-supplied
predicate (`beam_abi::nif::enif::resources::release_resource_terms`). The process route's caller is
`context/gc.rs::resources::reclaim` at the runner's `Yield` park arm; this module adds the two routes that site cannot
serve — the `-eval` **statement boundary** (the action process is never enqueued, so it never parks) and a **nested
run's epilogue** (a cross-module call runs on a fresh `Machine` that is not a scheduled process, and a per-call leak
there repeats once per element).
**The frontier is the soundness device, and it is not negotiable.** A prune may free only what it can prove dead, and
the proof is "nothing in *my* complete enumeration names it" — but neither site has a complete enumeration (a pre-
expression mint may be held by an unseen store; a nested site sees one machine while the enclosing ones are alive).
So each site releases only candidates **minted after a known earlier point**: a candidate the `Frontier` already
reports is retained unconditionally, while one minted inside the site's window can only have escaped by being
**returned**, and a returned term rides the site's own roots. Consequence: the statement boundary's first boundary
records the frontier and releases nothing, and the nested site's frontier is taken at run **entry** — a bounded
one-window leak per site.
**There is no region word scan, and there cannot be:** a resource's identity lives *inside* its 4-word
`[header][node][serial][id]` block, and the mint block itself stands in a runtime region, so a scan would find the
very candidate it asks about and match every candidate at every site. Omitting it stays sound because the descent is
region-agnostic: a live copy is reached from the named roots wherever it stands, and a copy in a region nothing names
is dead data (the regions are never reset, so treating their words as roots would retain forever).
**Gates (every one a decline, and a decline is a leak):** the kill switch (`Runtime::set_resource_prune_enabled`); the
frontier is unchanged (`O(1)`); the fresh set is empty; the walk declined (and the *observed* frontier still
advances, so a once-declined candidate is not re-walked every statement).
**Residual class** (shared with the park-arm site): a resource held only by a store outside the walk's root families
(another process's heap, ETS, a `persistent_term` value, an unenumerated `seq_trace` token) is answered unreachable,
so its `dtor` never runs; a candidate whose reference identity cannot be read is recorded as **reached**; the nested
site's error path prunes nothing (a raise's reason is not in the frozen root set); and pointer reuse in the rewound
pooled BIF heap makes a fresh mint look pre-existing (retained — a leak).
## Constraints
- **No silent swallowing**: every gate is a recorded decline with a `&'static str` reason on `Prune`/`PruneStats`; a
  declined walk retains everything and the count is reported.
- The `#![allow(unsafe_code)]` override (documented at both modules' tops) exists only for the term-graph raw reads:
  every read is guarded exactly as `graph_holds_reference`'s are, and no pointer is allocated.
- `apply_eval_closure` **never prunes** — its caller's terms are live and unnameable from there. A raise from a nested
  run takes the plain adopt path (its reason rides `Raise`'s live-reason slot, not the frozen root set), so that
  run's payloads are retained.
## Test Strategy
`../CONTEXT.md` *Test Strategy* is the recipe index; this node's evidence is `../../../tests/eval_off_heap_reclaim.rs`
(the store bounded end to end), `reclaim.rs`'s colocated suite (the valve pin, the region scan, the shared-payload
belt) and `resource.rs`'s colocated suite (`prune_beyond_defers_with_nothing_fresh` and the frontier cases).
Crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt`.
## Routing Table
- `resource.rs` → the NIF-resource half: `Frontier`/`ResourcePruneState`/`ResourceSink`, `prune_at_statement_boundary`,
  `prune_beyond`, `resources_pending`, and the raw 4-word reference-block read its `unsafe` override sanctions.
- `../reclaim.rs` (the parent module this directory documents) → the descent (`walk`/`Reach`), `Live`,
  `collect_region_payloads`, `prune`/`prune_scoped`, `WALK_VISIT_BUDGET`, `STORE_BUDGET_WORDS`.
## See Also
- `../CONTEXT.md` → the route's storage regions, the prune points and the rules a `-eval` run must obey.
- `../../runtime/reclaim/CONTEXT.md` → the region-reclaim pass (the store regions, the arena, the move).
- `../../context/gc/CONTEXT.md` → the off-heap reservoir prune for a scheduled process (the site this one cannot serve).
- `../../../tests/eval_off_heap_reclaim.rs` → the end-to-end store-bound evidence.
