# `beam_rt::context::pump` — the nested cross-module run (pump bounds · machine pool · host memory)

## Intent
`../pump.rs` runs a cross-module callee that a table-less or table-borrowed context cannot enter in-machine: it drives the callee's `Machine` through its parked `receive`s with a bounded pump instead of folding them into `beam_engine::blocked()`.
`fun_apply_matrix.rs` (this directory) is the matrix suite that pins the fun-application seams across four `lists` keys and four fun kinds.
`../machine_pool.rs` is the thread-local pool those runs recycle their `Machine` from; it is declared from `context/mod.rs`, and this node owns its behaviour.
The dispatch, atom-namespace and heap-door facts those runs rely on are the parent's ([`../CONTEXT.md`](../CONTEXT.md)); the store and its reclaim pass that bound the memory below are [`../../eval/CONTEXT.md`](../../eval/CONTEXT.md) and [`../../runtime/CONTEXT.md`](../../runtime/CONTEXT.md).

## API Surface
### The pump — `../pump.rs`
`run_export_blocking`/`run_label_blocking` take a machine from `machine_pool::acquire`, run the entry, and — when the outcome is a suspend — drive it with `pump_until_returned(BLOCKING_PUMP_DEADLINE, outcome, step)`, a shared bounded loop whose `step` is `pump_once`.
`RUN_EXPORT_PUMP_SLICES = 64` slices advance the runtime per round.
`BLOCKING_PUMP_DEADLINE = 900 s` is the nested counterpart of the boot drive's `DRIVE_DEADLINE`; a run that never returns is the typed `unsupported` gap, never a hang.
`pump_once` returns `Ok(None)` on its first line under `may_pump_scheduler() == false`, which is the typed `blocked()` gap for a scheduled slice's out-of-line nested run.
`pin_window_if_store_written(store_used_before)` latches the `-eval` statement window (`Runtime::pin_window_if_store_written` → `eval::scratch::pin_transient`) **only** when the run actually wrote on the store, so a run that copied nothing leaves the window resettable.
`adopt_nested_payloads` adopts the run's payload handles into the runtime's escaped store (`Runtime::adopt_escaped_binaries`) and then **drains** the machine's reservoir with `Machine::retain_refc_binaries(|_| false)`, so a recycled machine owns no payload.
### A nested port wait — `wait_for_port_event`
When a live port owes the callee an asynchronous answer (a passive `gen_tcp:recv/2,3`, an `accept`, a datagram read), `pump_once` hands the wait to `wait_for_port_event` instead of gaping it.
That drives the blocking pass until one of **three** progress signals moves: an executed slice, a delivered port message (`Runtime::port_events_delivered`), or no live port owing an asynchronous answer.
The wait is bounded by `crate::port::PORT_EVENT_DEADLINE` (5 s of consecutive **no-progress** rounds, re-armed at now on every round that made some); an expiry is the typed `unsupported` capability gap, never a hang.
The wait arms, latches and consumes nothing, so a re-park waits on the same queued operation and never issues a second one.
Detail: [`../../port/CONTEXT.md`](../../port/CONTEXT.md) *The nested port wait*.
### Entry is tier-free
A nested run enters through `Machine::run_export_outcome`/`run_label_outcome` → the tier-free `run_loop`/`run_loop_with_map` → `Machine::drive`.
**No `Tier` is passed anywhere on this path**, so a nested callee never runs on the default baseline JIT and `BEAM_RT_TIER` does not change its tier; the `*_with_tier` entries are the runner slice's.
`beam_engine::jit::cache::remember_module` has **no production caller** — only the runner slice (`../../runner/run.rs`'s `select_entry_tier`) compiles into or reads the cache.
A run's own resumes reuse the same machine (`Machine::resume_outcome`).
The module handle is the code server's cached `Arc<Module>`, never a per-call `Module` clone (`CodeServer::load`'s `module.clone()` is an `Arc` handle clone).
## The machine pool — `../machine_pool.rs`
`acquire(module)`/`release(machine, module)` over a **thread-local** pool (not per-context, because `FunBridge::nested` builds a fresh `RuntimeContext` per applied fun) of **module-affine slots**: one `Slot` per callee image holding a `VecDeque<Machine>`, so a recursive entry of one callee stacks inside its own slot and another callee never evicts it.
`MAX_CALLEE_SLOTS = 96`, `MAX_CALLEE_DEPTH = 32`, `MAX_POOLED_MACHINES = 32`; past the ceiling the least-recently-used **callee** gives up its idle machines.
`acquire` is one `FastHashMap` probe plus a `pop_back` — no scan, no `Machine` memmove — and `release` `reset()`s the machine before pushing it and `debug_assert!`s both off-heap reservoirs empty (the drain itself is `adopt_nested_payloads`').
Affinity is a **hint, never an identity claim**: the label cache keys its entry on the image's digest, atom namespace and address, so a stale hint costs one build exactly as no pool would, and can never answer with another module's map.
`pooled_len`/`pooled_slots`/`pooled_depth`/`pooled_modules`/`pooled_reservoir_owners`/`fresh_builds`/`clear` are the census and test-support surface.
The `perf` module is the env-gated reuse census (`BEAM_RT_PERF_COUNTERS=1`: acquires, hits, fresh builds, evictions, distinct callees, peak pool size, peak callee depth, per-callee rows, printed from a thread-local `Drop`) and costs one relaxed byte when unset.
### The measured reuse, and its residual
The pool answers **99.5 %** of acquires from an entry (`ordsets` 14 932/15 007 with 75 fresh builds, 44 distinct callees, peak depth 13; `maps` 76 865/76 966; `string` 501 576/501 915).
A 128-machine ceiling measures **worse** (+2.7 % user CPU on `string.erl`, identical label-map census) because the extra idle machines pin one-entry label caches that can never be reused, which is why the ceiling stays at 32.
The residual is a `beam_engine` property, not a pool one: the label cache holds **one** entry and `Machine::reset` clears the context registry, so every run re-registers its in-machine callees through `label_map_arc` and rebuilds each one's whole-module map.
The pool's own bare route is **8 %** of the fills (`ordsets` 4 733 misses of 15 109 probes against 59 277 fills); the other **92 %** is that per-callee-per-run `Arc` route (`string.erl` 1 925 467 of 1 934 350 builds).
Closing it needs a multi-entry `LabelMaps` (or a context registry that survives `reset`) in `beam_engine`; no pool shape reaches it.
## The nested-run memory model
A table-less run (`RuntimeContext::new`: `mailbox_table: None`, `gc_armed: false`) has **no collector at all** — nothing reclaims during it — so a long in-process `compile:file/2` is where this runtime's per-run retention is visible.
Nothing here is per-`RuntimeContext`: a context is a borrow-only struct over `&'a mut Runtime`, so it owns no heap and dropping it frees only its inline `dispatch_memo` slots.
Two stores grow with the workload, and both are bounded only at a **non-running** site by `../../runtime/reclaim.rs` (the `-eval` statement boundary and the driver exit): the `-eval` store (each region clamped, the retired **chain** not) and the `beam_engine` arena (region-rewound above the pass's mark).
The pooled BIF heap is **sized by need** rather than the workload, and the escaped off-heap store is genuinely bounded.
### What each store retains
- **`Runtime::eval_heap` + `eval_heap_retired`** — every container result of a call that *may* rewind is deep-copied onto the store by `move_result_into_runtime_store` → `Runtime::grow_eval_heap`, which doubles the region and pushes the replaced one onto `eval_heap_retired`.
  Growth is clamped at `MAX_EVAL_HEAP_WORDS` (`1 << 25` words = 256 MiB) through `eval_heap_ceiling() = MAX_EVAL_HEAP_WORDS.max(eval_heap.capacity())` — deliberately **not** `+hmax` — and a request past it is the typed `system_limit` that leaves the store exactly as it was.
  So each step is bounded and the retired **chain** is not: growth always replaces, never relocates or reuses in place, and a retired region is ~fully written because it is retired only once `available() < needed`.
  The statement window that would reclaim a dead prefix is **pinned by the first nested run that writes the store**, so a compile — one `-eval` statement made of store-writing nested runs — only ever advances the store.
- **`Runtime::arena`** — `heap_available`/`alloc_words` on a table-less context answer the pointer-stable arena, whose `ContextHeap` is a `Vec<ReservedHeap>` that only ever grows: an exhausted region is kept, unmoved, and a fresh doubled one becomes the frontier.
  Its retention is its **cumulative demand** until the reclaim pass rewinds it with `MinimalContext::heap_rewind`, which is a **suffix** rewind (an older region holding dead words between live ones is left as it is) and refuses a region still named from the pass's roots.
  `heap_stats()` reports `regions`/`reserved_words`/`committed_words`/`used_words`/`peak_words`; `used_words` is cumulative demand, never live memory.
- **`bif_heaps`** — a push/pop pool whose regions are **depth-sized**: the outermost call gets `bif_heap_words`, every nested level `NESTED_BIF_HEAP_SEED_WORDS` (131 072 words = 1 MiB) self-tuned upward to `2 ×` an observed nested peak by `note_nested_heap_need`, with `bif_heaps_taken` the live depth.
  `take_bif_frontier` grows a region geometrically from its own capacity (no `bif_heap_words` floor) and is capped by `max_heap_words` only when `+hmax` is set.
  Seeding every level at `bif_heap_words` instead gave the same 27 regions `cerl_clauses` 3.62 GB of reservation against 161 MB — 22× less at an identical region count.
  The pool is a **high-water mark reused across calls**: two compiles in one runtime end at the same regions and capacity, not double.
- **Bounded, not growers:** the machine pool (32 machines, 96 callee slots, depth 32; a machine's `Vec` high-water retained by `Machine::reset`, never shrunk) and the wall-clock `BLOCKING_PUMP_DEADLINE`. Neither holds a heap.
### Capacity is not RSS
`Heap::new` does **not** touch its region: it is one `alloc_bytes` and the carrier writes a 16-byte block header plus a 64-byte mseg header, so two 128 MiB boot reservations sit at 12–48 kB RSS.
`ReservedHeap` tracks `reserved_words()` against `committed_words()`/`used()`, and `try_grow` is pure bookkeeping (no `mprotect`), so abandoning a reservation lowers `VmSize` by its full size and RSS by **zero**.
Only a caller's own writes make pages resident — which is why `bif_peak_used_words` reads 28 916 words (226 kB) under 161 MB of pool capacity, and why the depth-sized seed trades address space for written pages rather than winning on both.
`erlang:memory(total)` is blind to all of it (≈4.7 MB after a full `cerl_clauses` compile) because the pool and arena heaps are `AllocType::Standard` and the `MemoryHook` excludes `Standard` from `emulator`.
The figures that do see it are `/proc/<pid>/status` `VmHWM`/`VmSize`, the last `VmRSS` after `../../malloc.rs`'s one `malloc_trim(0)`, and `Runtime::retention_stats()` (`BEAM_RT_MEM_TRACE=1`).
### The bound
**Peak RSS ≈ the store's copy-out volume + the arena's written words + O(1)** — both terms grow with the workload itself, and neither is a policy a flag can move.
`inet_parse`'s `-eval` peak splits 59 % retired store (19 mappings each at `Rss ≥ 90 %` of `Size` — fully written) / 37 % arena written pages (2.85 GB touched of 16.18 GB reserved) / 2.4 % glibc std-heap (flat from ~1.4 s, `malloc_trim(0)` releases 1 byte) / ~1 % pooled heap, JIT code, file-backed mappings and stacks.
`arena_words` is cumulative demand (10.6 GB over 7 047 134 calls for `inet_parse`) and therefore an **upper** bound on the arena's resident bytes, not a figure in them: the `map_words_capacity(n) = 21n + 18` reservation is a deliberate upper bound while the written map is ≈3.7 words/pair and `write_map` never touches `[written_end, capacity)`.
Representative envelope (release build, OTP-30 bootstrap compiler first on `-pa`, `compile:file/2` of the named source): bare `-eval 'halt(0).'` 118 MB `VmHWM` / 0.27 s; `cerl_clauses` in-process 372 MB / ~1.0 s; `inet_parse` `-eval` 5.2 GB / 12.1 s (**completes**); `dbg_iload` 17.4 GB at a 25 s cap and **does not complete**.
Read the ratios, never the absolute times: the host is shared, and `inet_parse` on the `run` route never completes (`RUN_DEADLINE`, 60 s) where the `-eval` route finishes in 12.1 s — the two routes reach different depths, so quote the route with the figure.
### The store's writers that do not grow
`Runtime::eval_heap`'s **one** growing seam is `move_result_into_runtime_store` → `grow_eval_heap`, with two callers (the table-less re-home and the exempt-window stabilize arm), so a *scheduled* process writes the store too.
Three further families copy into the region they find and fail when it is short: `Runtime::stabilize_term`/`stabilize_term_with` (the gateway to every runtime-lifetime store — a timer delivery, an exit reason, a runner outcome, `persistent_term`, a trace match spec, the `enif_select` payload, `port_set_data`'s data), the `-eval` evaluator's `heap!` materialization, and the native floor's `beam_codec::term::make_binary(self.eval_heap(), …)` replies.
A short region surfaces as the typed `error:system_limit` (a BIF at a dispatch tail) or the evaluator's `Flow::Fatal` (`beam_rt: allocation failure: heap (N bytes)`, exit 1); none of them grows the store and none returns a wrong term.
That is sound because a completed move installs a destination whose capacity is at least that of the region it replaces and at least the live set it copied, so a move never reduces headroom for the same live volume.
The residual is the **multi-region** case: a collapse that also moves words in from retired regions leaves one region's free capacity where the separate regions had the sum, so these writers stay bounded by one region's capacity — the bound they keep, deliberately with no growth path of their own.
### The exempt-window nursery chain, and the debt gate
`Process::grow_nursery` is unreachable on the table-less route (`move_result_into_process_heap` returns at `mailbox_table.as_deref_mut()?` before `hint_grow_gate`).
On a scheduled route a big compile's child reaches it only inside a nested `apply_fun`/`run_*_blocking` subtree where the pid is **relocation-exempt**, so every collapsing collection door declines and a growth request that cannot be honoured lengthens the chain instead of collecting.
`resolve_external_call` therefore declines the fun-applying stdlib fast paths (`beam_bifs::needs_fun_caller`'s `lists`/`maps` roster) while the calling process carries collection debt, entering the call in the caller's own **armed** machine so the debt door collapses the chain as it grows — the gate is inert on a context with no process table, and the two `ets` folds stay synchronous.
What it does **not** cover: a nested run entered while the caller owes no debt, and a fun-applying key outside the roster.
## Known Issues
Every entry is a typed capability gap or a measured own-state difference — never a wrong result.
- **A scheduled slice's out-of-line nested run cannot pump, so a callee that parks in `receive` is the typed `blocked()` gap** — `pump_once` answers `Ok(None)` on its first line under `may_pump_scheduler() == false`, whether or not the `receive` carries an `after T`.
- **No apply shape is in that class.** Every apply route enters in-machine (`erlang:apply/3` through `apply_target` → `resolve_external_call`, `erlang:apply/2` and a direct variable-fun application through `call_fun` → `resolve_local_fun`), so a blocking `receive` inside the applied fun suspends the process.
  What still gaps is a fun or callee with **no in-machine entry**: an `-eval` evaluator closure (`EVAL_FUN_UNIQ`), a served local stub key, an external fun whose module or export cannot be loaded, and any host key that applies a caller-supplied fun without being a `FUN_CALLER_BIFS` row — such a fun's blocking `receive` surfaces the reason atom `unsupported_opcode: run: process blocked (no message available)`.
- **No in-run reclamation exists inside a nested `compile:file/2` (Phase 2).** Both region-reclaim mechanisms (whole-region drop + Tier A arena rewind, and the store evacuation move) run only at a non-running site; the three candidate in-run points and the five present-day invariants blocking them are [`../../eval/CONTEXT.md`](../../eval/CONTEXT.md) *Phase 2*.
  The binding ones: the parent machine is a Rust stack local between `machine_pool::acquire` and `release` (so `Runtime` cannot reach it), the evaluator's live terms are Rust locals with no stable `&mut` slot, and `ensure_heap` returns `Ok(())` at `armed == false`, which is every nested machine.
- **A window-close mark rewind is the cheapest sound way to give the exempt-window copy-outs' pages back, and it is blocked by the same missing root publication.** After the closure the process is no longer exempt and the machine is released, so `reset_to(store_used_before)` would reclaim each window's dead copies instead of only each statement's — but it needs a complete census of the window's escapes (the `result` plus the `PinKind::Lifetime` seam stores, not the `PinKind::Transient` pin this arm latches), and a wrong census dangles a store word rather than leaking one, so the probe must decline by default.
- **`+hmax` sizes only the two boot-time regions**, the pool's outermost region and `eval_heap`'s seed; it does not bound growth (`eval_heap_ceiling()` ignores the policy and `take_bif_frontier` applies `max_heap_words` as a cap on one geometric step), so its RSS effect exists only where a region actually fills.
- **The `[rt-mem] exit` census alone is misleading** — it reports the pool and the store as *capacity*, and the glibc std-heap is outside `beam_alloc` entirely, so neither resident mechanism is attributable from it.
- **`file_server`'s `get_until`/`get_line` drive a bare `Machine` and bypass `pin_window_if_store_written`.** Such a run is table-less (it can neither collect nor rewind, and its terms are arena words), but the latch it skips is what pins a holder the `-eval` descent cannot enumerate — [`../../native/CONTEXT.md`](../../native/CONTEXT.md).
## Test Strategy
`../pump.rs`'s inline `#[cfg(test)]` modules: `blocking_fun_apply` (a blocking fun applied from a scheduled slice), `in_process_compile` (incl. the `#[ignore]`d `every_stdlib_source_compiles_in_process_like_the_oracle` sweep, shardable by `BEAM_RT_SWEEP_SHARD=i/n`) and `nested_pump_bound` (the deadline and its typed gap).
`fun_apply_matrix.rs` runs four `lists` keys × four fun kinds, each cell as a **scheduled** process and asserted against the system `erl` oracle; its two controls pin the host fast path (a context that may pump) and a nested apply with an immediate child (which may accept the oracle value or the typed gap).
[`../../../tests/scheduled_apply_seam.rs`](../../../tests/scheduled_apply_seam.rs) pins the `BudgetContext` forwarding of `resolve_external_call`/`resolve_local_fun`/`resolve_local_call`.
The machine pool's census is read with `BEAM_RT_PERF_COUNTERS=1` beside the dispatch-path counters: `../../../../target/release/beam_rt -noshell -eval '<a compile:file/2 or lists:map/2 workload>'`.
Crate gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt --lib --no-fail-fast`.
The `in_process_compile` suite is the long one (shard it); the pump suites are fast.
## Notes for Agents
* **A nested run is entered in-machine when the context allows it.** `run_export*` is reached only from a declined call, a top-level entry or a fun application, so its `current_module` `String` allocation is not a per-`call_ext` cost.
* **Measure this node in retired instructions, not wall clock, and pin a P-core** — the host is hybrid (two `instructions:u` PMU instances), a from-source rebuild is instruction-identical but 2–4 % apart in wall, and a stashed binary is never a control: [`../../../STARTUP_COST.md`](../../../STARTUP_COST.md) *Measurement discipline*, [`../../../PERF.md`](../../../PERF.md).
* **The measured per-call cost of this route (callgrind on `lib/stdlib/src/ordsets.erl`) is the parent's**, and its remaining hot items are sibling levers (`gc_walk`'s valve descent, libc `malloc`/`free`, `build_label_map`, the JIT cache's `HashWriter`) — [`../gc/CONTEXT.md`](../gc/CONTEXT.md), [`../../../PERF_DISPATCH.md`](../../../PERF_DISPATCH.md).
* **Sample host memory with `/proc/<pid>/status` `VmHWM` plus `BEAM_RT_MEM_TRACE=1`** and, when attributing a page to a mechanism, `/proc/<pid>/smaps` per mapping plus an `LD_PRELOAD` `mallinfo2` sampler — the `strace -f -k` attribution is what distinguishes a glibc thread-arena heap from a beam region (glibc's first words decode as `heap_info { ar_ptr, prev, size = 0x4000000, mprotect_size }`).
## Routing Table
* `../pump.rs` → the nested-run pump (`run_export`/`run_export_blocking`/`run_label_blocking`, `pump_until_returned`/`pump_once`/`wait_for_port_event`, `RUN_EXPORT_PUMP_SLICES`/`BLOCKING_PUMP_DEADLINE`), `adopt_nested_payloads` and the three inline test modules.
* `fun_apply_matrix.rs` → the fun-application matrix suite (four `lists` keys × four fun kinds, scheduled-process cells against the oracle, plus the two controls).
* `../machine_pool.rs` → the thread-local module-affine pool of finished nested-run `Machine`s (`acquire`/`release`, `Pool`/`Slot`, the three ceilings), its `perf` reuse census and the `pooled_*` census/support surface.
## See Also
* [`../CONTEXT.md`](../CONTEXT.md) — the node's API surface, constraints and the term-lifetime rules.
* [`../gc/CONTEXT.md`](../gc/CONTEXT.md) · [`../pool/CONTEXT.md`](../pool/CONTEXT.md) · [`../calls/CONTEXT.md`](../calls/CONTEXT.md).
* [`../../runtime/CONTEXT.md`](../../runtime/CONTEXT.md) (the region-reclaim pass) · [`../../eval/CONTEXT.md`](../../eval/CONTEXT.md) (the store window, the pin family, *Phase 2*) · [`../../native/CONTEXT.md`](../../native/CONTEXT.md) (the `get_until` latch gap).
* [`../../../M1_GAPS_GC.md`](../../../M1_GAPS_GC.md) — the heap-model ledger.
