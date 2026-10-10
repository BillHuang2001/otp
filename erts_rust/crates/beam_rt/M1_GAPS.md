# `beam_rt` — Known M1 Gaps & Cross-Node Handoff Detail

Companion to [`CONTEXT.md`](CONTEXT.md).
It holds the current-state per-item detail behind `CONTEXT.md`'s condensed `Known Issues / Intentional M1 gaps`
bullets and its longest cross-node handoffs, split out so `CONTEXT.md` stays under the Context Tree per-file
read limit.
Every gap below is a typed capability gap or a measured own-state difference — never a wrong result.
Present-state documentation, not a change log.

## Dispatch, atoms and module introspection
* **Shared-`AtomTable` cloning.** `call_registered`/`try_call_nif` commit pending names via
  `code_server.atoms_mut()` (`Rc::make_mut`) **only when `PendingInterner::is_empty()` is false**; the
  port/persistent-term/NIF handles keep the strong count at 2+, so an unconditional clone would copy the
  table per call — a call that really interns pays the copy-on-write.
* **The two `get_module_info` divergences.** `exports` comes from `Module::exports` order vs the oracle's
  **global ERTS export-table** order (`erl_bif_info.c:8434-8471`), so the differential test compares it as
  an unordered set; and `get_module_info(M, ...)` raises `badarg` for a module not loaded here (a query
  for e.g. `lists` diverges by module-load state, not by seam).
* **`erlang:module_loaded/1`** is truthful to *this* runtime's loaded set (`preloaded() ||
  CodeServer::is_loaded`; `CodeServer::load` memoizes into the same map). The residual divergence is the
  **boot-set size** only: the oracle loads ~109 modules, this runtime its 22 preloaded `.beam`s, so
  `module_loaded(lists)` is `false` here until `lists` is loaded (`tests/module_loaded.rs`).

## Own-state answers (differ by construction, not bugs)
* `erlang:processes/0`, `erlang:ports/0`, `system_info(port_count)`, the `persistent_term`/
  `get_module_info` figures. `erlang:whereis(init)` **does** agree (the root process; oracle `<0.0.0>`).
* **Minor known divergences (all outside this node unless noted).** `persistent_term:info/0`'s `memory`
  is this runtime's own word total (`count` is exact). The process dictionary keys by raw `Eterm` word
  identity (`beam_process::Dict`), so `erlang:put({a},1), erlang:get({a})` is `undefined` here vs `1` in
  the oracle. `register/2` of an internal PORT id is a typed gap. `process_flag(save_calls,
  <non-integer>)` returns the previous value where the oracle raises `badarg` (`bif.c:1911`).

## Placeholders where a truthful source exists
* **`port_info(Port, input|output)`** is hard-coded `0` (`src/port.rs`) where the oracle reports real
  cumulative per-port byte counts; only the **node-global** `IoCounters` behind
  `erlang:statistics(io)` exist, never per-port totals. The differential `port_info` case filters these
  two items out (`tools/differential/src/corpus/driver.rs`).
* **`NodeIdentity`'s creation is a fixed `0`** (`src/context/bridges.rs`) while a live
  `crate::dist::DistNode::creation()` exists, so an internal pid/port image still encodes creation `0`
  on a wired node.
* **`init:get_argument(bindir)`** answers `error` (no arm) where the oracle returns the real
  `erts-*/bin` path from the resolved code root; `root`/`home`/`progname` **are** answered from the
  runtime's own state (`src/native/init.rs`).
* **Flag-driven `system_info` literals** (`src/info/system.rs`: `port_parallelism`, `break_ignored`,
  `modified_timing_level`, `halt_flush_timeout`; `src/info/limits.rs`: `dist_buf_busy_limit`,
  `delayed_node_table_gc`) report their default because no A.6 CLI arm preserves the driving flag
  (`+spp`/`+B`/`+T`/`-shutdown_time`/`+zdbbl`/`+zdntgc`): `cli.rs::record_unmodeled` drops an unmodeled
  `+flag`, so there is no state to derive from.
* **The key-partition gap.** Some oracle-accepted `system_info` keys are in neither `SUPPORTED_KEYS` nor
  `UNSUPPORTED_KEYS` — `lock_checking`, `lock_counting`, `hipe_architecture`, `ets_realloc_moves`,
  `ets_always_compress`, `snifs`, `port_tasks`, `io_thread`, `scheduling_statistics`, `compat_rel`,
  `max_integer` — so this runtime raises `badarg` where the oracle answers a value. Classifying them is a
  `src/info.rs` change; the per-key analysis is in [`src/info/CONTEXT.md`](src/info/CONTEXT.md).
* **`process_info(P, last_calls)`** answers `false` although the runtime holds the real `save_calls` flag
  (`beam_process::Process::save_calls`).

## Process-control families (`src/native/`)
`erlang:monitor/2,3` serves a **process** target (a pid, a registered name, or a `{Name, Node}` pair on
this node) and a **port** target: a port monitor is recorded on the runtime's live port table
(`PortRegistry::port_table_mut` → `ProcessTable::monitor_port_term_with_tag`), the same table
`open_port/2` allocates from, so its `{'DOWN', Ref, port, Port, Reason}` fires when the port closes.
`erlang:monitor/3`'s `{tag, T}` option is served on both paths for an **immediately-representable** tag;
a heap-resident tag is `beam_process`'s typed `error::unsupported_tag`. The two remaining monitor gaps
are a **named** port monitor `monitor(port, Name)` (`error::named_port_monitor` — the local registry
resolves names to processes only) and a remote `{Name, Node}` monitor (contract A.5). `exit/2,3` matches
the oracle except an internal **port** target answers `true` without closing the port, and a
foreign/decoded reference or ETS id is `badarg` vs the oracle's `true`. `spawn_opt`'s
`{scheduler, N}`/`{async_dist, B}` stay `unsupported`; every other option is applied for real.

## ETS ownership
`ets:new/2` records the calling process as the table owner (`beam_ets::Table::set_owner`) and installs the
parsed `{heir, …}`; `ets:info(T, owner)`/`heir` render the recorded pids (`undefined` when unset),
`ets:setopts/2`/`ets:give_away/3` are registered, and `private`/`protected` access is enforced per
owner. A dead owner's table is reclaimed through the `beam_process` `ProcessExitObserver` seam: the
observer records the dead pid and `Runtime::drain_ets_owner_exits` releases every table a dead pid owned —
dropped, or handed to its heir with the oracle's `{'ETS-TRANSFER', Tid, FromPid, Data}` (silent for
the `{heir, Pid}` 2-tuple form, which carries no data; an arity-3 `{heir, Pid, undefined}` *does*
transfer `undefined`). `tests/ets_owner.rs` oracle-cross-checks the flows.
**The authority is the public owner→handle roster, so an *unnamed* table is reclaimed too:**
`beam_bifs::TableRegistry::handles_owned_by(TableRegistry::owner_of(dead))` lists every live table whose
recorded owner is the dead pid (its handle is the registry key, reachable from no public term otherwise).
The index this runtime built at `ets:new/2` time is unioned in only for what the roster cannot supply — a
`named_table`'s **registered name**, released with `TableRegistry::remove_name` (an unnamed table has
none). An unnamed table's tid in the transfer message is this runtime's own ref thing
(`src/runtime/ets_owner.rs::tid_ref_thing`, a fresh `RefAllocator` walked to the handle, because the
runtime's ref id space differs from `beam_bifs`'s).
**The remaining live gap is `ets:setopts/2`'s gift data.** `beam_bifs`'s `src/ets/adopt.rs::setopts_2`
installs an `{heir, …}` option through `beam_ets::Table::setopts`, which stores the gift term **verbatim**
(its own "does not copy" contract) where `beam_ets::Table::install_heir` (a table-owned deep copy) is
meant. The drain hands that term to a `{'ETS-TRANSFER', …, Data}` message **after** the owner is dead, so
a term still aliasing the dead process's heap would be garbage; the creation path is safe (the
`ets:new/2` build path copies). The fix belongs in `beam_bifs` (or in `beam_ets`, by making `setopts`
copy); `Runtime::own_heir_gift` mitigates the `ets:new/2` path only, and it **records** a failed
`Table::install_heir` on the owner index (logged, then returned as the typed error by the next
`drain_ets_owner_exits` *after* every death is discharged), so a failed copy is never silently dropped.
A monitor whose watching process is dead is swept by the same drain (`ProcessExit::ets_monitors`).

## Port monitors (A.4)
`erlang:monitor/2` on a port records an incoming entry on the port; when the watcher dies first the
runtime sweeps it through the same observer payload (`ProcessExit::port_monitors`) +
`drain_port_monitor_exits`, so a port never keeps a monitor for a process that no longer exists
(`tests/port_monitor_cleanup.rs` is the discriminating white-box pin; a *surviving* watcher's monitor is
kept). `erlang:port_info(Port, monitors)` reads the always-empty `PortMeta.monitors` — the oracle returns
`[]` even with a live watcher, so that matches and must not change.

## `process_info/1,2`
The heap items read the target process's own policy (`Process::collector().policy()`), so a `spawn_opt`
min-heap / `fullsweep_after` / `max_heap_size` is observable. Two `max_heap_size`-map divergences remain:
the key order here is `kill, error_logger, include_shared_binaries, size` (oracle: `error_logger, include_shared_binaries, kill, size`)
and the per-map flags are not honoured. ~28 items are answered from this runtime's real state (catalog in
[`src/native/CONTEXT.md`](src/native/CONTEXT.md)); the four **execution-position**
items (`current_function`/`initial_call`/`current_location`/`current_stacktrace`) only for a scheduled
process whose entry is a module export, and any other valid oracle item it cannot answer (`catchlevel`,
`magic_ref`, `priority_messages`, `binary`, ...) raises the typed `unsupported`. `registered_name`'s
wrapping and `process_info/1`'s fixed `pi_1_keys` order are exact; `total_heap_size` is **466** (vs the
oracle's 233) and `memory`/`reductions`/`group_leader` are this runtime's own figures. A pid not in the
table is `undefined`; an unknown item or a non-pid is `badarg`.
**`trace`/`message_queue_data`/`last_calls` read real state.** `trace` serves the **live** per-process ERTS
tracee-flag bitmask (`0` untraced, `1024` after `erlang:trace(P,true,[call])` — the oracle's own integer
shape, translated from this runtime's internal bits by `src/trace.rs::flags_to_atoms`'s table).
`message_queue_data`/`last_calls` report this runtime's real placement/saved-calls state
(`on_heap`/`false` — the queue is heap-resident and no calls are saved), documented rather than framed as
a missing subsystem. The one divergence left: `process_flag(message_queue_data, off_heap)` is
accepted-and-ignored, so such a process still reports `on_heap` (per-process storage in `beam_process`
is a cross-crate handoff).

## Printing
The registered `beam_bifs` `io:format` `~w`/`~p` printer rejects a non-byte-aligned bitstring (or a
container past ~80 columns) with `badarg`; the corpus reads such values via `bit_size/1`/`is_list`. A
segment wider than 64 bits is *unsupported*; a bignum is `system_limit`.

## Scheduler / runner
A blocked receiver is resumed by a sender (send→wake→resume), a due `send_after/3` timer, or a fired
`receive ... after T` deadline. `src/runner/` returns `RunOutcome::Yield` once the per-slice budget is
spent (one reduction per heap-admission point, `ensure_heap`), keeping the parked `Machine` at
`current_code` and letting the scheduler re-enqueue — so an allocating loop is pre-empted and a second
runnable process interleaves (`tests/preempt.rs`). Gaps: a loop **without** an allocation has no
admission point; a `receive ... after T` whose delay is `after infinity` or is not a statically
positive integer (a Y-register slot) still parks; a **capability gap is uncatchable** on the runner path.

## Execution tier (`src/runner/tier.rs`)
The runner's execution tier defaults to `JitMode::Auto`: a process's **first** entry runs on the native
baseline JIT exactly where `jit_is_switch_safe` proves the entry module free of a module-context switch
(no `Apply`/`ApplyLast`, every `CallExt*` target provably synchronous), and on the interpreter otherwise.
`BEAM_RT_TIER` (`auto` default / `baseline` / `interpreter`) overrides it. The tier chosen at that first
entry is **remembered on the process** (`ProcessEngine::tier`), and every later slice — a pre-emption on
the reduction budget or a `receive` park — resumes on it through `Machine::resume_outcome_with_tier`, so
a native-tier process is not demoted to the interpreter mid-life (a `Tier::Interpreter` process is
unchanged). Measured: **10.648 s → 0.079 s** on a non-allocating tail loop, **0.296 s → 0.205 s** on
allocating tree recursion and **0.180 s → 0.086 s** (≈2.1×) on the committed `preempt:alloc_loop/1`
allocating loop, with the full suite at mismatch 0 on the new default. Residual engine gap: the JIT
driver has no in-machine module-context switch; detail in [`src/runner/CONTEXT.md`](src/runner/CONTEXT.md).

## NIF (A.3)
`load_nif/2` really `dlopen`s the shared object and runs the C NIF; a load failure is the oracle's
**returned** `{error, {Reason, Text}}`, never a raise. `NifHostImpl` implements the runtime-independent
hooks — `heap`/`intern_atom`/`atom_name`/`self_pid`, `term_to_binary`/`binary_to_term` (ETF),
`existing_atom`, `unique_integer`, `system_info` (whose `scheduler_threads` is this runtime's **real**
normal-scheduler count), `alloc_env`/`free_env`, and `make_copy` as the **identity** — and, through the
borrowed `NifHostServices` provider (`RuntimeNifServices`: the runtime's reference-id allocator,
reference registry, name table and port registry) threaded into the **per-call** host, `make_ref`,
`whereis_pid`, `is_process_alive`/`is_current_process_alive`, `is_port_alive` and `port_command`. The
**load/unload** host lives in `Runtime::nifs` (so it cannot borrow the runtime) and is built with
`services = None`, so every provider-backed hook stays the trait default there.
The hooks still at the trait default each need runtime state the host cannot reach: `priv_data` (the
library's own `void**` is kept by `beam_abi`'s loader, so the host answers null — the C's value for a
library that set none); `consume_timeslice` (`false`: the provider holds the process table immutably and
the hook's `bool` cannot express "unsupported"); `send` (`None` ⇒ `enif_send` answers `0` — the most
damaging remaining gap, since it needs a mutable process table plus a copy context); `whereis_port` (no
atom can ever name a port here — `erlang:register/2` refuses a port — so `None` is the *provable*
answer); `monitor_process`/`demonitor_process` (monitors are keyed by a **process** watcher and a NIF
resource has no process identity); `select` (a new poller/notification subsystem, not a wiring gap);
`schedule_nif` (`badarg`; this runtime has no dirty schedulers and no scheduling trampoline); and
`dynamic_resource_call` (needs a per-library `{module, name}`-keyed resource-type registry).
A second `load_nif` for a loaded module is `reload`; a NIF returning a non-term is caught by
`try_call_nif` as a typed error, never an abort.

## Funs
`fun M:F/A` builds a real `FunKind::External` (`EXPORT_EXT`); a **compiled-module** `make_fun3` closure
gets its five identity fields from `FunIdentityBridge`, so `term_to_binary/1` is byte-exact
(module-scoped to the **caller**). An **`erl_eval` lambda** stays the honest `unsupported: local_fun`
(its oracle image closes over `erl_eval.erl` internals); no wrong encoding is emitted.

## Ports, drivers and the send family (A.4)
The read path builds one `{Port, {data, Data}}` per driver read, at the width the child wrote: a
`binary` stream delivers a `binary` up to `beam_io`'s `READ_CHUNK` (64 KiB) per `read(2)`, a `list`
stream a char list. The child's **stderr is inherited** from the emulator by default and is never port
data; `stderr_to_stdout` makes descriptor 2 an exact duplicate of descriptor 1, so its diagnostics
arrive as port data interleaved in true write order. `connected` flips to `false` only at
`port_close/1`; `os:cmd/1` stays the runtime-native host spawn.
A `{spawn_driver, Name}` port's `driver_output` **is** delivered to its owner as `{Port, {data, Data}}`,
and a driver-emitted `erl_drv_output_term`/`erl_drv_send_term` term reaches the connected process or the
explicit destination (the `TermQueue` drain is wired end to end). `driver_select` is real, and
`driver_set_timer` arms for every attached port (the C substitutes a no-op `no_timeout_callback` when
the entry declares no `timeout` slot, so `-1` only fires for an invalid port or an overflowing
deadline). An `OUTPUT` port task always drains the port's output queue through the single write path
(the entry's `outputv`/`output`) and *then* runs the entry's `ready_output` as the write-**readiness**
notification, so a queued `port_command` buffer reaches the driver whether or not the driver armed a
writable fd. The driver's `control` callback **is** routed behind `erlang:port_control/3`, and
`ERL_DRV_USE`'s `stop_select` **fires** on the cancel/deselect handback.
Typed gaps: `driver_async` (no async pool; `Runtime` is `!Send` and the `invoke`/`free` pair is
`unsafe`); a `control` reply the driver answered from **its own** memory (needs a safe copy helper on
`beam_abi`); an OS child (`os_pid` is `undefined` for a driver port, and `take_exit_status` is always
`Ok(None)`); a built-in (`spawn`/`spawn_executable`/`fd`) port's `port_control/3` answering `[]` where
the oracle raises `badarg` (a gap owned by `beam_io`'s default `Driver::control`); `port_info`'s
`input`/`output` (the placeholders above); and a driver arming **more than one** registration per
direction (the typed `unsupported`).
The six `erl_ddll` registry BIFs (`loaded_drivers/0`, `try_load/3`, `try_unload/2`, `monitor/2`,
`demonitor/1`, `info/2`) stay typed `unsupported`: `loaded_drivers/0` would report this runtime's own
(empty) driver set where the oracle lists its statically-linked `tcp_inet`/`udp_inet`/`ram_file_drv` — a
by-construction own-state divergence never worth fabricating, and the corpus covers none of them. With
no `erl_ddll:load_driver/*`, a `Name` doubles as the *search* (a `/`-containing name is the object path,
driver name = its file stem; else `<name>.so`).
The send family serves `erlang:send/2` and `erlang:'!'/2` (the `SendBridge`); `erlang:send(Port, Data)`
needs the port write path and the distributed `{Name, Node}` send is contract A.5 — both raise
`unsupported: …`, never a fabricated value.

## Distribution and the node (A.5)
`-name`/`-sname` make the runtime a distributed node: `DistNode` binds a wildcard listener, publishes to
`epmd` best-effort (a failure is recorded, never fatal) and completes the server-side `'N'` handshake
against a real OTP peer (`tests/dist.rs`). Gaps: no `net_kernel` protocol; the inbound
link/exit/monitor/alias `DOP_*` families stay typed `Unsupported` (no remote-peer seam in
`beam_process`); `node/0` and the node-valued `system_info` items do **not** reflect the live node
(`NodeIdentity` carries the creation-`0` placeholder above). `DOP_SEND`/`DOP_REG_SEND` already reach
local mailboxes through `LocalDeliverySink`, gated by `Runtime::allow_inbound_delivery` (default OFF, so
the differential corpus is byte-identical); a live-peer loop past the handshake needs the
`FragmentAssembler` receive-buffer coupling.

## Timers
`send_after/3` answers a `#Ref<>`; `cancel_timer(Pending)` the remaining ms,
`cancel_timer(Fired|Cancelled|unknown-ref)` `false`, `cancel_timer(non-ref)`/a bad time or destination
`badarg`. `send_after/3`/`cancel_timer/1` are **native-floor** capabilities; `send_after/4`,
`cancel_timer/2`, `start_timer/3,4` (`{abs, false}`; `{abs, true}` is the typed gap),
`function_exported/3`, `link/1`/`unlink/1` and `os:set_signal/2` come from the `RuntimeHooksBridge`.
Unimplemented: `erlang:system_info/2`, `read_timer/1,2`, and the roster keys this bridge does not serve
(the `erl_ddll` registry family, `delete_module/1`/`purge_module/2`,
`port_get_data/1`/`port_set_data/2`, `term_to_iovec/1,2`, `erlang:is_builtin/3`, `bump_reductions/1`,
`loaded/0`, …); each is the typed `unsupported` gap.

## Memory and statistics
`erlang:memory/0,1` (the `MemoryHook` seam) is adopted: `RuntimeMemory` (`src/context/memory_hook.rs`)
returns the nine oracle keys in oracle order with `total = processes + system`, every figure
**measured** (process heaps from the `ProcessTable`'s collectors, atoms from the shared `Rc<AtomTable>`,
binaries from `beam_alloc::registry`, `code` from loaded `.beam` sizes, `ets` from the `TableRegistry`,
`system` = those plus the remaining allocator live bytes excluding `Standard`/`EHeap`), snapshotted at
construction (built only on the `uses_memory_hook` arm); `memory/1` answers in the requested order and
`badarg` for an unknown key.
`erlang:statistics/1` is **partly** served: `run_queue`, `run_queues`, `run_queue_lengths(_all)`,
`total_run_queue_lengths(_all)`, `wall_clock`, `reductions`/`exact_reductions` (a node-global cumulative
counter bumped at the real reduction charge site), `context_switches` (a real process-switch counter),
`io` (`{{input,N},{output,N}}` from real port byte counters), `garbage_collection` (`{GCs,
WordsReclaimed, 0}` from the live collectors' counters), `runtime` (`{Total, SinceLastCall}` user-CPU
milliseconds from the runtime's one long-lived `beam_io::cpu_time::RuntimeClock`, whose anchor persists
for the runtime's whole lifetime) and `scheduler_wall_time`/`scheduler_wall_time_all` ⇒ `undefined`
(never enabled). Gaps: `statistics/0` is not served, and on `statistics/1` the keys naming per-scheduler
state this runtime genuinely lacks (`total_active_tasks`/`total_active_tasks_all`/`active_tasks`/
`active_tasks_all` — a scheduler's current-process flag — and `microstate_accounting` — no MSACC
accounting) are the typed `unsupported`; every other `RuntimeHooks` family is unserved too.

## GC / heap model
`DEFAULT_HEAP_WORDS` is `1 << 24` (16 Mi words of lazily-mapped address space). The term-construction
heaps (the BIF-heap pool + the `-eval` heap) are plain bump arenas sized `config.heap_words`; `+hmax N`
is a **real** cap on that capacity (`runtime.rs::term_heap_words`). The `system_limit` translation is
applied **after** the copy engine's own retry (so `deliver_message` still matches the **raw**
`AllocKind::Message` to trigger the heap-fragment retry). This crate **does** drive `beam_gc`: a *scheduled* process allocates on its own young heap and is
collected at `ExecutionContext::ensure_heap` (armed by the slice's `BudgetContext`, roots = `x[0..live)`
plus the Erlang stack), pinned by `tests/gc_collect.rs` and reported by `Runtime::gc_stats()`; every
other context is a no-op on the unbounded arena. The limits are honest and measured: a workload that
allocates only through `ExecutionContext::alloc_words` (no `test_heap` admission point, as real Cowboy
`cowboy_bstr`-style code does) always falls back to the pointer-stable arena and is never collected
(`collections: 0`, `fallbacks` ≈ 620·N at N=10 000), `Machine`'s float-register file is not mirrored by
this crate's root publish (so a module with a float-family instruction is never collected over), and a
failed collection halts that process's collection rather than failing the program.
**The term-heap capacity is still the hard limit** for anything not collected: both exhaustion classes
become `error::system_limit` via `error::enforce_term_heap_limit` — `AllocKind::Heap` (the per-process
bump allocator) **and** `AllocKind::Message` (the copy engine's destination-heap exhaustion, classified
by `beam_message::is_message_heap_exhaustion`) → CLI `beam_rt: system_limit`, exit 1.
**On-demand heap growth is adopted only on the `beam_process` message-delivery path.** `Process::deliver`
retries a refused `AllocKind::Heap`/`Message` copy after `Process::ensure_capacity` grows the nursery to
fit it; a bounded `HeapPolicy::max_heap_words` makes that growth refuse fail-fast, and the refusal is what
reproduces the raw `AllocKind::Message` that `deliver_message` keys its heap-fragment fallback on (an
unbounded receiver would instead absorb the payload into its grown nursery). A **relocation-exempt**
receiver skips the collect-and-grow retry entirely. `beam_rt`'s own allocation
paths still do **not** grow, so heap exhaustion there still raises `error:system_limit`: the engine `ensure_heap` hook is a
no-op and BIFs build terms on a `beam_alloc::Heap` (`BifContext::heap()`) caching raw `hp`, so the live
set is invisible to `roots`; the bounded heaps (`Runtime::bif_heaps`/`eval_heap`) are bump arenas that
never move. To adopt: a reservation-capable `Heap` + re-derivable top/`hp`, `beam_engine` `alloc_words`
re-basing, and `beam_bifs` per-object `hp` re-fetch.

## Heap switches
`+hms`/`+hmbs` set a `beam_gc::HeapPolicy` rounded through the growth table (`+hms 1000` → 1598),
`+hmax*` the `max_heap_size` map; a malformed value is exit 2 where the oracle exits 1.

## Cross-node handoff detail
* **The differential gate.** **No case disagrees**, and the sole `unsupported` is an honest capability
  gap, never a `mismatch`: `wire/new_fun_ext_encode` (an `erl_eval` lambda's `NEW_FUN_EXT` identity —
  `erl_eval` internals, structurally unclosable; recorded in `tools/differential/known-gaps.txt`).
  `erlang:statistics(runtime)` is served, so its case agrees. The live rollup, the allowlist state and
  the `gate.sh` exit code belong to [`../tools/differential/CONTEXT.md`](../tools/differential/CONTEXT.md).
* **A.7/A.9 boot — COMPLETES.** `Runtime::boot_from_script` runs the real `bootstrap/bin/start.boot`
  (OTP 30 / erts-17.1, **22 instructions**) to `{progress,started}` (the CLI `-boot` action exits `0`),
  so the differential case `boot/proc_lib_start_link_blocks_the_boot_path` **agrees** with the oracle;
  `-boot FILE` drives it (`src/cli.rs::drive`), a plain start keeps the preloaded-only floor. Remaining
  cross-node gaps: (1) **`beam_engine`** — on the non-blocking entries `fold_outcome` folds a callee's
  `Wait` into `error::blocked()`; (2) **`beam_bifs`** — whatever the started applications still need.
  Detail: [`src/boot/CONTEXT.md`](src/boot/CONTEXT.md).
* **The native `file_server_2` service** (`src/native/file_server.rs`) answers the
  `{'$gen_call',{Pid,Tag},Request}` / `{file_request|io_request,From,ReplyAs,Request}` wire and the
  server/io-device request surface `lib/kernel/src/file.erl` issues (`{open,…}`, `read_file`, `list_dir`,
  `delete`, `rename`, `make_dir`, `del_dir`, `read_link`, `make_link`, `make_symlink`, `altname`,
  `get_cwd` + `close`/`sync`/`datasync`/`truncate`/`{position,At}`/`{get_chars,…}`/`{put_chars,…}`);
  every other command is the typed `unsupported`, and a `From` tag that is a process **alias** is the
  typed `unsupported` too. The `{get_until,…}`/`{get_line,…}` io requests **are** served
  (`src/native/file_server/{get_until,get_line}.rs`), so `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4`
  and `file:read_line/1` complete and agree with the oracle (`tests/io_scan.rs`). Wire tables + per-request
  gaps: [`src/native/CONTEXT.md`](src/native/CONTEXT.md).
* **`beam_process` / `beam_message` split borrow.** The split accessor exists:
  `Process::heap_and_off_heap_mut()` (`beam_process/src/process.rs:579`) returns
  `(&mut Heap, &mut OffHeapTable)` in one borrow, and `Process::heap_and_mailbox_mut()` returns
  `(&mut Heap, &mut Queue)`, so the `beam_message::Fragments::move_into` call at receive borrow-checks.
  The remaining blockers for adopting it here are an **owning drain for the wake
  list** and a `!Send`-friendly `SchedulerPool` for SMP adoption (`Runtime` holds `Rc`/`RefCell` tables
  and `*mut Eterm` heaps, so `beam_process`'s `unsafe impl Send` escape is unavailable here). Without
  that drain this crate keeps composing the public fragment primitives directly and
  `MailboxState::remove_at` retains a removed message's `Fragments` so a just-bound payload stays valid.
  The `*_with_ports` scheduler forms exist, but this crate does not route its slices through them
  (holding the port-registry borrow across interpreted code turns every port BIF into the typed
  `unsupported`; `Runtime::with_port_table`), so the pass discharges the same obligations at its round
  boundaries through the exit observer (`src/runtime/ets_owner.rs`).
* **`beam_engine`** — (a) an **in-machine module-context switch** in the baseline JIT driver (a
  `Step::Enter` there is the typed `unsupported`; hence `runner::tier::jit_is_switch_safe` refuses an
  `Apply`/`ApplyLast` or a non-provably-synchronous `CallExt*`); (b) a `MinimalContext` **arena
  word/byte accessor** so `erlang:memory/0,1`'s `processes`/`processes_used` can be real on the
  `-eval`/single-pass path; (c) an **allocation-admission root-set hook** — `ExecutionContext::alloc_words`
  carries no root set, so an `alloc_words`-only workload is never collected.
* **`beam_bifs` / `src/context`** — feed `NodeIdentity` from `Runtime::dist_node()` so `erlang:node/0` and
  the node-valued `system_info` items reflect a live `-sname`/`-name` node.
* **Distribution, phase 2** — the inbound link/exit/monitor/alias `DOP_*` families need a
  `beam_process` **remote-peer** seam (plus an alias table); `DOP_SEND`/`DOP_REG_SEND` already reach
  local mailboxes, and a live-peer loop past the handshake needs the `FragmentAssembler` receive-buffer
  coupling.
* **`beam_io` / `beam_abi` lifecycle.** Finish the driver-port lifecycle (`driver_async` needs a
  `beam_abi::DriverHost` seam that can fire `ready_async`, since `beam_rt` is `#![deny(unsafe_code)]`; a
  per-registration `ErlDrvEvent` identity). `erl_drv_monotonic_time`'s phase differs because
  `beam_abi/src/driver/drv/mem.rs` reads `CLOCK_MONOTONIC` from the system epoch where the oracle returns
  the Erlang monotonic time (a process-global origin shared with `beam_bifs`' `time.rs`).
* **`beam_bifs` — the `ets:setopts/2` heir gift copy and the tid-ref mint.** The owner→handle roster
  exists, so a dead owner's *unnamed* table is reclaimed (see the ETS section). Remaining gaps:
  `ets:setopts/2` installs `{heir,…}` through the non-copying `beam_ets::Table::setopts` where
  `Table::install_heir` is meant (`src/ets/adopt.rs::setopts_2`; the runtime can mitigate only the
  `ets:new/2` path via `Runtime::own_heir_gift`), and this runtime mints an unnamed table's tid ref via
  `runtime::ets_owner::tid_ref_thing` (a shared `beam_bifs::access::make_ref_thing` or
  `MonitorRef::from_id(u64)` would remove that loop).
* **`beam_gc` — peak RSS still grows in N.** The retained set grows because `beam_gc`'s `resize_nursery`
  feeds `young_after_collection` the **pre-reset** used count, and `young_after_collection`
  (`src/policy.rs`) steps one growth-table entry upward (`next_words(…, 1)`) rather than settling on the
  C's fixed point, so a process under sustained allocation steps up once per collection and never reaches
  a steady state (`collect_major`'s transient is ≈2×). `memory(system)` is flat, so no `beam_rt` change
  is the lever — the sizing policy is `beam_gc`'s.
* **`beam_io` — the port table sits behind one `RefCell`.** `Runtime::with_port_table` is the short-borrow
  seam and the quiescence predicate reads `PortRegistry::has_nothing_to_poll`, but `beam_io::PortRegistry`
  keeps its `ids` port-number table private behind the same single `RefCell` as the live ports, so the
  runtime cannot hand the process table and the port table out as disjoint borrows and therefore cannot
  route its slices through `Scheduler::run_ready_with_ports` (a cross-crate handoff, not a bug); a split
  accessor over the id table would unblock it.
* **`beam_bifs` registry key allocation.** `BifRegistry::contains`/`lookup` build owned `(String, String)`
  key tuples per lookup, and the dispatch path does both (`contains` in `call_bif`, then `lookup` inside
  `BifRegistry::call`) — ~4 small `String` allocations per registered call, the largest attributable
  dispatch residual (measured ~60-80 ns/call alongside the caller-module clone). A borrowed-key
  `lookup(&str, &str, u32)` would remove it. This crate's `uses_*` gates delegate to
  `beam_bifs::hook_sets`' exported `needs_*`/`*_BIFS` tables, so a new consumer is picked up
  automatically (no per-crate drift); `RUNTIME_HOOKS_BIFS` has 195 entries (`hook_sets.rs`'s `assert_eq!`).
