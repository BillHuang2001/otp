# `beam_rt::runtime` — the `Runtime` façade, the scheduler-pass family and the runner host

## Intent
`../runtime.rs` is the `Runtime` façade: the one type that owns every runtime table and every runtime-level
operation the CLI, the boot path and the `-eval` evaluator call (`run_function`/`run_module`/`run_scheduled`,
the spawn API, `system_info`, `eval`, `format_term`, `refs_mut`, the timer/distribution accessors).
`./scheduler.rs` (+ `scheduler/tests.rs`) is the **scheduler-pass family**: it drives processes, ports, timers
and distribution in bounded steps; every entry point is an inherent `Runtime` method.
`../runner/` is the `beam_process::ProcessRunner` each scheduled process runs through — a sibling node,
[`../runner/CONTEXT.md`](../runner/CONTEXT.md).
`gc.rs` holds the runtime-level GC state, accounting and safety valves; `ets_owner.rs` the
`ProcessExitObserver` host with the two owner-death drains; `retention.rs` the retention accounting;
`arena_census.rs` its attribution census; `timer_wakes.rs` the timer reference→target mirror; `executing.rs`
the in-machine position record.
Three children own the bulky detail — [`scheduler/CONTEXT.md`](scheduler/CONTEXT.md) (the pass),
[`reclaim/CONTEXT.md`](reclaim/CONTEXT.md) (region reclamation + its measured record) and
[`roots/CONTEXT.md`](roots/CONTEXT.md) (the mutable root door); this file keeps the node's contracts, its
cross-cutting inventories and the routing into those three.

## API Surface
### `../runtime.rs` — the façade
`Runtime` owns every table (config, atom table, processes + mailboxes, BIF heaps, scheduler + runner state,
ETS/port/NIF registries, refs, timers, the node-global `persistent_terms` store, `file_services`, dist) and the
scratch `info_heap`.
* **boot/config**: `new(RuntimeConfig)`, `with_defaults()`, `boot()` (idempotent; A.7), `boot_from_script(path)`,
  `is_booted()`, `config()`, `load_module(path)`, `module(name)`, `preloaded()`, `preloaded_loaded()`/
  `is_preloaded_loaded(name)`.
* **the slice**: `run_function(path, function, arity, args) -> Result<Eterm>` (x(0)); `run_module(module,
  function, arity, args, out: &mut dyn OutputSink)`.
* **the CLI `run` action**: `run_scheduled(path, function, arity, args) -> Result<Eterm>` loads the file,
  `spawn_process`es the export and pumps the **full** scheduler in bounded steps (`RUN_STEP_SLICES` = 1000,
  `TimerWait::AtMostWhenIdle(PORT_POLL_SLICE)`, `RUN_DEADLINE` = 60 s) until the process returns (`x(0)`),
  **raises** (`take_process_error` returns the original `BeamError`), is quiescent
  (`ran == 0 && delivered == 0 && is_quiescent()` → the typed `beam_engine::blocked()`, exit 3), or outlives
  the deadline. `is_quiescent` is four terms — empty run queue; `PortRegistry::has_nothing_to_poll()`;
  `timers.next_deadline().is_none()` (the `run` route narrowing it to a deadline `timer_wake_targets.can_wake`);
  and no distribution work (`DistNode::is_idle`, `DistClient::has_links`, `DistClient::has_pending_outbound`) —
  [`scheduler/CONTEXT.md`](scheduler/CONTEXT.md).
* **the bounded pumps** — each is the same pass body under a different `TimerWait` policy (`Never`,
  `Deadline`, `AtMost`, `AtMostWhenIdle`, `ImminentWithin`; only the last three carry a "sleep at most once"
  budget): `pump_scheduler` (**blocking**, `AtMost(TIMER_PUMP_SLICE)` = 100 ms — the `-eval` **bare** `receive`,
  the busy-port wait and the nested cross-module port half), `pump_scheduler_now` (`ImminentWithin(ZERO)`, the
  `-eval` `receive … after T` loop), `pump_scheduler_boot` (`ImminentWithin(TIMER_PUMP_SLICE)`, the boot drive),
  and `wait_for_timer(until, max_slices)` (sleep to `min(earliest deadline, until)`, then one pass under
  `ImminentWithin(until - now)`, so a wait never outlives its caller's deadline). Per-policy table and pins:
  [`scheduler/CONTEXT.md`](scheduler/CONTEXT.md) *The entry points and their wait policies*.
* **the `-s`/`-run` action**: `run_named(module, function, arity, args) -> Result<Eterm>` resolves the module
  through the code path (system library `ebin`s included) and runs it as a freshly spawned, unregistered action
  process under the `RootAction::Module` frame; a **found-but-refused** image is the bare `error:undef` here
  exactly as at a call site (`error::call_load_failure`), never the loader's `module name in object code is …`.
* **spawning/scheduling**: `spawn_process(module, function, args)`, `spawn_fun_process(fun, args)`,
  `resolve_export_entry(module, function, arity) -> Result<ProcessEntry>` (code-path search + load + export
  check, classified like a call and resolved **eagerly**, so a failed load raises at the spawn where the oracle
  spawns and the child dies — `../../M1_GAPS.md` *The `spawn(M,F,A)` reason surfacing*), `enqueue_process(pid)`
  (idempotent), `process_result(pid)`, `process_exit(pid)`, `run_queue_len()`.
* **refs/introspection**: `refs_mut() -> &mut RefAllocator` (**THE** ref id space — `make_ref` + monitors +
  timers), `system_info(key)`, `system_info_value(key)` (heap-free source of truth), `eval(expr)` (A.6
  `-eval`), `format_term(term, module)`.
* **timers/dist**: `timers_mut()`, `dist_node()`, `node_name()`, and the connector accessors (`dist_client()`,
  `dist_connected_nodes()`, `dist_connect(peer)`, `dist_send_reg_send`, `dist_send_message_pid`,
  `dist_send_alias`). `dist_connect` is **idempotent** — a peer already established on **either** half returns
  `Ok(())` and dials nothing (a second `'N'` handshake is a simultaneous-connection collision the peer kills one
  half of), which is why `net_adm:ping/1` against a peer that dialled *us* answers `pong`; each outbound send
  entry **dials an unconnected peer once** (`dist_ensure_link`/`dist_dial_name`/`dist_peer_is_local`), so a
  send — not a prior `net_adm:ping/1` — opens the link (`no_link_error` for one the dial cannot reach) —
  [`../dist/CONTEXT.md`](../dist/CONTEXT.md).
* **node monitors (A.5)**: `node_monitors: FastHashMap<ProcessId, Vec<NodeMonitorLatch>>` —
  `erlang:monitor_node/2`'s subscriptions (the C's `ERTS_MON_TYPE_NODE` chain, **not** the `monitor_nodes` flag
  store), each latch the node's atom term plus its `saw_connected` state — with `monitor_node_add` (the C's
  idempotent set: no duplicate, no refcount) / `monitor_node_remove`, the pure diff
  `node_monitor_transitions` (`up` only on not-seen→present, `down` only on seen→absent, nothing in either
  steady state, an unnameable node inert) and the pass step `service_node_monitors`, which delivers each
  transition as the plain 2-tuple `{nodeup|nodedown, Node}` (arena-built message, `emulator` `from`). It is the
  **one** pid-keyed runtime map iterated on purpose, over subscribers only: `Ok(0)` and no allocation when nobody
  subscribed, and on `nonode@nohost` every latch stays silent. The BIF body is `../native/net.rs::monitor_node_2`;
  `erlang:monitor_node/3` stays a typed gap.
* **peer loss (A.5)**: `lost_dist_peers: LostPeers` is **one** shared `Rc<RefCell<Vec<String>>>`
  (`../dist/links.rs`), installed on both halves, so either can be the observer. A half records a peer's
  `node@host` only when it retired a link that **had** established (`was_established`, the pump-set latch, not
  `is_established` — the pump that finds the death is the call that moves the driver to `Closed`/`Failed`); a
  graceful drop, a `close` or a `connect_addr` replace records nothing, because a link that never handshook was
  never a node down. The retirement sites only *record* (they run inside a pump, which holds no process table);
  `service_lost_dist_peers` is the drain — `Ok(0)` after one borrow and one `is_empty` when nothing was lost,
  a name `DistClient::link_names` still lists skipped (a **re-connect** fires nothing), an unresolvable atom
  declined (never fabricated), else `noconnection` interned and `ProcessTable::node_down` run. Complementary to
  `service_node_monitors` and no double-fire: that step serves `erlang:monitor_node/2` subscribers, this one the
  **process** relationships (a remote monitor's `{'DOWN',Ref,process,PeerPid,noconnection}`, a trapping remote
  link's `{'EXIT',PeerPid,noconnection}`, a non-trapping one's termination).
* **the native file service**: the `file_services` field (`FileServices`: the `file_server_2` service process +
  its live io-devices) and `service_file_requests`, the pass step that services them —
  [`../native/CONTEXT.md`](../native/CONTEXT.md)'s `file_server.rs` subsection.
* **the runtime's own processes**: `Runtime::new` starts exactly **four**, none user-spawned — the **`init`
  stand-in** (`Runtime::init_pid`, the table's first identity, so it renders as the oracle's `init` `<0.0.0>`;
  born `Runnable`, never enqueued, registered under `init` on every route, and the `parent` of every process the
  runtime did not see being spawned), the **root process** (`Runtime::self_pid`, `<0.1.0>`, the process every
  action runs as, registered under **no** name — the oracle's `-eval` process has none either), the
  **`erl_prim_loader`** stand-in (born `Runnable`, never enqueued, registered on **every** route, so
  `erlang:whereis(erl_prim_loader)` answers a live pid) and the native `file_server_2` service; the loader's
  *functions* stay on the native floor (`../native/loader.rs`). One pid plays both the `init` name and the
  `parent` role, exactly as the oracle's `<0.0.0>` does.
* **`Timers`** probes with the crate's own `FastHashMap` and reclaims a non-head cancellation with a **watermark
  heap rebuild** (`Timers::compact` once `heap.len() > 2 * pending.len() + COMPACT_SLACK` = 64), so a finite
  `receive … after T` cancelled by a real message leaves no permanent tombstone per iteration.

### `runner/` — the `beam_process::ProcessRunner`
`RuntimeRunner<'a>{runtime, state}` implements `ProcessRunner::run(pid, table, budget) -> Result<RunOutcome>`:
`Returned(v)` → record `v` + `Exit(normal)`; `Wait{func,pc}` → park + `Wait`; `Err(e)` → the reason term + the
original `BeamError` recorded in `RunnerState`, then `Exit(<reason term>)`. `RunnerState` holds an engine per pid
plus the pid-keyed `results`/`exits`/`errors` maps — all the crate's own `FastHashMap`, each probed only by pid
and never iterated for an observable order.
Everything else about this instrument — the park/resume root hand-off (`publish_parked_roots`/
`resume_from_process_roots`, `RootSet::ALL_LIVE`, the FR-prefix window, the parked engine's out-of-root-set terms
as extra roots: the pending-exception reason plus the host-held families), `park_machine`, `BudgetProbe`/
`BudgetContext`, receive-deadline arming, the exit-reason mapping and the per-file split — is its own node's:
[`../runner/CONTEXT.md`](../runner/CONTEXT.md); the measured per-workload cost is
[`../runner/MEASURED.md`](../runner/MEASURED.md).

### `gc.rs` — the runtime-level GC state
`GcStatsSnapshot` (`gc_stats`) reports collections/failures/halted pids/valve trips **and the `reclaimed` word
counter**, advanced by `Runtime::note_gc_reclaimed(words)` from `serve_gc_request`'s Ok arm, the three
collection sites in `../context/gc.rs` (`ensure_heap`/`ensure_heap_full`/`alloc_words_rooted`) and the boundary
door (`../context/gc/reds_door.rs`) — backing `erlang:statistics(garbage_collection)`'s
`{collections, reclaimed, 0}` (detail: [`../context/CONTEXT.md`](../context/CONTEXT.md),
[`../../M1_GAPS_GC.md`](../../M1_GAPS_GC.md) *Memory and statistics*).
`GcState` holds the **pin** set (a pid whose heap must not move), the **halt** set (a pid whose collection
failed, or whose root-set sizes changed) and the global **valve** (`disabled`, the first reason, a trip
counter).
`try_stabilize_escaped(term) -> Result<Eterm>` is the **one** escape-store stabilization: it copies a
runtime-level store's term onto the pointer-stable store when a heap may move, propagates a **genuine** copy
failure (`BeamError::SystemLimit` — the term does not fit) and trips the valve (`disable_gc`) **only** for the
documented off-heap-payload capability gap (an unresolvable **MSO** container, or a malformed graph — a refc
binary whose payload no source table owns is **flattened** on-heap instead, `stabilize_term`), returning the
original term then. Every runtime-level store goes through it — the pending spawns (`note_pending_spawn`), the
native message/function stores (`native.rs`, `native/erts_internal.rs`), the `RuntimeHooks` store
(`context/runtime_hooks.rs`) and the runner's exit-reason call site (`../runner/run.rs`).
`try_stabilize_escaped_with(term, &SourcePayloads)` is the **machine-aware** sibling: it builds the copy's
*source* off-heap table from the caller's `SourcePayloads` (`Runtime::source_payloads`/
`Runtime::stabilize_term_with`) rather than the empty one, so the one caller holding the machine that built the
term (the runner's exit-reason arm) **shares** an engine-built off-heap binary or MSO container — one
refcount, no materialized copy — where the empty table must flatten the refc binary; `try_stabilize_escaped` is
exactly `try_stabilize_escaped_with(term, &SourcePayloads::default())` (handle-pushing, node-list and
destination-table rules: [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md) *the transport copy*).
`halt_process_gc`/`disable_gc`/`gc_enabled`/`gc_process_enabled(pid)` are the mutators/queries; a process whose
heap may not move is marked **relocation-exempt per slice** on the `Process` itself (the runner's post-slice FR
valve), never through a runtime-level pin.
Because the copy keeps a NIF resource's **reference identity**, `Runtime::gather_off_heap_roots` counts the
pending timer **deliveries** (`Timers::gather_off_heap_roots`) and the stable engine **arguments and entry fun**
plus an un-published parked machine's own root set (`RunnerState::gather_off_heap_roots`'s two sub-families) as
root families beside the escaped closures' environments, the `seq_trace` tokens, `pending_spawns` and every live
process's dictionary, mailbox, registers, saved arguments and signals — a family left out makes a live resource
look unreachable, and only the families that survive a pass's two table moves are live at the park arm —
[`../../M1_GAPS_A3_NIF.md`](../../M1_GAPS_A3_NIF.md) *NIF (A.3)*.
The collection gate is the **two-phase unscanned-root valve** (`../context/gc.rs::root_is_unscanned` +
`../context/gc_walk.rs`): phase 1 classifies every root word and always runs; phase 2 descends the process's
root-reachable scanned graph and runs only when the per-pid descent bit `GcState::foreign` is open, over the
reused scratch `GcState::walk` (`WalkScratch`, `Runtime::walk_scratch()`). Phase 1 declines only for a foreign
container that forwards a **scanned** word, so a leaf (a `make_ref/0` reference, a float, a pid, a pooled-BIF
tuple of immediates) latches nothing; `Runtime::close_foreign_reachable` retires the latch on a
`ValveVerdict::foreign_free` descent from the **three** doors that hand the walk the machine's whole `RootSet`
(`ensure_heap_full`, `alloc_words_rooted`, `gc/reds_door.rs`), while the narrow `ensure_heap`, the park arm and
the `RuntimeCollectionGate` signal sites may only open it. The **same valve is installed on the process table**
as the `beam_process::CollectionGate` host seam — `context/gc_walk.rs::RuntimeCollectionGate`, built by
`Runtime::new` over the **one** shared `ForeignLatch` cell so a signal site decides in `O(1)` while the pid's bit
is clear, owning its own `WalkScratch`, travelling with the table into every pass, because the `beam_process`
signal sites hold only `&mut ProcessTable` — [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md) *The signal
sites' collection decision is the host's `CollectionGate`* and *A collection is declined only by a foreign
container that forwards a scanned word*.

**The forced-request probe is split for codegen, and the attributes are the lever.** `Runtime::gc_request_pending`
is `#[inline]` — the `requested.is_empty()` length compare lands in each caller's frame, which is the state of
every workload that never calls `erlang:garbage_collect/0,1` — in front of `Runtime::gc_requested_contains`,
which is `#[inline(never)]`, so the SipHash probe's body stays out.
The attributes are load-bearing rather than decorative: the release profile is `lto = "thin"` with
`codegen-units = 1`, which inlines a plain private helper across crates, so an outline stated only by "do not
write `#[inline]`" is erased — that is how both whole-root doors (`ensure_heap_full`, `alloc_words_rooted`, which
take the predicate on every admission that finds room in the nursery) came to carry the probe's body *and* the
six callee-saved pushes its call forces in their prologues.
With the split the doors pay `cmpq $0,len` + `je` on the hot arm and one indirect call on the armed arm;
re-inlining it is a regression of ≈0.8 % of `bench_gc_bound:churn/1`'s retired instructions.

### `ets_owner.rs` — the `ProcessExitObserver` host and the two drains
`OwnerIndex` (additive, shared with `Runtime`) records, in death order, the dead pids waiting for a drain and
the append-only `(PortId, MonitorRef)` `PortWatch` pairs.
`EtsExitObserver` is the `beam_process::ProcessExitObserver`: it pushes the dead pid and its drained `PortWatch`
pairs and returns (no BIF borrows the index).
`Runtime::drain_ets_owner_exits` discharges the ETS obligations (delete a dead owner's indexed tables, honour
an installed heir), and `Runtime::drain_port_monitor_exits` sweeps the dangling incoming port monitors; both
take the borrow with `try_borrow_mut` and **defer** to the next drain on a live borrow — never a panic, never a
dropped obligation.
**A failed heir-gift deep copy is never swallowed.** `Runtime::adopt_created_table` re-owns a fresh table's
creation-time gift through the fallible `Table::install_heir`; `Runtime::own_heir_gift` returns that error (a
`try_borrow_mut` miss, an unknown handle, a missing heir and an arity-2 heir are all `Ok(())`) and the one
policy `Runtime::report_heir_gift_failure` handles it: logged immediately (`beam_error!`, the policy
`beam_process`'s exit-observer uses for a panicking observer) **and** recorded in the owner index, so the next
`drain_ets_owner_exits` returns it as the typed error it is (`badarg` for a term outside `beam_ets`'s copyable
subset, `out_of_memory` for an exhausted store). The successful `ets:new/2` stays successful: the table exists
and its gift term is table-owned (the creation-time gift was deep-copied by `Table::new`'s build path before the
re-ownership is attempted), and `install_heir` leaves the current heir untouched on failure.

### `retention.rs` / `arena_census.rs` / `timer_wakes.rs` / `executing.rs`
`retention.rs` is the **retention accounting**: `RetentionStats` (one snapshot of the four long-lived storage
regions — arena demand counters; the pooled BIF heap's regions/words/capacity/peak-use plus its nested-call loan
count and seed; the `-eval` store's current and retired capacity; the escaped off-heap store's handle count) and
its `Runtime::retention_stats()` accessor (a pure read, no allocation, consulted by nothing in the runtime),
plus the one-line `Display` form `BEAM_RT_MEM_TRACE` prints after the boot and again on the way out of
`cli::run`.
`arena_census.rs` is the **attribution** census that goes with it (`BEAM_RT_ARENA_TRACE`,
`Runtime::arena_census_report`): *which* requests produced the arena's demand (see its own section below).
`timer_wakes.rs` is `TimerWakes`, the timer-reference→`TimerTarget` mirror the pass absorbs its timer wakes
through; `executing.rs` is the in-machine position record (`InMachinePosition`).

## Constraints
* **`unsafe`-free** (`#![deny(unsafe_code)]`) and return-based errors ([`../../CONTEXT.md`](../../CONTEXT.md)
  §3): every fallible API returns `beam_common::Result`.
* **The scheduler moves the table (and the runner state) out of the `Runtime`** — `run_pass`'s
  `take_processes`/`put_processes`, restored whatever the outcome, because a pass needs `&mut Runtime` **and**
  `&mut ProcessTable` at once while the table is a *field*; the runner likewise takes the pid's `Machine` out of
  its `HashMap`. Rationale: [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md) *the scheduler moves the
  table*.
* **A process's exit reason is not its return value** — [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md)
  *a process's exit reason*.
* **Heap lifetime**: `Runtime` owns the process heap so a returned `Eterm` stays valid after `run_*` returns;
  `system_info`/`format_term` use the separate `info_heap`, rewound per call by `Runtime::reset_info_scratch`,
  which **refuses** once `Runtime::info_heap_tainted` is latched, and `erlang:system_info/1` leaves the region
  through `RuntimeContext::relocate_off_scratch` (the copy-out: the running process's scanned young heap, else
  the never-reset table-less `-eval` store) — [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md) *Heap
  lifetime*; pinned by `../../tests/system_info_retained.rs`.
* **A run-queue pid with no engine entry point is a recorded typed diagnostic, never an aborted pass.**
  `RuntimeRunner::run` takes the pid's `ProcessEngine` out of `RunnerState::engines`; when a pid was enqueued that
  this runtime never registered an engine for (`RunnerState::contains`, `../runner/entry.rs`), the runner records
  one `UnrunnableProcess` (pid, `ProcessState`, `in_run_queue`, priority, registered name when the table holds
  one) in `RunnerState::unrunnable`, **restores the process to `Runnable`** (`slice_begin` had set it `Running`,
  which also changes `Process::has_unpublished_roots`) and returns `RunOutcome::Wait`, so the pid leaves the run
  queue and no slice is fabricated. The record is first-write-wins per pid and surfaces as a `[rt-trace]` line
  through `crate::debug_trace`. The **enqueue** side refuses the same pid: `Runtime::absorb_fragment_wakes` skips
  one the pass's `RunnerState` does not know, and `Runtime::enqueue_process` rejects one with the typed
  `error::unknown_process_entry`.
  The other half is why the runtime's own services are safe: a runtime-owned, never-enqueued service is created
  with `ProcessTable::spawn(policy)` (born `Runnable`, `insert`ed, **not** enqueued) +
  `ProcessTable::register(name, pid)` — `init`/`Runtime::init_pid` (the root process `Runtime::self_pid` holds
  **no** name), the `erl_prim_loader` stand-in, the `file_server_2` service, the registered `net_kernel` service
  and the `-eval`/`-s`/`-run` action child. `whereis`/`monitor(process,Name)` resolve it and a send lands in its
  mailbox, but it never runs: only `Waiting`/`Suspended` are woken by a delivery (`Process::wake`), so it never
  reaches a run queue's wake list, and a heap-fragment delivery notes a wake **only** when that same `wake()`
  answered `true` ([`../dist/CONTEXT.md`](../dist/CONTEXT.md)). `scheduler_pass` calls
  `Runtime::service_file_requests` as an **additive step right after the run queue drains** (the service's and
  its io-devices' mailboxes, replying via `ProcessTable::deliver_simple` + `scheduler.resume_woken`), and each
  delivered reply counts into `delivered`, so a serviced-only pass never looks like "nothing happened".
* **A lease is never a cheap borrow.** Holding an `Rc<RefCell<…>>` handle across an allocation is a deep-copy or
  a dangling hazard: the port registry's borrow (`with_port_table`) and the two `Rc<RefCell<…>>` root stores
  (`persistent_terms`, `ports`) are borrowed for exactly the statement that needs them, and the atom table's
  one-process-lifetime lease rule is `../CONTEXT.md`'s *The one process-lifetime `Rc<AtomTable>` lease*.
* **Files ~1000 lines**, split by responsibility; every public item documented (`#![warn(missing_docs)]`).

## Term lifetime and rooting — the four rules
Every GC/scheduler contract in this file rests on these; a change that violates one is a dangling-term defect,
not a tuning choice.
* **(a) A raw `Eterm` in a long-lived slot roots nothing.** Any term that can outlive one instruction must be
  copied onto a scanned heap (the process's) or explicitly stabilized (`try_stabilize_escaped`/`stabilize_term*`)
  before the owner's heap can be relocated; the host-side inventories below are the roster of slots that do that
  and of the ones that deliberately do not.
* **(b) The pointer-stable, never-scanned store is the wrong home for a container whose children live on a
  scanned young heap.** `Runtime::arena` (`alloc_words`) is never scanned and its regions never move, so a
  collection leaves the container verbatim and relocates the children — a container built there naming young
  words dangles. The mirrored rule for the `-eval` store: it is pointer-pinned, so the one thing it cannot
  survive is a **move** of the region a stored word names.
* **(c) When a heap growth/move is exempted the fallback is "stabilize, never leave dangling"** — refuse ⇒
  stabilize. `alloc_words_rooted` inside a `with_relocation_exempt` window stabilizes instead of growing; the
  store move's refusal is `MoveOutcome::KeptBoth`; the off-heap prune's every gate is a decline (leak, never a
  dangle).
* **(d) An `Rc` lease held across an allocation is a deep-copy or a dangling hazard, never a cheap borrow** —
  see the `Rc<AtomTable>` and port-borrow rules in `../CONTEXT.md` and `Constraints` above.

## Every site that can relocate a process heap (the soundness inventory)
Heap relocation is **not** confined to the runner's slice: any path that reaches a rooted collection moves a
process's young objects, so each must either publish that process's root set or decline. All of them share ONE
root set — `Process::regs` + each mailbox message's `payload`/`from`/`token` + dict + saved args + pending-signal
queue, gathered/scattered by `run_rooted_collection` (`../../beam_process/src/process.rs`):
* the three heap-admission doors — `../context/gc.rs::ensure_heap`, `ensure_heap_full`, `alloc_words_rooted`
  (the *running* process; publish → collect → scatter);
* the reduction/yield-boundary door — `../context/gc/reds_door.rs::collect_at_reds_boundary` (refuses
  `is_relocation_exempt` first, then its interval/occupancy gates);
* `Runtime::serve_gc_request` (`gc.rs`), called from the runner's Yield arm and Wait arm;
* **message delivery**, i.e. the *receiver's* heap — `ProcessTable::deliver` → `Process::deliver`
  (`ensure_capacity` on the retry). Reachable from `RuntimeContext::deliver_in_table` (`../context/send.rs`),
  hence from every pass-boundary step: `deliver_gc_completions`, `service_file_requests`, `drain_ets_owner_exits`,
  `deliver_port_messages`, `fire_due_timers`, `dispatch_dist_inbound`/`dispatch_dist_client_inbound`;
* **the exit cascade** — `ProcessTable::enqueue_exit_signal` → `copy_reason_onto` → `grow_receiver_for_signal`,
  reached from `terminate`/`terminate_into`; deliberately **not** gated by `gc_process_enabled`, only by
  `has_unpublished_roots()`/the gate.
All of them refuse when `Process::has_unpublished_roots()` (`relocation_exempt || state == Running`); the
delivery seam adds the receive-side valve `RuntimeContext::delivery_collection_is_unsound`.
The `foreign_reachable` latch is only ever a **descent** short-circuit — with it clear each valve still
classifies every root word — and `close_foreign_reachable` runs only from the three whole-`RootSet` doors.
**Recorded residual:** `serve_gc_request`'s `Err(_) => halt_process_gc` arm discards the `BeamError` itself, so a
genuine collection failure becomes a silent halt (only the `errors` counter and the `halted` set record it).

## The inter-slice window's relocating doors
A `Yield` park with a **dead** FR prefix (`fr_count == 0`) publishes the machine's whole root set into the
process's `Registers` (`park_machine` → `publish_parked_roots`) and leaves the process **not**
relocation-exempt, so every site that delivers into the parked pid between slices may collect over those
registers: `deliver_port_messages`, `fire_due_timers`, `deliver_gc_completions`, `service_file_requests`,
`drain_ets_owner_exits`, `dispatch_dist_inbound`/`dispatch_dist_client_inbound`, all through
`RuntimeContext::deliver_in_table` → `ProcessTable::deliver` → `Process::deliver`, gated by
`Process::has_unpublished_roots()` plus `RuntimeContext::delivery_collection_is_unsound`. An **exempt** park (a
live FR prefix) publishes nothing, so those sites refuse; a dead-FR `Wait` park publishes its whole root set and
is collectable like the `Yield` park above. Inside a slice the process is `Running`, so `slice_begin`'s
`service_signals` and every in-slice delivery take the fragment route.
**The machine is stale from the moment one of these doors collects** until the matching resume scatters
(`resume_from_process_roots` → `RuntimeContext::resync_parked_roots` → `scatter_roots_full`). Nothing in the
window re-reads the machine, so that is sound — and the arm's own ordering preserves it: in the `Yield` arm a
served `serve_gc_request` has already moved the heap, so the boundary door is **skipped outright**
(`if request_collected { … } else if let Err(..) = self.collect_at_reds_boundary(..)`) — running it would
re-publish pre-collection machine words, and the collector classifies an out-of-from-space word as
`Target::Foreign` and leaves it verbatim, so the stale word would survive and the region it names become the next
collection's to-space. **Never move the boundary door ahead of `serve_gc_request` or run both.**
A resume can also fail to scatter at all: `scatter_roots_full`'s length check (`context/gc.rs`) counts the parked
loose terms `Machine::with_roots` re-derives — the two failed-clause argument latches, the pending exception's
reason and the two supplied-trace term latches (`../../beam_engine/src/machine/mod.rs`,
`LOOSE_HEAP_TERMS = 5`) — so a loose field armed or disarmed across the window makes it answer `false` →
`halt_process_gc` → the machine keeps its pre-collection words with no scatter.

## Host-side cached term words (what a heap compaction must relocate)
The node holds **two** term-carrying fields; nothing else under `runtime/` caches an `Eterm` (`scheduler.rs`,
`executing.rs`, `retention.rs`, `arena_census.rs` and the whole ETS/port owner index are term-free — pids, ports,
`MonitorRef`s, handles and counters only).
* `GcState::async_completions[].req_id` (`gc.rs`) — the caller's `{async, ReqId}` term, run through
  `Runtime::stabilize_term` before it is recorded, so it outlives both the caller's young heap and the pooled BIF
  region the option list was parsed from. **That destination is `Runtime::eval_heap`** (`../runtime.rs`, the
  `-eval` store — what many docs call "the pointer-stable arena"; `Runtime::arena: MinimalContext` is the *other*
  region, reached only by `Runtime::alloc_words`). Lifetime: the `erlang:garbage_collect/2` call → the next pass
  boundary's `deliver_gc_completions`, so it genuinely spans passes; the word is **relocatable** (the store copy
  is its only home, and `crate::eval::scratch::pin` latches the statement window so it cannot be rewound). The
  completion tuple built around it (a table-less `RuntimeContext`, so its tuple comes from `Runtime::alloc_words`)
  is transient within the drain.
* `TimerWakes::deliver: FastHashMap<Eterm, TimerTarget>` (`timer_wakes.rs`) — keyed by the timer's **reference
  term**, a boxed 4-word ref thing minted on a **pooled BIF heap** (`../native.rs`, `../native/process.rs`).
  `Eterm`'s `PartialEq`/`Hash` are the raw word (`../../beam_term/src/eterm.rs`), so this key is **address
  identity, not content**: moving the pooled region would dangle it *and* break every later
  `cancel_timer`/`is_timer_ref` lookup (`../timers.rs`). Lifetime: schedule → fire/cancel, i.e. across
  arbitrarily many passes. The payload terms (`from`/`message`) are not here — they are `eval_heap` copies
  (`try_stabilize_escaped`) held in `Timers::pending`.
`GcState::scratch: Vec<Eterm>` is **not** a term store: it is the all-`NIL` placeholder fill `context::gc::bump`
hands `Heap::tuple`, never a root, only ever grown and **never reset** — a compaction can ignore it.
The `{'ETS-TRANSFER', Tid, FromPid, Data}` the owner drain builds shows the same discipline for a term that
crosses a call: the tuple is built on the **pooled BIF heap**, `Data` is the table-owned gift read out of
`beam_ets`'s never-relocating store, and `RuntimeContext::deliver_in_table` **deep-copies** it into the heir's
mailbox, so no word of it survives the drain.

## The off-heap refc-binary reclamation point
The runtime's **one** reclamation point for a process's off-heap refc-binary reservoir is the runner's **park
arm** (`../runner/run.rs` → `RuntimeRunner::reclaim_off_heap_binaries` → `context/gc/bin_vheap.rs::reclaim`), and
it runs in this order: prune against reachability (`beam_gc::for_each_reachable_binary_payload` over the
**process's** registers — relocated by whichever collection above ran, never the machine's term words) →
`Machine::retain_refc_binaries` (compacts handles, never payloads, so a surviving handle's `data_ptr` is
unchanged) → `Process::set_off_heap_binary_bytes(surviving)` (the figure `max_heap_size`'s
`include_shared_binaries` charges).
It runs on the `Yield` arm only — neither the `Wait` arm nor a `BifOutcome::Suspend` park calls it, so a process
that parks in a `receive` keeps its payloads until it is next pre-empted. Its gates (the relocation exemption,
the runtime's GC valves, the bin-vheap trigger, the unscanned-root valve, the reachability walk) keep it off the
per-slice path, and **every gate is a decline, so the residual is a leak, never a dangle** (a declined prune
retains the whole reservoir). Measurement discipline for `erlang:memory(binary)` figures, the capacity-vs-leak
tests and the dead-parked-process residue are [`../CONTEXT.md`](../CONTEXT.md)'s and
[`../context/gc/CONTEXT.md`](../context/gc/CONTEXT.md)'s.

## The pass's exact step order
`scheduler_pass` is one loop body. Before the first round it boots the runtime once (`boot_if_needed`), moves the
process table, the scheduler and the runner state out so the runner can hold `&mut Runtime` while the scheduler
holds `&mut ProcessTable`, absorbs the caller's pre-pass wake list when `resume_first` is set
(`Scheduler::resume_woken`) and absorbs the heap-fragment wake list. Each round derives its own budget,
`max_slices - ran`; a budget already at `Some(0)` ends the pass at the top.
The order **per round** is:
1. `scheduler.run_ready` — the run queue, consuming the round's budget.
2. `note_context_switches` — credits the executed slices to `statistics(context_switches)`; the round's
   `progressed` flag (`round_slices > 0 || !fragment_wakes.is_empty()`) and the port step's timeout derive from
   it.
3. `prune_gc_requests` — drops a `garbage_collect` request whose process died.
4. `deliver_gc_completions` — the `{async, ReqId}` half of `erlang:garbage_collect/2`.
5. `service_file_requests` — the native-service half (`file_server_2` + io-devices, A.9).
6. `service_net_kernel` — the `net_kernel` service's mailbox.
7. `drain_port_monitor_exits` — dead watchers' `(port, reference)` pairs; delivers nothing.
8. `drain_ets_owner_exits` — the ETS owner-death obligation.
9. `absorb_pending_spawns` — a child an `erlang:spawn*` created; **does not** count into `delivered`.
10. `absorb_fragment_wakes` — the fragment counterpart of the table's own wake list; yields the round's `woke`.
11. `drain_driver_monitor_exits` — marks every `driver_monitor_process` target the table no longer holds, so the
    round's port tasks fire the driver's `process_exit`; delivers nothing.
12. `deliver_port_messages` — the **port half**.
13. `drain_nif_thread_sends` — the **foreign-thread NIF half** (A.3), right after the port step and before
    `pump_dist`; one `Option` test with no factory installed.
14. `pump_dist` — the accepting node's handshake/accept step; tally only, falls through.
15. `bind_dist_peers` — binds each freshly established accepted link to its peer's node atom term; falls through.
16. `service_node_monitors` — the `erlang:monitor_node/2` transitions, after the peer binding.
17. `dispatch_dist_inbound` — routes what the accepted links decoded.
18. `pump_dist_client` — the connector's outbound step, which also flushes the controls the process layer's
    `DistSignalRouter` queued; tally only, falls through.
19. `dispatch_dist_client_inbound` — routes what the outbound links decoded.
20. `service_lost_dist_peers` — the **peer-loss half** (A.5), after both inbound drains and before the timer half.
21. `fire_due_timers` — the **timer half**: every `send_after/3` message whose deadline passed plus every fired
    finite `receive … after T` deadline (its own arm also calls `absorb_timer_wakes`).
22. a round that absorbed a fragment wake (`woke > 0`) goes round again, else `timer_wait` decides the pass's
    sleep: a policy allowing one more wait sleeps to the earliest pending deadline and `continue`s, `None` ends
    the pass.
Steps 4-6, 8, 12, 13 and 16-21 count into `delivered` and `continue` back to the run queue; **except**
`pump_dist`, `pump_dist_client` and the two monitor drains, which only add to the tally (if anything) and fall
through. The pass's own outcome is restored unconditionally: the runner state, the scheduler and the process
table go back into the runtime whatever the loop's `Result` says.
**Outbound bytes are written at three moments.** (a) `Runtime::dist_send_reg_send`/`dist_send_message_pid`/
`dist_send_alias` and anything else reaching `DistLinks::route_control`/`route_message`/`route_alias` write
**synchronously inside the call** (`DistConnection::send_control` → `flush_output`). (b) A signal the **process
layer** routes (`DistSignalRouter::route`) only materialises onto the client's scratch heap and pushes onto
`DistClientInner::pending`; `flush_pending` writes its bytes and its **only** caller is `DistClient::pump` —
reached at `pump_dist_client` and from `Runtime::dist_connect`'s handshake loop. (c) `DistNode::pump` drives the
**accepted** links' handshake/reads only and writes no queued control.
So a routed signal queued in a slice is normally written **later in the same pass**, but a round that
`continue`s at any step above, or a pass that ends first, defers it to the **next** pass: no drain at the pass
tail and none before a `break`. The quiescence test sees a link **and** what the wire is still owed, so a
`pending` control keeps a pass alive until `pump_dist_client` flushes it — precisely when its node's link was
**retired** between queueing and the flush, where the flush drops the item with a recorded `last_error`
([`../dist_client/CONTEXT.md`](../dist_client/CONTEXT.md)).

## Re-entrancy: a nested pass cannot run the run queue
`scheduler_pass` **extracts every mutable table before its first round** and restores them unconditionally at the
end (`take_processes` replaces `self.processes` with a fresh empty `ProcessTable::new`, `mem::replace` swaps in a
`Scheduler::new()`, `mem::take` empties `runner_state`). So a pass invoked *from inside a slice* would drive
those three placeholders and, on return, have its writes silently discarded by the outer restore. `run_ready` is
therefore given `&mut table` / `&mut scheduler` / a `RuntimeRunner::new(self, &mut state)` explicitly — the state
is threaded, never re-read from `Runtime`.
* **The guard is cooperative only.** `RuntimeContext::may_pump_scheduler` (`false` in `for_process*`) is checked
  at **exactly one site**, `pump_once`'s first line (→ `Ok(None)` → the typed `blocked()`). No `in_pass` flag,
  no debug assertion, no `RefCell` on the pass itself exists: a nested `scheduler_pass` would not trip anything,
  it would corrupt silently. The port registry's `try_borrow_mut` (`with_port_table`, `deliver_port_messages`)
  and `is_quiescent`'s `Err ⇒ not quiescent` are the only borrow-level brakes.
* **`&mut Runtime` is owned by the `RuntimeContext` for the whole slice** (`ctx.runtime`), so
  `self.runtime.pump_scheduler(..)` from inside `call_bif` does not typecheck as written. A mid-slice caller also
  cannot reach the pass's `ProcessTable` except through `RuntimeContext::mailbox_table`, and during a slice the
  executing pid's `ProcessEngine` is out of `RunnerState::engines` while its `Process` is `Running`, so
  `has_unpublished_roots()` is `true` — every delivery into it takes the heap-fragment route and no collection
  may move its heap.
* **Every service step IS separately callable**, which is the half a nested pass *could* use:
  `service_file_requests`, `deliver_port_messages`, `fire_due_timers`, `deliver_gc_completions`,
  `service_net_kernel`, `service_node_monitors`, `pump_dist`/`pump_dist_client`, plus the non-delivering drains —
  each takes `&mut self` and at most `&mut ProcessTable`; none touches `Scheduler`/`RunnerState` except
  `absorb_pending_spawns`/`absorb_fragment_wakes`/`absorb_timer_wakes` and `drain_ets_owner_exits`. What is
  missing for a service-only nested pass is a driver function that sequences them (that sequencing lives inline
  in the pass's `loop`) plus the wake-list bookkeeping, which belongs to the outer `Scheduler` the context cannot
  reach.

## Distribution (A.5) — inbound validation and the unrouted families
**Every inbound delivery is validated against its own link.** Both production inbound drives (the accepting
node's `dispatch_dist_inbound` → `dist::DistNode::dispatch_inbound`, the connector's
`dispatch_dist_client_inbound` → `dist_client::DistClient::dispatch_inbound`) drain each link through
`beam_dist::ConnectionDriver::dispatch_inbound_checked`, so a control message is checked against **that** link's
own `peer_name()`/`our_name()` identity before it can reach a mailbox: `beam_dist`'s
`Delivery::classify_validated` reproduces the C's per-opcode `invalid_message` shape checks (`dist.c`) plus the
per-link anti-spoof check `dep != external_pid_dist_entry(from)`, the identity interned into the **same**
`AtomTable` the message was decoded against so the node comparison is a raw term `==`. A validation failure is an
`Err` — a **link-level** failure that never reaches `beam_dist::InboundSink`: the drive retires (kills) **just
that link** and records the reason in `DistNode::last_link_error`, never a pass failure; the check cannot live in
`LocalDeliverySink`, built once per pass and shared by every link, which has no peer to compare a `From` against.
`Runtime::dispatch_inbound` (`../dist.rs`, the explicit single-driver seam) deliberately stays on the
*classify-only* `ConnectionDriver::dispatch_inbound`: its caller supplies an arbitrary hand-built driver whose
handshake names need not be this runtime's node identity, so a per-link check there would judge a message against
a *foreign* identity — it is the semantic view, not a wire drive.
**`LocalDeliverySink` routes the message families plus the route-free receipt families; the rest is the honest
`DeliveryOutcome::Unsupported`**, with each unrouted arm's own reason stated at the arm (`../dist.rs`'s
`LocalDeliverySink::deliver`). `beam_process` exposes the **outbound** relationship entry points
(`ProcessTable::link_term`/`unlink_term`/`monitor_term_with_options`/`demonitor`/`send_exit_term`, each routing a
`DistSignal` through the installed `DistSignalRouter`) **and** the **route-free receipt** counterparts, which
install/drop the local record and then follow the C's `break; /* Done */` without echoing the opcode received:

| Inbound family (C arm) | Receipt API used (or why the arm is unrouted) |
|---|---|
| `Link` | **routed** — `record_remote_link(to, remote)` (record-only; the C never echoes one) |
| `Exit`/`Exit2`/`PayloadExit`/`PayloadExit2` | **routed** — `apply_remote_exit_signal(to, from, reason, untrappable)` (`untrappable = true` for the `Exit2`/`PayloadExit2` family) |
| `UnlinkId`/`UnlinkIdAck` | **partially routed** — `unlink_from_remote`/`unlink_ack_from_remote`; an inbound `DOP_UNLINK_ID` with `id != 0` owes a `DOP_UNLINK_ID_ACK` this sink has **no reply channel** to emit, so the link record is dropped and the wire half is reported `Unsupported`. An `id == 0` (or non-uint-id) one is a **wire error**, never a delivery: `classify_validated` refuses it as the C's `invalid_message` before any sink is called, so the pass retires that one link and delivers nothing |
| `Monitor`/`Demonitor` | **routed** — `monitor_from_remote_ref(watched, watcher, reference, kind)` / `demonitor_from_remote_ref(watched, reference)`, both keyed by the **whole** peer-minted wire reference (`RemoteWatcherRef::Remote`, all-immediate, so two peer-minted references never collapse) |
| `MonitorExit`/`PayloadMonitorExit` | **routed** for a DOWN whose reference *this* node minted (the wire image inverts to the recorded `MonitorRef`) — `remote_monitor_down`; a name-targeted DOWN or a foreign reference stays `Unsupported` |
| `GroupLeader` | **routed** — `set_group_leader_from_remote(to, from)` |
| `SpawnRequest`/`SpawnReply` | **still a typed gap** — `beam_process`'s reservation surface exists (`ProcessTable::{reserve_remote,reserve_identity,spawn_reserved,cancel_reservation}`); missing is this crate's **responder** (`{MFA, Opts, args}` → reservation + process install), a pending-spawn table keyed by `(peer, Ref)` for a reply to resolve, and any `DOP_SPAWN_REPLY` channel (`InboundSink` has none) |
| `Other(Dop::Unlink)` | **still a typed gap** — `DOP_UNLINK` is not a v6 opcode; the C's `invalid_message` arm kills the connection, which the link driver owns, so the sink reports "recognised, not routed" |

The scheduler pass drives the two inbound halves from `Runtime::dispatch_dist_inbound` /
`Runtime::dispatch_dist_client_inbound`, **gated by `Runtime::inbound_delivery`** (default **ON**): the pass
early-returns unless the runtime **is** a distribution node **and** a half has an established link, so a
`nonode@nohost` run pays one bool test per pass and allocates nothing.
The **envelope rule**: the mailbox `from` is always an **immediate** (`envelope_from` — the sender peer's node
atom when the wire carried a sender pid, otherwise the word the peer put on the wire, `''` for the plain
`DOP_SEND*` forms); a non-immediate is refused, because `beam_message` gathers only each queued message's payload
as a GC root. There is **no** `{setup,…}`/`{nodeup,…}` control exchange in OTP distribution: after the final `$a`
handshake ACK the link carries `DOP_*` immediately, and node-up is local bookkeeping
(`net_kernel:mark_nodeup`). Evidence and per-family delivery semantics: [`scheduler/CONTEXT.md`](scheduler/CONTEXT.md)
*The inbound family table*, `../../tests/dist_delivery.rs`, [`../dist/CONTEXT.md`](../dist/CONTEXT.md).

## Port-aware scheduling (A.4) — the handoff
The pass runs the **port-less** `Scheduler::run_ready` (its port half, `Runtime::deliver_port_messages` + the one
persistent poller, owns the polling), and discharges the port-aware slice's obligations **at the pass boundary**,
where the port registry is free:
* **covered** — a process that died in the slices just run had its recorded outgoing `(PortId, MonitorRef)`
  `PortWatch` pairs swept by `Runtime::drain_port_monitor_exits`.
* **covered** — `Runtime::drain_driver_monitor_exits(&mut self, &ProcessTable)` runs immediately before
  `deliver_port_messages`, sweeping every live C-driver port's monitor tree (`PortRegistry::note_driver_monitor_exits`)
  and marking each watched pid the table no longer holds, so the same round's port-task probe fires the entry's
  `process_exit`; it is a scan of the live `ProcessTable`, not a second `set_process_exit_observer` (the single
  observer slot stays `EtsExitObserver`), and it defers with `Ok(0)` on a live registry borrow.
* **not covered** — `ProcessTable::port_closed` (a *closing* port notifying its monitors/links). It can only run
  **at the close site, while the id is still in the table**: `beam_io::PortRegistry::close` releases the id
  (`ids.release`) and `beam_process::PortTable::close` is `BadArg` afterwards, so a post-hoc drive from a pass
  boundary is impossible. That site is `crate::port`'s close path (`RuntimePorts`, reached from `beam_bifs`'s
  `PortHook::close_port`), outside this node.
`Runtime::with_port_table(&self, &mut ProcessTable, f)` is the intended **short-borrow seam** for such a caller
(it `Rc::clone`s the port field, so the returned `RefMut` borrows the clone and both tables can be borrowed at
once; a nested borrow stays the typed `unsupported`, never a panic). `beam_process::PortTable`'s seam itself is
**sufficient as-is** — `port_closed`/`monitor_port*`/`terminate_with_ports` already take the two tables as
disjoint parameters — per-port detail in [`../port/CONTEXT.md`](../port/CONTEXT.md).
**Two routes must not be retried; both are unsound by construction:** (1) **holding the
`Rc<RefCell<PortRegistry>>` borrow across `run_ready_with_ports`** — `open_port`/`port_command`/`port_close`
executed *inside* a slice (`RuntimePorts::registry_mut`) `try_borrow_mut` the same cell, so a held borrow degrades
every in-slice port BIF to the typed `unsupported` gap (exercised by the `run <file.beam>` action and the
`event_driver`/`port_recv` fixtures); (2) **`mem::replace`-ing the `PortTable` out and leaving a placeholder** —
`PortTable` has no `Clone` and `beam_io::PortRegistry::open`/`register` `insert` into the table they were given, so
a nested `open_port` during a slice would silently **overwrite** a live port's `Port`/`PortMeta` (an empty
placeholder does not free number 0).
**The crate change that would make the literal routing sound:** the port table must be reachable as a **handle
both the scheduler window and the nested BIF paths share** — e.g. `beam_io::PortRegistry` exposing `ids` as its
own `Rc<RefCell<PortTable>>` (a slice borrows *that* cell and an in-slice BIF borrows it too, failing only on a
genuine re-entry), and/or `beam_io`'s close path driving `ProcessTable::port_closed` at its close site with the
process table in hand, **before** `ids.release(id)`.
**Process termination.** A normally-returning scheduled process is reclaimed by `beam_process`'s schedule-out,
not by any `beam_rt` code: `RunOutcome::Exit(normal(..))` → `Scheduler::run_slice_inner` → `slice_end`/
`slice_end_with_ports` → `ProcessTable::terminate`/`terminate_with_ports`. That runs **only** for a pid the
scheduler dequeued, so a **never-enqueued** `Runnable` process (the `-eval`/`-s`/`-run` action child) is ended by
its own teardown: `../runtime.rs::as_action_process` first copies an `Ok` result onto the pointer-stable store
(`stabilize_term_with` over `source_payloads`, exactly as the runner does for a scheduled `x(0)`, so the term
outlives the heap that built it) and then calls `ProcessTable::terminate` with the `normal` reason. It must be
`terminate`, not a silent `ProcessTable::remove` (which would skip the death itself: the exit observer/ETS-owner
drain, the name/link/monitor teardown), and the result copy is what keeps the returned `Eterm` from dangling
(`../../tests/action_result.rs`). `ProcessTable::spawn` installs a **naked** process (no serial link, no monitor,
no group leader); `Runtime::spawn_process_with_policy` adds only a group-leader entry and `as_action_process`
calls `spawn` + `set_group_leader`, so nothing binds the action child to `init` (the runtime's root process does
**not** trap exits).
**No path in `beam_rt` closes a dying process's *owned* ports.** `ProcessTable::terminate_into` drains the dying
process's links, monitors and port *monitors* and resolves their peers, but never closes a port the process
**owns** (the oracle closes them with reason `normal`). `ProcessTable::port_closed(&mut ports, id, reason)` would
notify a closing port's monitors/links and has no production `beam_rt` caller. Closing exactly one pid's ports
needs an enumeration of them: the authoritative owner table is `beam_process::PortTable`
(`beam_io::PortRegistry::port_table_mut()` / `RuntimePorts::port_table_mut()`), which stores
`owners: HashMap<PortId::number, ProcessId>` with a forward `owner(id)` but **no reverse `owner → [PortId]` index
and no allocated-id iterator**; a caller needs a new reverse index (or id iterator) and then, per owned port, the
runtime close path (`RuntimePorts::close` / `PortHook::close_port`) plus `ProcessTable::port_closed`.

## The region-reclaim pass
`./reclaim.rs` + [`reclaim/compact.rs`](reclaim/compact.rs) + [`reclaim/tests.rs`](reclaim/tests.rs) own the
reclamation of the runtime's two long-lived term regions. **One descent, two mechanisms, two sites.**
**The sites are the two instants at which the runtime is not executing** — the same domain
`Runtime::for_each_root_mut` claims completeness on: the `-eval` statement boundary (`scratch::boundary`) and the
driver exit (`scratch::close`, after the driver's result or raise is known). The corpus is that mutable door's
eighteen host-side families, plus the caller's own live terms (the evaluator's `Env`, and at the exit the
driver's result — or a raise's reason and stacktrace head, lent as **the `outcome` fields themselves** through
`&mut outcome` and never as a copy: a copied slot is rewritten by the move while the word actually handed back
still names the dropped region), plus `RunnerState::for_each_error_live_reason`.
* **Phase 1a — whole-region actions, no word moves.** A **whole-dead** retired region in `eval_heap_retired` is
  dropped (its `Heap::drop` frees the carrier, so RSS really falls), a whole-dead prefix of the CURRENT
  `eval_heap` region is `reset_to(cursor)`, and the engine arena is rewound above the pass's mark through the
  **Tier A** `MinimalContext::heap_rewind` (Tier A validates the epoch, region index, base and used figure
  *before* mutating, so a refusal leaves the arena byte-identical).
* **Phase 1b — the evacuation move** ([`reclaim/compact.rs`](reclaim/compact.rs)). When a retired region is only
  **partially** live and its dead fraction is worth reclaiming (candidates ≥ `MIN_WINDOW_WORDS` = 1 024 words,
  **≥ half** of their words dead by the exact `beam_gc::live_words_in` count over the same corpus, and at least
  one carrier released), its live words are evacuated into **one** fresh destination region through `beam_gc`'s
  `FromSet`/`LinearDest`/`Relocator` and every root is rewritten. The destination is sized from the **store's own
  growth policy** (`Runtime::eval_store_growth_capacity`: keep the current capacity when it already serves the
  live set, else double, floored at `EVAL_STORE_GROWTH_FLOOR` = 256 words and capped at `MAX_EVAL_HEAP_WORDS` =
  256 MiB) and clamped by the total capacity of the regions it replaces, so a multi-region collapse always
  releases memory. **Headroom is a correctness property of a move, not a tuning knob** — `Runtime::eval_heap` has
  exactly one growing consumer, so a destination sized from the live set alone would shrink every other writer's
  bound ([`../context/CONTEXT.md`](../context/CONTEXT.md) *The store's writers that do not grow*). The win comes
  from collapsing N regions into **one**, never from making that one small: a capacity-sized region is a lazy
  `mmap` that costs address space, not RSS.
* **The completeness precondition.** A post-move *value*-descent re-walks the corpus over the OLD ranges, and
  **any hit** keeps **BOTH** — `MoveOutcome::KeptBoth` installs the destination as a *retired* region and leaves
  every source installed, so the rewritten roots stay valid, the counters record the refusal
  (`store_move_declines`/`store_move_last_decline`), and nothing dangles. A hit is **attributed**
  (`compact.rs::verification_owner` → `MoveHitOwner`, published as `store_move_hit`): the caller's `MoveSlots` are
  re-walked one at a time, then the corpus family by family through `Runtime::for_each_root_family_mut` (a cheap
  whole-slice reject, then one walk per root of a hitting slice), so the counter names `CallerSlot(i)` /
  `Family(family, slot)` — or `OutsideCorpus` when no slot reproduces the hit. The attribution runs only on the
  hit path. A post-copy refusal deliberately does **not** fall through to the whole-region drop (`pass` matches
  `Moved | KeptBoth => {}` vs `Skipped | Declined => store_action(…)`), because the descent that just failed is
  known incomplete for exactly those ranges.
* **The decline list, headed by `R_PIN`.** The move declines on `R_PIN` whenever the statement window is
  **lifetime-pinned** (`scratch::pin`/`PinKind::Lifetime`, latched by `Runtime::stabilize_term`/
  `stabilize_term_with` and `port/completion.rs::decode_caller_tag`): such a store may name a word in a region a
  move would evacuate and has no writable slot, so doubt is a refusal at both sites. That refusal is
  **empirically necessary, not conservative-by-accident** — with the pin check bypassed the post-move
  verification **HITS** on `cerl_clauses` and the compile then fails with `system_limit`, i.e. the incomplete
  corpus is real. The other `R_*` reasons are `compact.rs`'s constants (`R_SWITCH`, `R_REASON`, `R_DEST_ALLOC`,
  `R_DEST_CEILING`, `R_CAPACITY`, `R_COPY`, `R_SCAN`, `R_ENUMERATION`, `R_VERIFY_HIT`); the whole-region pass has
  its own on `reclaim_declines`/`reclaim_last_decline`. The switch can only *disable* an action — never a guard.
* **Counters, switch, diagnostics.** `Runtime::region_reclaim_stats` publishes the `[rt-mem] exit` line's
  counters (store compactions/words/regions, the move's declines and hit, the arena's reclaims/words/regions, the
  whole-region declines, and `reclaim_last_decline_header` — the raw header word when the last decline was the
  walk's unknown-boxed-header refusal, cleared by `decline()` so a later refusal cannot leave a stale header). The
  switch is `BEAM_RT_REGION_RECLAIM` (`all`/`store`/`store-whole`/`arena`/`nomove`/`off`) plus
  `Runtime::set_region_reclaim_enabled`; the **latched** configuration is readable through
  `Runtime::region_reclaim_enabled()` (what a pin that must assert per mode reads instead of assuming the
  default). `BEAM_RT_STORE_TRACE=1` ([`reclaim/holder.rs`](reclaim/holder.rs), prefix `[rt-store]`, silent unless
  set, nothing depends on it) is the **holder diagnostic** behind both refusals: it prints the attributed owner,
  classifies the hit word's address, re-descends every corpus slot to name the one that refuses, dumps every live
  process dictionary, and prints `RunnerState::for_each_engine_census`'s per-engine park state.
Thresholds, counter semantics, the two exact `[rt-mem] exit` lines, the A/B table and the six-mode
"what the pass does *not* cost" sweep are [`reclaim/CONTEXT.md`](reclaim/CONTEXT.md).

### What refuses the move — a parked machine's used extent
`store_move_hit` names `Family(ProcessDict, 1)` on the two-statement workload, and **that attribution is a
measurement artifact**: `attribute_hit` reports the first corpus candidate whose descent records a range hit,
while the `BEAM_RT_STORE_TRACE=1` diagnostic re-descends every slot and shows the dictionary slot's own graph
names **no** word inside a candidate range. What actually refuses is the walk meeting an unparseable word,
reachable from exactly **two** corpus slots — `RootFamily::ParkedMachine[0]` and `[8]`, both holding the same word
— and `RunnerState::for_each_engine_census` names the holder: 31 engines, all in a park window
(`parked_published = false`, `live_x = 12`, `stack = 9`, `fr = 0`, `roots = 21`), i.e. the idle blocked set, whose
machines the corpus over-approximates and walks in full, and the words that refuse are `#Ref`-shaped residue at
the **edge of the used extent** whose address a released region once occupied.
Two consequences bound what the move can do on any route that hosts idle processes:
the corpus **over-approximates** a parked machine (the live `x` prefix, the whole Erlang stack, the FR prefix —
**no frame liveness map**), so a dead word in that extent that coincides with a candidate range refuses the move
for the whole pass — the refusal is the honest answer to an extent the corpus cannot prove dead, not a missing
writable slot (a parked machine's roots *are* in the corpus and *are* forwarded); and an "any engine is parked ⇒
decline the move" gate is **not** the fix and was reverted — every engine is parked at a statement boundary, so
it made `store_compactions` structurally unreachable while buying no soundness the post-move verification does
not already give.
The **unknown boxed header** refusal is the same shape, not an untaught subtag:
`reclaim_last_decline_header & TAG_HEADER_MASK` is `0x0a`/`0x2a` and `0x2a & 0b11 == 0b10` is
`TAG_PRIMARY_BOXED` — a boxed **term** word where an object header belongs, reachable from the same
released-region residue. No legal `beam_term::tag::BoxedTag` is ≡ 2 (mod 4), so no child enumeration can be
taught for it: dereferencing it further would treat a pointer as an object — a memory-safety defect, not an
approximation. The refusal stays, and `Walk::UnknownHeader` carries the header **and** the leading term word so
the finding is diagnosable.

### Measured A/B: the pass on a two-statement `-eval`
A single-statement `-eval` cannot show the pass's win: its peak is reached before either non-running site runs.
Two compiles in ONE expression put a statement boundary between them, so the first compile's store face is
reclaimable before the second starts. Release build, `BEAM_RT_MEM_TRACE=1`, the tree OTP-30 bootstrap compiler
first on three literal `-pa` flags, peak `VmHWM` polled from `/proc/<pid>/status` every 20 ms:

| workload | pass | peak `VmHWM` | wall |
|---|---|---|---|
| bare `-eval 'halt(0).'` | on / off | 118 776 / 118 596 kB | 0.5 / 0.5 s |
| `inet_parse` (1 statement) | on / off | 8 197 644 / 8 195 536 kB | 17.2 / 17.2 s |
| `inet_parse` (**2 statements**) | on / off | **13 109 804 / 16 175 512 kB** | 33.3 / 33.3 s |
| `cerl_clauses` (1 statement) | on / off | 382 688 / 382 484 kB | 0.9 / 1.1 s |
| `cerl_clauses` (**2 statements**) | on / off | **520 312 / 554 088 kB** | 1.4 / 1.5 s |

So the pass is worth **−3.07 GB (−19.0 %)** on the two-statement `inet_parse` run and **−33.8 MB (−6.1 %)** on
`cerl_clauses`, and **nothing measurable** on a single statement — exactly what the two-site design implies. The
win is the **arena rewind**, not the store (on the two-statement run `store_compactions=0`,
`store_regions_dropped=0`, and the store chain stays additive); on the **single-statement** run the whole-region
half does its work (`store_words_reclaimed=603 193 147`, `store_regions_dropped=18`,
`eval_heap_retired_regions` **19 → 1**). The pass also releases **address space**: peak `VmSize` on the same
two-statement case reads **43 996 436 kB off against 27 415 256 kB on** (−16.6 GB, −37.7 %), from the whole-region
drops plus the arena rewind (each releases its carrier's reservation), **not** from the destination. The
single-statement case's peak is unchanged in **both** dimensions, because it is reached before either site runs.
Residual, stated honestly: a run retains a region for one of two measured reasons — `R_PIN` on the
single-statement route, `R_VERIFY_HIT` on the two-statement one — never because a move quietly dropped nothing,
and the two-statement run ends with **one more** retired region than the pass-off run because a refused move
retains its destination. The full measured record (exact `[rt-mem] exit` lines, the six-mode "what the pass does
not cost" sweep, artifact identity, the Phase-2 blocker points and the reproduce commands) is
[`reclaim/CONTEXT.md`](reclaim/CONTEXT.md).

## The runtime-level mutable root door — `roots.rs`
`Runtime::for_each_root_family_mut` is the *moving* twin of `Runtime::gather_off_heap_roots` (which only answers
liveness): it hands a visitor the writable `Eterm` slots of every host-side root family, one slice at a time,
tagged with a `RootFamily` (23 variants; the tagged walk visits **18** steps in a frozen order).
`Runtime::for_each_root_mut` is the family-blind form (a relocation rewrites a word the same way wherever it is
stored). Both are `pub(crate)` and both answer `beam_common::Result<()>`.
It **decides nothing**: it collects nothing, retains nothing, prunes nothing — visiting a word has no effect
beyond the writes the visitor makes — and every per-owner enumerator it reaches lives in the owner's own module
(`roots/process.rs` composes `beam_process`'s `gather_roots`/`scatter_roots` pairs plus the mailbox
rebuild, so no sibling crate is edited).
Its **two production callers** are the region-reclaim pass — the whole-region descent (`reclaim.rs::gather_roots`)
and the store move's corpus (`reclaim/compact.rs`, which forwards every slot so a moved region leaves each root
naming the destination) — plus the parity suite in `root_tests.rs`.
**Visitor contract:** `f` may **replace any element** of a slice; it must **never** change a slice's length,
reorder it, or depend on any container's hash/iteration order (an unordered owner buffers its words and scatters
them back positionally). A slice is **one family of one owner**, one owner borrowed at a time. An `Err` **aborts
the walk** and is returned as-is, with the failing owner writing its (possibly half-rewritten) buffer back
**first**: the container and the error leave together, and families already visited keep their writes.
**Completeness — whole only at a non-running site:** the `-eval` statement boundary and a nested run's epilogue.
During a scheduler pass `runner_state`, `processes` and the scheduler are moved out of the `Runtime`, so the
coverage argument and the remaining gap are exactly `gather_off_heap_roots`'s `# Residuals` states (a **running**
machine's root set is published by the in-pass park arm instead, `../context/gc.rs::resources::reclaim`).
`RootFamily::in_value_gather()` is the machine-checkable parity predicate (11 true / 12 false) and carries
`#[allow(dead_code)]` for a **lint** reason alone (the parity suite is its sole reader today); keeping that one
allowance narrow is what stops the rule rotting unnoticed.
The 18-step traversal table, the per-family owner seams, the documented superset's three reasons, the parity
suite (`root_tests.rs`, 6 tests over 10 states) and the deliberately-not-enumerated relocation checklist with its
(a)/(b)/(c) classes are [`roots/CONTEXT.md`](roots/CONTEXT.md).

## The arena allocation attribution census (`BEAM_RT_ARENA_TRACE`) — `arena_census.rs`
`arena_census.rs` answers *which* requests produce `RetentionStats::arena_words`: a power-of-two size histogram
(calls + words per `words.next_power_of_two()` bucket), the ten largest single requests with a
`Backtrace::force_capture` each, a non-overlapping inside/outside-**nested-run-window** word split, and a
per-window word-delta histogram.
It is switched on by `BEAM_RT_ARENA_TRACE` only; `Runtime::new` latches the answer into `Runtime::arena_census`
(`debug_trace::arena_enabled`), so with the variable unset `Runtime::alloc_words` pays one `bool` test and
`Runtime::arena_census_report` is `None` — nothing else runs.
The state is a **thread-local** (`arena_census::STATE`, `RefCell<State>`): `alloc_words` is the arena's only
allocation path (it records every **served** request, after the arena handed the words out) and
`Window::enter(this.runtime)` — opened at the top of `RuntimeContext::run_export_blocking`/`run_label_blocking`,
closed by `Drop` — records only the depth and the entry mark, so the guard never borrows the runtime. A *window*
is one nested run (a fun-taking BIF arm, an `apply`, the `-eval`/scheduled route's blocking entry); a word
allocated inside several windows is attributed to the **innermost** one, while the per-window histogram records
every window's own delta, so its `summed_delta_words` exceeds `arena_words` by exactly the nesting.
`crate::cli::run` prints the report as `[rt-arena]` lines at the retention line's own point (after the action,
while the runtime is still alive), only when the census is on.
Release-build backtraces are useful: `[profile.release] debug = 1` carries line tables, so a frame names a
function + `file:line`; the census drops its own frames and keeps `TOP_FRAMES = 12` per request (heavy inlining
means the deepest engine caller can still be attributed to its enclosing function).
**What it established** (release, the tree OTP-30 bootstrap compiler first on three literal `-pa` flags — a bash
array literal expands to nothing in this sandbox and silently runs the *system* compiler, so write the flags out
and verify `code:which(compile)` in the same `-eval`): on `inet_parse`, `arena_words` ≈ 1.33 × 10⁹ over 7.05 ×
10⁶ calls (mean 188.5 words/call) — **70 % of the calls carry 0.94 % of the words** and **69.67 % of the words
ride in ~101 K requests of 4 097–16 384 words**, so the demand is neither a few huge blobs nor millions of small
cells; 100.00 % of it is inside a nested-run window and **one** outermost window's delta is 77.74 % of the summed
deltas (the compile is a single outermost nested run whose ~350 K inner windows average ~2 074 words each). Both
modules' ten largest single requests are the **same site** — `families::maps::write::put_map_assoc` →
`terms::map::make_map_rooted` → `terms::slack::reserve` → `Machine::alloc_rooted` →
`RuntimeContext::alloc_words_rooted` → `Runtime::alloc_words` — i.e. the engine's ordinary term-build reservation
on the route that has no process heap. Read it as: **no allocation-policy change is implied by the census
itself** (it changes no size, no policy and no lifetime); what it establishes is the input a
recycling-heap/collection decision needs. The size histogram, the control workload, the per-mapping RSS split and
the exact commands are [`reclaim/CONTEXT.md`](reclaim/CONTEXT.md) *Where the retention is, and what the pass
costs*.

## The scheduled path's fixed costs (read before optimizing this crate's glue)
`run_scheduled` (the CLI `run` action) drives **one** target pid after `boot_if_needed`.
A bare `spawn(fun …)` child is created *inside* a slice (`src/native/process.rs::spawn_in_pass` →
`note_pending_spawn` → `absorb_pending_spawns`), and every slice's end absorbs the wake list, so a synchronous
two-process ping-pong never empties the run queue and stays in **one** `run_ready` call — it does **not** re-enter
the pass boundary per iteration. The pass granularity is `RUN_STEP_SLICES = 1000`, so per-pass costs are
amortized ×1000 and **per-slice** costs dominate a long run
(`DEFAULT_REDUCTIONS_PER_SLICE = CONTEXT_REDS = 4000`, `beam_process`).
- **Per pass:** `take_processes` builds a **fresh empty `ProcessTable`** (ten `HashMap::new()` + `Vec`s +
  `RefAllocator`, `ptab.rs`) and `mem::replace`s the runtime's table with it; `Scheduler::new()` replaces the old
  scheduler; `put_processes` is a plain assignment.
- **Per pass: the pass wait is progress-aware, so a ready `run` never sleeps** — the sleep
  (`TimerWait::AtMostWhenIdle`, 25 ms) is taken only for a round that ran **zero** slices with a deadline
  `TimerWakes::can_wake(run_wake_pid)` can wake, is `remaining.min(25 ms)`, and is permitted **once** per pass.
  The measured figures, the anti-spin pins and the bench caveat are the first `Known Issues` bullet below; the
  per-case cost of this path is [`../runner/MEASURED.md`](../runner/MEASURED.md) *The run route's sleep
  accounting*.
- **Per slice:** the machine/context churn (engine moved out of/into its map, a fresh `RuntimeContext` incl.
  `DispatchMemo`, `enter_module`'s `String`, the park hand-off's `copy_from_slice` of the x file plus the stack,
  skipped entirely for an exempt park) is [`../runner/CONTEXT.md`](../runner/CONTEXT.md) *Per-slice (not
  per-instruction) host cost*; the parked machine stays the authoritative root set in an exempt window (see
  [`../../M1_GAPS_GC.md`](../../M1_GAPS_GC.md) *GC-relocation hazards*). This node's own per-slice addition is the
  receive-deadline bookkeeping: `schedule_receive_timeout`/`cancel_receive_timeout`/`take_timer_resume` add+remove
  a timer-heap entry and a `TimerWakes` entry — a finite `receive … after T` pays this on **every** iteration, and
  the `Timers` heap reclaims a non-head cancellation by rebuilding past a watermark (`COMPACT_SLACK` = 64: a
  rebuild runs only once tombstones outnumber live entries), so a cancelled deadline is never a permanent
  tombstone.
  Every pid/Eterm-keyed collection the hot path touches is `crate::hasher`'s word-mixing map, not SipHash
  (`TimerWakes::{deliver,resume}`, `Runtime::{timer_resumes,group_leaders,async_dist,monitor_nodes}`,
  `RunnerState`'s `engines`/`results`/`exits`/`errors`, the mailbox map and the reachability latch);
  `slice_begin`/`slice_end` and the capacity-retaining wake sweep (`drain_woken_scratch_into`) run per boundary.
- **Per slice, O(heap)/O(#mailbox) only when triggered:** the heap-admission seam (`context/gc.rs::ensure_heap`)
  short-circuits on occupancy but still pays a `current_module` **`String` clone** and two valve lookups on *every*
  admission point; the O(live-objects) descent (`any_root_is_unscanned`) is gated per-pid on the shared latch
  (`foreign_reachable`), and the process table's `RuntimeCollectionGate::declines_relocation` is built over the
  **same** latch cell (`ForeignLatch`) so a clear bit answers `false` with no descent (O(1) per delivery) — both
  only on a collection/refusal.
- **Per round (cheap here):** `service_file_requests` allocates one `Vec::with_capacity(1+devices+retired)`;
  `deliver_port_messages` early-returns `Ok(0)` on `has_nothing_to_poll()` with no live port (no poll sleep);
  each dist half is one `Option` match with no node.
- **Out of this node:** the per-slice machine re-entry/resume (`beam_engine`'s `resume_outcome*`) is a sibling
  node's cost.

## Test Strategy
* **In-file suites:** `scheduler.rs` 5 (the `quiescence_and_port_table_tests` module) + `scheduler/tests.rs` 34;
  `gc.rs` 15; `ets_owner.rs` 10 (plus its four `#[cfg(test)]`-only accessors); `arena_census.rs` 9 (bucket
  ranges, the per-bucket call/word fold, the inside/outside split, the nested-window delta fold, the
  retained-top-ten rule, the unpaired-close guard, the backtrace frame split, the empty-census report);
  `retention.rs` 10; `timer_wakes.rs` 2; `reclaim/tests.rs` 11; `root_tests.rs` 6; `../runtime.rs` 31 (inline).
  The sibling `../runner/tests.rs` adds 44 (its own node).
* `scheduler/tests.rs` runs the bounded pass family (step budgets, wake absorption, the port/timer/distribution
  halves, incl. a live connector loopback) over a real `Runtime`; `scheduler.rs`'s in-file pins are the
  quiescence predicate (idle ⇒ quiescent; an armed-but-idle `SigUsr1` `handle` disposition stays quiescent while
  a **caught, undrained** signal is not; no node name ⇒ quiescent, a listener with no accepted link ⇒ quiescent,
  a client dialling a loopback peer ⇒ non-quiescent until dropped) and the `with_port_table` short-borrow seam
  (a nested call is the typed `unsupported: the port table is already in use`).
* `gc.rs`/`ets_owner.rs` units pin the pin/halt/valve sets, `gc_stats`, the escape-store stabilization (a copyable
  term: the copy, the valve closed; an un-copyable one: the honest `SystemLimit`, **no** valve trip), the
  port-watch/drain contracts (incl. the deferred-borrow path) and the five **heir-gift-failure** cases
  (`OwnerIndex::gift_failures` → the next `drain_ets_owner_exits` returns the typed `BeamError` after discharging
  every death, the failed copy leaving the heir untouched).
* `../runtime.rs`'s two in-file transport-copy pins (`an_engine_built_off_heap_binary_survives_the_transport_copy`,
  `the_escape_store_stabilization_resolves_engine_off_heap_handles`) build a genuine off-heap refc binary
  (`beam_codec::term::make_refc_binary`) and assert the empty-source form **flattens** it (byte-identical on-heap
  bits, nothing in the destination table), the machine-aware form reads back **byte-identical** with
  `eval_off_heap` owning a second payload reference, and neither a *resolved* nor a *flattenable* payload trips
  the valve.
* **Measurement is structural, never wall clock:** the pass-sleep and port-block counters are test-only
  per-thread counters (`PASS_SLEEPS`/`pass_sleeps`/`reset_pass_sleeps`, `BLOCKING_PORT_ROUNDS`), and the
  region-reclaim evidence is counters plus `/proc` peaks — never a timing assertion.
* Integration (`../../tests/`): `port_wake.rs`, `proc_lib_start.rs`, `file_server.rs` (oracle-cross-checked),
  `position.rs`, `boot.rs` (the real `start.boot`, all 22 directives), `run_route_sleep.rs`, and the `run`-action
  suites (`cli.rs`, `slice.rs`). Crate-wide routing and the gate commands:
  [`../../TEST_STRATEGY.md`](../../TEST_STRATEGY.md), [`../../tests/CONTEXT.md`](../../tests/CONTEXT.md).

## Known Issues
* **Quiescence blind spot: a retired native io-device is never drained again.** `scheduler_pass`'s native-service
  step drains exactly `FileServices::server()` + `file_services.devices.keys()`; `serve_file_message` with
  `retire == true` does `file_services.devices.remove(&service)`, so a still-live device pid holding a queued
  request vanishes from the drain list — the reply is never delivered, the parked sender never woken, and the next
  pass sees `ran == 0 && delivered == 0 && is_quiescent()`, so `run_scheduled` returns `beam_engine::blocked()`
  (exit 3) though a wake source still has work. `is_quiescent` is not itself wrong; the wrongly-holding premise is
  "no drainable service has work" while a *retired* device does. Reproduced with `epp:parse_file/2` from a
  scheduled `run` process. The `retire` flag itself is a false positive:
  `get_until::EngineContinuation::apply` sets `raised = true` on **any** `Err`, including the typed gaps
  (`unsupported("… continuation suspended in a receive")`, `unsupported("… returned neither {more,Cont} nor
  {done,Result,Buf}")`), not only on a genuine scan raise. Fix belongs in `../native/file_server.rs` (keep
  draining a retired pid, or reproduce the OTP close-handshake ordering), **not** in this node.
* **A *live port* alone keeps a pass non-quiescent.** `PortRegistry::has_nothing_to_poll` is
  `meta.is_empty() && pending_events.is_empty() && pending_port_signals.is_empty() && !tty.is_armed() &&
  !watch.has_pending_signal()`, so while any port's metadata exists `is_quiescent` answers `false` — a `run`-route
  process parked on a message only a port can produce never takes the `blocked()` abort and keeps stepping
  (sleeping `PORT_POLL_SLICE` on a round that ran nothing) until the port answers or `RUN_DEADLINE` expires. That
  is what a driver-side asynchronous reply (an `{inet_reply, …}`-style term drained at the `deliver_port_messages`
  boundary) relies on; the port round is drained **only** there on the scheduled route.
* **The port half's block is per *zero-slice round*, and only the boot drive's policy may shorten it.**
  `port_timeout` is `Duration::ZERO` for a round that ran a slice, else `PORT_POLL_SLICE` inside
  `PortRegistry::pump` (the blocking `epoll_wait(…, 25)`) — **except** under `TimerWait::ImminentWithin(_)` while
  neither port predicate holds: no port owes a reply (`Runtime::awaits_port_reply`, some port with
  `output > input`) and no driver holds one of its own asynchronous operations
  (`Runtime::ports_awaiting_async`), which is what keeps the boot's live-but-idle `inet_gethost` child off the
  poll budget ([`../port/CONTEXT.md`](../port/CONTEXT.md) *Measured*; [`../../PERF.md`](../../PERF.md) *The boot
  path's one live-but-idle port, and the poll budget*). A table that cannot be borrowed stays patient, so the gate
  only shortens a wait it can justify; the `run` route and the `-eval` **bare** receive keep the unconditional
  blocking poll because a child there may speak unprompted (the `-eval` `after T` arm runs under
  `ImminentWithin(0)` and waits port-side itself, in `poll_port_messages`). On the `-eval` route `halt/N` makes
  **zero** passes, so its pump sites are a `receive`, the busy-port wait and a nested cross-module callee's port
  half.
* **`delivered` does not count ordinary Erlang `send`.** Only port/timer/file-service/ETS/dist deliveries are
  counted, so a pass whose only progress is a plain message wake can look like `delivered == 0`.
* **The `run` route's pass wait is progress-aware; a pass that still has slices to run never sleeps.**
  `pump_run_to_completion` passes `TimerWait::AtMostWhenIdle(25 ms)` and `timer_wait` returns `None` for it
  whenever the round that just ended **ran at least one slice** — the arm is
  `(!progressed).then(|| remaining.min(max))`, still gated by `TimerWakes::can_wake(run_wake_pid)`. The guard keeps
  a budget-exhausted pass from sleeping: `run_ready_inner` breaks at the pass's slice budget with the run queue
  still non-empty, so such a pass would otherwise sleep a full 25 ms immediately before its own budget check ends
  it. The anti-spin half is unchanged, pinned by `the_run_route_still_sleeps_to_a_parked_receive_timeout` (a
  `receive … after 20` still sleeps to its deadline and resumes on the timed-out path),
  `the_run_route_never_sleeps_a_poll_slice_while_it_still_has_slices_to_run` (`pass_sleeps() == 0` while the
  target is runnable) and `the_idle_only_wait_policy_skips_the_wait_when_the_round_ran_slices` (the policy table),
  all in `scheduler/tests.rs`. Measured on a `send_recv:plain/1` ping-pong (`receive … after 30000`), N = 20000:
  **0 `clock_nanosleep`** and **6** blocking waits in total (≈137 ms, the boot's own idle rounds) on both the
  default native-baseline tier and `BEAM_RT_TIER=interpreter`. The `tools/bench` figure for the `copy_bound` case
  `send_recv_plain` does not reflect this wait (owner `tools/bench`).
* **A round holding an *unabsorbed heap-fragment wake* is not an idle round, and counting it as progress is what
  keeps a request/response driver workload off the poll slice.** The fragment term
  (`!self.fragment_wakes.is_empty()`) is load-bearing because a delivery whose message did not fit the receiver's
  heap takes `context::send`'s heap-fragment fallback, which owns its own wake channel
  (`Runtime::note_fragment_wake`) **instead of** the process table's wake list and is therefore absorbed *later in
  the same round* — after this decision points. Left out, such a round read as "ran nothing" and blocked 25 ms for
  work it was already holding: on `file_io/ram_file` the reply lands as a fragment on ≈half the ops, so each paid
  one such sleep (a fixed ≈12.9 ms/op against ≈0.37 M retired instructions of real work). `strace -f -e trace=
  epoll_wait` on the `run` route, 100 ops with a 4096-byte payload: **49** `epoll_wait(…, 25)` before, **1** after
  (and that one is the boot's `inet_gethost` request, which returns an event immediately). A genuinely idle pass is
  unaffected (bare boot: 0 timing-out 25 ms polls; a `receive … after 500` still takes its 15 real 25 ms blocks).
  The gate is deliberately **not** widened for an OS child that has not spoken yet — that patient poll is what a
  later unsolicited write needs. Pinned by `scheduler/tests.rs::the_port_poll_blocks_only_for_a_round_that_ran_nothing`
  (the `BLOCKING_PORT_ROUNDS` counter) and end-to-end by `tests/run_route_sleep.rs`.
* **What still costs a decline: a *foreign container that forwards a word*.** The valve has **five** doors —
  `ensure_heap`, `ensure_heap_full`, `alloc_words_rooted`, the park arm's `serve_gc_request` and
  `context/gc/reds_door.rs`'s reduction/yield-boundary door — each running the **per-word** classification phase
  plus, once the pid's foreign-reachability bit is set, an `O(live)` descent of its root-reachable scanned graph
  (`../context/gc_walk.rs`). A container that forwards **nothing scanned**
  (`context/gc.rs::forwards_scanned_word` over `beam_term::layout::for_each_child_slot`) declines nothing — so a
  `make_ref/0`-per-iteration loop keeps its latch closed and its heap flat, and a young term met by a
  `Process::grow_nursery` retried bump stays on the reclaimable heap **before** the valve/arena path; one that
  **does** forward a scanned word declines at every valve call while reachable, and dropping it lets the next full
  admission door retire the latch on a foreign-free descent. Rule, one-way clearing and pins:
  [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md) *A collection is declined only by a foreign container
  that forwards a scanned word*; here `context/gc/refs_plateau.rs` (a ref-minting loop against a reference-free
  control, at N and 2N) and `../../tests/foreign_root_loop.rs` (a loop holding a BIF container result — a
  *scanned* root, since the process-route copy-out re-homes it — with equal collection counts at N = 5 000 …
  200 000). Per-iteration dispatch/profile figures: [`../../PERF.md`](../../PERF.md).
* **An inbound `DOP_SEND_SENDER*` envelope, a `process_info` figure and the `-eval` store's written volume are
  measured/own-state caveats, not defects** — see `../CONTEXT.md` *Host-side term stores*, `../../M1_GAPS.md` and
  `../../M1_GAPS_GC.md` *GC / heap model*. Nothing here invents a figure: `RetentionStats::eval_heap_words` is
  `Heap::capacity()` and **blind to `used()`** (read the written prefix through `Runtime::eval_region_stats`),
  and `RetentionStats::arena_words` is **cumulative demand**, so a rewind does not lower it.

## Notes for Agents
* **Read [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md) before touching the GC/root plumbing** — the
  heap-lifetime rule, the scheduler-moves-the-table rule, the two transport-copy rules, the signal sites'
  `CollectionGate` rule and the relocation inventories all live there; this file states the local consequences.
* **Run the crate gate** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt` (a bare
  `cargo` from the repo root fails — the workspace manifest is `erts_rust/Cargo.toml`, and the sandbox `$HOME` is
  read-only, hence the wrapper).
* **Adding a root family is a three-file change:** the `RootFamily` variant + its step in `roots.rs`, the matching
  arm of `gather_off_heap_roots` (or a documented reason it is a *false* `in_value_gather`), and a state in
  `root_tests.rs` so the parity suite keeps the two implementations from drifting.
* **Adding a term-carrying `Runtime` field is a relocation-inventory change:** decide (a) rewritable root /
  (b) address-keyed / (c) unrelocatable, add it to `roots.rs` if (a), and record it in the inventory above plus
  `../CONTEXT.md` *Host-side term stores*.
* **Never add a second `set_process_exit_observer`** — the slot stays `EtsExitObserver`; a driver-port sweep is a
  scan of the live `ProcessTable` (`drain_driver_monitor_exits`), by design.
* **`is_quiescent` is the pass's termination oracle** — a new wake channel that is not a port, a timer, a dist
  link or a counter the pass tallies must say how it makes the pass non-quiescent, or it will be lost on a
  `blocked()` exit.
* **Measuring this node:** structural counters and `/proc` peaks, never wall clock (a shared host makes wall
  figures unusable); the region-reclaim and arena-census commands are [`reclaim/CONTEXT.md`](reclaim/CONTEXT.md)'s.

## Routing Table
* `scheduler.rs` (+ `scheduler/tests.rs`) → the scheduler-pass family (one pass = processes → ports → timers →
  distribution, in bounded steps; the exact step order, the five `TimerWait` policies, `is_quiescent`,
  `run_scheduled`, the progress-aware wait, port-aware scheduling, process termination, the A.5 inbound family
  table and the re-entrancy account) + the `Runtime::with_port_table` short-borrow seam —
  [`scheduler/CONTEXT.md`](scheduler/CONTEXT.md).
* `reclaim.rs` (+ `reclaim/compact.rs`, `reclaim/holder.rs`, `reclaim/tests.rs`) → **the runtime-side region
  reclamation pass**: Phase 1a's whole-region actions (drop a whole-dead `eval_heap_retired` region, reset the
  current region's dead prefix, `arena.heap_rewind` above a recorded mark — Tier A, never Tier B) and Phase 1b's
  **store move** (evacuate a partially live region into one fresh region via `beam_gc`'s
  `FromSet`/`LinearDest`/`Relocator`, with the post-move verification and its `KeptBoth` obligation), the two
  sites and decline lists, the thresholds, the counters `Runtime::region_reclaim_stats` publishes, the switch, the
  holder diagnostic and the whole measured record — [`reclaim/CONTEXT.md`](reclaim/CONTEXT.md).
* `roots.rs` (+ `roots/process.rs`) → the **mutable** root door: `RootFamily` (the tag + the `in_value_gather`
  parity predicate), `Runtime::for_each_root_mut`/`for_each_root_family_mut` (the 18-step fixed-order traversal
  over every host-side root family), `PendingSpawn::for_each_root_mut`, and `ProcessRootsMut` — the extension
  trait that composes `beam_process`'s `gather_roots`/`scatter_roots` pairs plus the mailbox drain-and-rebuild —
  [`roots/CONTEXT.md`](roots/CONTEXT.md).
* `root_tests.rs` → the **parity suite**, the door's only test caller: the multiset comparison of the two walks
  over ten states, the populated-superset assertions, the stable-engine gate's two sides, the `Err`-write-back
  case and the order-independence case — [`roots/CONTEXT.md`](roots/CONTEXT.md) *Test Strategy*.
* `../runtime.rs` → the `Runtime` façade (boot wiring, code server + module cache, the slice, the spawn API, the
  timer/distribution accessors, `stabilize_term`/`stabilize_term_with` + their destination off-heap table
  `eval_off_heap`, `materialize_args`, `system_info`, `eval`, `format_term`, `refs_mut`, `retention_stats`,
  `eval_region_stats`, `region_reclaim_stats`, the `file_services` field, and the runtime's own
  `init`/`erl_prim_loader` process registrations).
* `gc.rs` → `GcState` (the pin/halt/valve sets + the per-pid `foreign` descent latch — a shared
  `Rc<RefCell<HashSet<ProcessId, FastBuildHasher>>>` cell (`ForeignLatch`) the installed `RuntimeCollectionGate`
  also holds a clone of; a verdict-only set of internal pids, so it probes with the crate's own
  `hasher::FastBuildHasher` and its `#[cfg(test)]` `latch_counters` count every probe/insert/fresh insert — plus
  `walk: WalkScratch`) + `gc_stats`/`GcStatsSnapshot` + `try_stabilize_escaped`/`_with` +
  `foreign_reachable`/`note_foreign_reachable`/`walk_scratch`.
* `ets_owner.rs` → the `ProcessExitObserver` host (`OwnerIndex`/`EtsExitObserver`) +
  `Runtime::drain_ets_owner_exits`/`drain_port_monitor_exits`.
* `retention.rs` → `RetentionStats` + `Runtime::retention_stats()` + the `BEAM_RT_MEM_TRACE` line.
* `arena_census.rs` → the env-gated arena allocation attribution census (`BEAM_RT_ARENA_TRACE`): `ArenaCensus`
  (size histogram, retained largest requests, inside/outside split, per-window delta histogram — a plain value,
  unit-tested without a runtime), the thread-local `State` (window depth + entry marks), the `Window` guard
  `RuntimeContext`'s blocking entries open, and `report()` behind `Runtime::arena_census_report`.
* `timer_wakes.rs` → `TimerWakes`, the timer-reference→`TimerTarget` mirror (address-keyed; see *Host-side cached
  term words*). `executing.rs` → `InMachinePosition`. `dist_connect_tests.rs`, `lost_peer_tests.rs` → the
  distribution accessor and peer-loss drain suites.
* `../runner/` → the `ProcessRunner` (`mod.rs`/`entry.rs`/`run.rs`/`budget.rs`/`reason.rs`/`tier.rs`/`perf.rs`/
  `tests.rs`), re-exporting `crate::runner::{ProcessEntry, RunnerState, RuntimeRunner}` — its own node,
  [`../runner/CONTEXT.md`](../runner/CONTEXT.md).
* `../context/gc.rs` → the engine's heap-admission point (`ExecutionContext::ensure_heap`) that drives this state;
  `../context/gc_walk.rs` → the two-phase valve's reachability walk (`any_root_is_unscanned`/
  `process_roots_are_unscanned` + `WalkScratch`) and the `CollectionGate` host seam (`RuntimeCollectionGate`) —
  [`../context/CONTEXT.md`](../context/CONTEXT.md).
* `../boot/CONTEXT.md`, `../cli/CONTEXT.md`, `../info/CONTEXT.md` → the boot, A.6 and A.8 surfaces layered on the
  façade; `../port/CONTEXT.md` → the port half this node drives; `../dist/CONTEXT.md` +
  `../dist_client/CONTEXT.md` → the two distribution halves; `../native/CONTEXT.md` → the native floor the pass's
  service steps reach; [`../../M1_GAPS.md`](../../M1_GAPS.md) / [`../../M1_GAPS_GC.md`](../../M1_GAPS_GC.md) /
  [`../../M1_GAPS_A3_NIF.md`](../../M1_GAPS_A3_NIF.md) → the per-item gap ledgers.

## See Also
* [`../../CONTEXT.md`](../../CONTEXT.md) → the crate façade (CLI, exit codes, boot, A.5 status) and the
  *Host-side term stores* rule this node's inventories are the local half of.
* [`../../DESIGN_DECISIONS.md`](../../DESIGN_DECISIONS.md) → the rationale behind every invariant above.
* [`../CONTEXT.md`](../CONTEXT.md) → the runtime source root's cross-cutting seams (version/boot resolution,
  the `-eval` store, the debt gate, module literal areas).
