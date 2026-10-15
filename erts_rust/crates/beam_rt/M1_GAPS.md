# `beam_rt` — Known M1 Gaps & Cross-Node Handoff Detail

Companion to [`CONTEXT.md`](CONTEXT.md).
It holds the current-state per-item detail behind `CONTEXT.md`'s condensed `Known Issues / Intentional M1 gaps`
bullets, its longest cross-node handoffs (a crate index plus the long form) and its GC-relocation hazards, split
out so `CONTEXT.md` stays under the Context Tree per-file read limit.
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
* **`NodeIdentity`'s creation is a fixed `0`** (`src/context/bridges.rs`) while a live
  `crate::dist::DistNode::creation()` exists, so an internal pid/port image still encodes creation `0`
  on a wired node. The **node atom** beside it *is* the runtime's live `Runtime::local_node()`, stamped
  on every `call_registered` arm.
* **Flag-driven `system_info` literals** (`src/info/system.rs`: `port_parallelism`, `break_ignored`,
  `modified_timing_level`, `halt_flush_timeout`; `src/info/limits.rs`: `dist_buf_busy_limit`,
  `delayed_node_table_gc`) report their default because no A.6 CLI arm preserves the driving flag
  (`+spp`/`+B`/`+T`/`-shutdown_time`/`+zdbbl`/`+zdntgc`): `cli.rs::record_unmodeled` drops an unmodeled
  `+flag`, so there is no state to derive from.
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
items (`current_function`/`initial_call`/`current_location`/`current_stacktrace`) are answered for a
scheduled process (a parked one probed from another process; the executing one for its **own** query
from compiled code, through the in-machine BIF door) and omitted for a process this runtime gave no
position (a fun-bodied process with no resolved local target, a never-scheduled pid, a direct/no-machine
caller); `catchlevel` follows the same rule. Two honest residuals of the door: `initial_call` is the
machine's **entry MFA**, where the oracle answers `{erlang,apply,2}` for some spawn shapes (a
fun-valued spawn, a start instruction), and the published slot is runtime-wide, so a **nested**
fun-applied run inside its window reads the **outer** machine's position for that pid. Any other valid
oracle item it cannot answer (`magic_ref`, `priority_messages`, ...) raises the typed `unsupported`.
`registered_name`'s
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
spent, keeping the parked `Machine` at `current_code` and letting the scheduler re-enqueue — so a hot
loop is pre-empted and a second runnable process interleaves (`tests/preempt.rs`). The budget is charged
**per interpreted instruction** through the engine's `ExecutionContext::reduce` hook (charged *before*
the instruction's body runs, so the resume applies its effect exactly once) **and**, on either tier, at
the machine's heap-admission points (`ensure_heap`/`ensure_heap_full`). `RuntimeContext::wants_reduce()`
is `false`, so only the runner's `BudgetContext` charges — the non-budgeted single-pass/nested paths
skip the hook call — and a **non-allocating** loop is therefore pre-empted on the interpreter tier just
like an allocating one. Residual gap: the **native baseline tier** runs its compiled blocks natively and
never crosses the per-instruction hook, so such a loop is charged only at a deopt or a heap-admission
boundary and a natively linked tail loop is not pre-empted mid-block. Other gaps: a `receive ... after T`
whose delay is `after infinity` or is not a statically positive integer (a Y-register slot) still parks;
a **capability gap is uncatchable** on the runner path.

## Execution tier (`src/runner/tier.rs`)
The runner's execution tier defaults to `JitMode::Auto`: every process's **first** entry runs on the
native baseline JIT. No per-module predicate guards it — the engine's baseline driver handles the
module-context switch (`Step::Enter` via `switch_into`), so no module has to be inspected first; the
one host-side obligation (the cross-module seam must be idempotent, because the native `call_ext*` arm
re-consults it through the interpreter-step fallback) is enforced in
`RuntimeContext::resolve_external_call`. `BEAM_RT_TIER` (`auto` default / `baseline` / `interpreter`)
overrides it, with `interpreter` the kill switch. The tier chosen at that first entry is **remembered
on the process** (`ProcessEngine::tier`), and every later slice — a pre-emption on the reduction
budget or a `receive` park — resumes on it through `Machine::resume_outcome_with_tier`, so a
native-tier process is not demoted to the interpreter mid-life (a `Tier::Interpreter` process is
unchanged). Measured figures and commands are in [`src/runner/tier.rs`](src/runner/tier.rs) and
[`TEST_STRATEGY.md`](TEST_STRATEGY.md).

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
library's own `void**` is kept by `beam_abi`'s loader, so the host answers the C's null); `consume_timeslice`
(`false`: the provider holds the process table immutably and the hook's `bool` cannot express
"unsupported"); `send` (`None` ⇒ `enif_send` answers `0` — the most damaging remaining gap, since it needs
a mutable process table plus a copy context); `whereis_port` (no atom can ever name a port here —
`erlang:register/2` refuses a port — so `None` is the *provable* answer);
`monitor_process`/`demonitor_process` (monitors are keyed by a **process** watcher and a NIF resource has
no process identity); `select` (a new poller/notification subsystem, not a wiring gap); `schedule_nif`
(`badarg`; no dirty schedulers and no scheduling trampoline); and `dynamic_resource_call` (needs a
per-library `{module, name}`-keyed resource-type registry).
A second `load_nif` for a loaded module is `reload`; a NIF returning a non-term is caught by
`try_call_nif` as a typed error, never an abort.

## Funs
`fun M:F/A` builds a real `FunKind::External` (`EXPORT_EXT`); a **compiled-module** `make_fun3` closure
gets its five identity fields from `FunIdentityBridge`, so `term_to_binary/1` is byte-exact
(module-scoped to the **caller**). An **`erl_eval` lambda** stays the honest `unsupported: local_fun`
(its oracle image closes over `erl_eval.erl` internals); no wrong encoding is emitted.

## Ports, drivers and the send family (A.4)
The read path builds one `{Port, {data, Data}}` per driver read, at the width the child wrote (`binary` →
up to `beam_io`'s `READ_CHUNK`, 64 KiB, per `read(2)`; `list` → a char list). The child's **stderr is
inherited** by default and is never port data; `stderr_to_stdout` makes fd 2 an exact duplicate of fd 1,
so its diagnostics arrive as port data in true write order. `connected` flips to `false` only at
`port_close/1`; `os:cmd/1` stays the runtime-native host spawn.
`{spawn_driver, Name}` resolves the name against the **built-in driver table** (`tcp_inet`/`fd`/`efile`)
before any `dlopen` exactly as the C does (`io.c:599-628`) — an unknown name is still `badarg`. A driver
port's `driver_output` **is** delivered to its owner as `{Port, {data, Data}}`, and a driver-emitted
`erl_drv_output_term`/`erl_drv_send_term` term reaches the connected process or the explicit destination
(the `TermQueue` drain is wired end to end). `driver_select` is real, and `driver_set_timer` arms for
every attached port. An `OUTPUT` port task always drains the port's output queue through the single write
path and *then* runs the entry's `ready_output` as the write-**readiness** notification. The driver's
`control` callback **is** routed behind `erlang:port_control/3`, and `ERL_DRV_USE`'s `stop_select`
**fires** on the cancel/deselect handback. The **driver-private queue**
(`driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq`), `driver_pdl_create`, the port
**state** flags (`get_port_flags`; `set_control_flags`) and the **process-monitor family**
(`driver_monitor_process`/`_demonitor_process`/`_get_monitored_process` over the per-port monitor tree,
and `monitor_process_result`'s three-way answer from the published `ProcessLiveness` view) are
implemented, so the host covers **27 of the 41** `DriverHost` hooks. The monitor residual is that the
view is a **per-round snapshot**, so a callback reached **outside** a round (a driver's `start` during
`open_port/2`, a `control` during `port_control/3`) consults the previous round's snapshot, and one
reached before any round answers `Ok`, never a fabricated `NotAlive`.
Typed gaps: `driver_async` (no async pool; `Runtime` is `!Send`, the `invoke`/`free` pair is `unsafe`,
`async_port_key` keeps the default `-1`); a `control` reply the driver answered from **its own** memory
(needs a safe copy helper on `beam_abi`); a NULL driver reply with `PORT_CONTROL_FLAG_BINARY` set
(answers `<<>>` where the C answers `[]`); an OS child (`os_pid` is `undefined` for a driver port,
`take_exit_status` is always `Ok(None)`); a built-in (`spawn`/`spawn_executable`/`fd`) port's
`port_control/3` answering `[]` where the oracle raises `badarg` (owned by `beam_io`'s default
`Driver::control`); and a driver arming **more than one** registration per direction (the typed
`unsupported`).
`line`/`{line, N}` framing is **adopted** (contract A.4): `open_port/2` decodes the option into
`PortOptions::line` (bare `line` ⇒ `512`; `{line, N}` ⇒ a positive small integer, else `badarg`;
mutually exclusive with `{packet, N}` and with `stderr_to_stdout` unless `use_stdio`),
`PortRegistry::register` arms `beam_io`'s `set_line_mode`, and the port-task round's
`take_line_notifications()` is delivered as the oracle's `{Port,{data,{eol|noeol,Data}}}` frames.
`StreamType::port_flags` reports `PORT_FLAG_BINARY`/`PORT_FLAG_LINE` independently (`[line]` ⇒ `2`,
`[binary]` ⇒ `1`, `[binary,line]` ⇒ `3`).
`erl_ddll` is **served** (A.4) from one loaded-driver view: `loaded_drivers/0`/`try_load/3`/`try_unload/2`/`info/2`
come from the **native floor** (`src/native/ddll.rs`) over the **one** name-keyed loaded-driver registry
`src/port/driver.rs` keeps — the same list `open_port({spawn_driver, Name})` resolves against — while
`monitor/2`/`demonitor/1`/`format_error/1` fall through to `beam_bifs`. `loaded_drivers/0` reports this
runtime's **own** (empty) set where the oracle lists its statically-linked
`tcp_inet`/`udp_inet`/`ram_file_drv` (a by-construction divergence), and with no `erl_ddll:load_driver/*`
a `Name` doubles as the search on `open_port` (a `/`-containing name is the object path, driver name = its
file stem; else `<name>.so`).
The send family serves `erlang:send/2` and `erlang:'!'/2` (the `SendBridge`); `erlang:send(Port, Data)`
needs the port write path and the distributed `{Name, Node}` send is contract A.5 — both raise
`unsupported: …`, never a fabricated value.

## Distribution and the node (A.5)
`-name`/`-sname` make the runtime a distributed node: `DistNode` binds a wildcard listener, publishes to
`epmd` best-effort (a failure is recorded, never fatal) and completes the server-side `'N'` handshake
against a real OTP peer (`tests/dist.rs`). Gaps: no `net_kernel` protocol; the distributed-spawn
family (`SpawnRequest`/`SpawnReply`), `Delivery::Other(_)` and the ack-owing `UnlinkId` half stay
typed `Unsupported`; `node/0` and the node atom a pid image carries **do** reflect the live
`-name`/`-sname` node (`Runtime::local_node()` is stamped on every `call_registered` arm), but the
`NodeIdentity` reports creation `0` (the placeholder above), so a decoded local id is still external
(see the `beam_codec`/`beam_bifs` handoff). `DOP_SEND`/`DOP_REG_SEND` already reach
local mailboxes through `LocalDeliverySink`, as do the route-free receipt families
(link/exit/unlink/monitor/demonitor/group-leader) — each emitting no outbound `DOP_*` — gated by
`Runtime::inbound_delivery` (default **ON**; `Runtime::allow_inbound_delivery` turns it off).

**The Erlang-level distribution subtree is not run** on a `-sname`/`-name` node: the native floor
answers `erl_distribution:start_link/0,1` with `ignore` (this runtime's distribution is the native
`DistNode` the CLI binds and publishes to `epmd`), so `kernel_sup` skips `net_sup`. Measured on a
`-sname` node: `whereis(net_sup)`/`whereis(net_kernel)`/`whereis(auth)` are `undefined` here vs pids on
the oracle, while `whereis(kernel_sup)`/`whereis(application_controller)`/`whereis(logger)` are pids on
both, `whereis(timer_server)` is `undefined` on both, and `node()`/`is_alive()` agree. Mechanism:
[`src/native/CONTEXT.md`](src/native/CONTEXT.md) *The Erlang-level distribution supervisor*.

## Timers
`send_after/3` answers a `#Ref<>`; `cancel_timer(Pending)` the remaining ms,
`cancel_timer(Fired|Cancelled|unknown-ref)` `false`, `cancel_timer(non-ref)`/a bad time or destination
`badarg`. `send_after/3`/`cancel_timer/1` are **native-floor** capabilities; `send_after/4`,
`cancel_timer/2`, `start_timer/3,4` (both the relative form and the `{abs, true}` absolute-time form,
whose delay is computed from the registered `erlang:monotonic_time/0` against the oracle's
`start_time`/`end_time` millisecond window),
`function_exported/3`, `link/1`/`unlink/1` and `os:set_signal/2` come from the `RuntimeHooksBridge`.
Unimplemented: `erlang:system_info/2`, `read_timer/1,2`, and the roster keys this bridge does not serve
(`delete_module/1`/`purge_module/2`, `port_get_data/1`/`port_set_data/2`, `term_to_iovec/1,2`,
`erlang:is_builtin/3`, `bump_reductions/1`, `loaded/0`, …); each is the typed `unsupported` gap.
`timer:apply_after/4` schedules through the booted `timer_server` (a `kernel_sup` child started on demand
through `timer:do_start/0`) with the `{abs, true}` timer arm, so `is_reference(timer:apply_after(50,erlang,self,[]))`
prints `false` on both runtimes with exit 0 (the call returns `{ok,Ref}`).

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
(the switch `erts_internal:scheduler_wall_time/1` records — armed or not — but no per-scheduler wall
time is sampled, so the tuple list is never fabricated). Gaps: `statistics/0` is not served, and on
`statistics/1` the keys naming per-scheduler
state this runtime genuinely lacks (`total_active_tasks`/`total_active_tasks_all`/`active_tasks`/
`active_tasks_all` — a scheduler's current-process flag — and `microstate_accounting` — no MSACC
accounting) are the typed `unsupported`; every other `RuntimeHooks` family is unserved too.

## GC / heap model
`DEFAULT_HEAP_WORDS` is `1 << 24` (16 Mi words of lazily-mapped address space). The term-construction
heaps (the BIF-heap pool + the `-eval` heap) are plain bump arenas sized `config.heap_words`; `+hmax N`
is a **real** cap on that capacity (`runtime.rs::term_heap_words`), and the `system_limit` translation is
applied **after** the copy engine's own retry (so `deliver_message` still matches the **raw**
`AllocKind::Message` to trigger the heap-fragment retry). This crate **does** drive `beam_gc`: a
*scheduled* process allocates on its own young heap and is collected at
`ExecutionContext::ensure_heap`/`ensure_heap_full` (sharing `collect_or_grow`; the latter is the full-root
door `test_heap` reaches, handing over the FR file too; armed by the slice's `BudgetContext`, roots =
`x[0..live)` plus the Erlang stack), pinned by `tests/gc_collect.rs` and reported by
`Runtime::gc_stats()`; every other context is a no-op on the unbounded arena. The limits are honest and
measured: a workload that allocates only through `ExecutionContext::alloc_words` (no `test_heap`
admission point, as real Cowboy `cowboy_bstr`-style code does) always falls back to the pointer-stable
arena and is never collected (`collections: 0`, `fallbacks` ≈ 620·N at N=10 000), `Machine`'s
float-register file is not mirrored by this crate's root publish, and a failed collection halts that
process's collection rather than failing the program.
**The term-heap capacity is still the hard limit** for anything not collected: both exhaustion classes
become `error::system_limit` via `error::enforce_term_heap_limit` — `AllocKind::Heap` (the per-process
bump allocator) **and** `AllocKind::Message` (the copy engine's destination-heap exhaustion, classified
by `beam_message::is_message_heap_exhaustion`) → CLI `beam_rt: system_limit`, exit 1.
**On-demand heap growth is adopted at the engine's heap-admission point and on the `beam_process`
message-delivery path.** `src/context/gc.rs::ensure_heap`/`ensure_heap_full` run the collector's
`Process::ensure_capacity` **growth** whenever the process's heap-size policy can reach the request
(`HeapPolicy::next_words(need, 0) >= need`) and a plain `Process::collect` otherwise, so a `test_heap`
request larger than the nursery is admitted by growing it in place (`tests/heap_growth.rs`); a bounded
`+hmax N` policy the request cannot reach is refused **before** collecting (the collection still runs,
the allocation falls back to the arena). `Process::deliver` grows the nursery the same way for a refused
message copy; the refusal is what reproduces the raw `AllocKind::Message` that `deliver_message` keys its
heap-fragment fallback on. A **relocation-exempt** receiver skips the retry entirely. The
**registered-BIF result** is the one allocation that really leaves its pooled heap (the copy-out
subsection below). Allocation **outside** the admission point still raises `error::system_limit`: BIFs
(and the floor) build *intermediate* terms on a `beam_alloc::Heap` (`BifContext::heap()`) caching raw
`hp`, so the live set is invisible to `roots`, and the bounded heaps
(`Runtime::bif_heaps`/`eval_heap`) never move.

### The BIF-result copy-out — a registered BIF's container result moves onto the process's scanned heap
A registered BIF builds its result on the runtime's pooled 16 MiB `Runtime::bif_heaps`
(`Runtime::take_bif_heap`) — `Target::Foreign`, so the process collector does not scan it — so a
container left there is a root the unscanned-root valve declines over while live.
`RuntimeContext::call_registered` (`src/context/calls.rs`) therefore **moves** the result onto the
running process's own young heap (`Process::heap_mut()`, which the collector scans) and rewinds the pool
to the offset read before the call, so the result lands in `x(0)` (the root set the engine already
publishes) and is relocated by a later collection. The copy is `beam_message::copy_struct` with an
**empty** source `OffHeapTable`; a full nursery is met by `Process::grow_nursery(hint)` (the
**no-relocation** seam — inside a BIF call the machine's x registers and stack are **not** published, so a
collecting `Process::ensure_capacity` would free live terms) and **one** retry, the hint being the
copier's own preflight count from the refusal (`AllocFailure::requested / size_of::<Eterm>()`, so growth
costs no second graph walk). One `OffHeapTable`/`CopyContext` pair serves both attempts, and the retry is
reached **only** for `beam_message::is_message_heap_exhaustion`; every other refusal a legal BIF result
can produce (the malformed-graph `badarg`, an `AllocKind::Heap`/`LiveObject` failure, a raised reason) is
permanent, so the move answers `None` without growing a space
(`src/context/calls.rs::tests::{a_permanent_copy_refusal_grows_no_nursery_and_writes_nothing, an_exhaustion_refusal_grows_the_nursery_and_the_retry_copies}`).
The empty source table is load-bearing: with no payload owner the copier **flattens** a refc binary's
window into a self-contained on-heap bitstring (byte-faithful, no refcount lost), and an off-heap **MSO**
reference with no owner is the copier's typed `bad_term`, so the move answers `None` — the pool is not
rewound and the result is left exactly as it was, never a dangling payload. The rewind is
`Heap::reset_to(mark)` at the `heap.used()` read *before* the call, so it frees only this call's own
words `[mark..used)` and keeps whatever a previous owner holds below `mark` (the pool is shared with the
native-floor reference minting, trace and the ETS-owner sweep, which return a heap *unreset*). It is
applied only when the move is **safe and complete** — no escaping `DispatchHooks` arm
(`dict`/`process_hook`/`nif_loader`/`send_hook`/`persistent_term`/`runtime_hooks`, each letting a term
built on `ctx.heap()` escape into state that outlives the call), no **reentrant** fun-applying call, an
**`Ok`** result (a raise may carry a `live_reason` term pointing into this heap, so the **error path
never rewinds**), a **scheduled process** (`mailbox_table` is `Some`; the `-eval`/nested shape has no
process to move onto), a **container** result (`result_is_foreign_container`; an immediate is unboxed and
a literal lives in the frozen literal area, so such a result rewinds with **no** copy), and a move that
succeeded.
**Residuals.** A **NIF** result is **not** copied out (a deliberate M1 residual), so it stays on the
pooled heap and keeps declining a collection while live; the `-eval`/no-scheduled-process shape, an
escaping arm and an error result also leave their container on the pooled (`Target::Foreign`) heap; a
**nested** cross-module run (`RuntimeContext::run_export`) never reaches a collection point. Pinned by
`src/context/tests.rs` (`a_bif_container_result_is_moved_onto_the_process_heap_and_the_pool_is_rewound`,
`an_immediate_result_rewinds_the_pool_without_a_copy`,
`an_escaping_arm_neither_moves_the_result_nor_rewinds_the_pool`,
`a_bif_result_on_the_process_heap_lets_the_churn_loop_be_collected`,
`a_partially_used_pool_is_rewound_to_the_callers_mark`,
`an_escaping_arm_does_not_rewind_a_partially_used_pool`).

### `ets_copy` peak RSS — the copy-out's retained young space, not the ETS adopt path
`copy_bound/ets_copy`'s peak RSS is **linear in N** (94.68 MiB at N = 100 000, ≈99 words per iteration)
and is a structural limit of the BIF-result copy-out above, **not** the ETS adopt path: `safe_to_rewind`
is already `true` for `ets:*` and the pooled heap is already rewound. It tracks BIF calls, not floats and
not ETS table state (a float-free ETS loop of the same shape grows identically; a float-carrying churn
loop with no ETS is flat). The loop's heap traffic is the copy-out, which by design cannot collect (the
machine's x registers / Erlang stack are not published at a BIF call), so a full nursery is met by
`Process::grow_nursery(hint)` (`src/context/calls.rs::move_result_into_process_heap`) and the **retired
young space is retained**, so capacity accumulates linearly; the loop never reaches a real collection
either — `ensure_heap_full` returns at `!process.should_collect(heap_need)` (`src/context/gc.rs`) because
its `test_heap` prefixes are tiny (a 233-word default nursery, `beam_gc/src/policy.rs`) and
`alloc_words_rooted` is never entered (the BIF, not the engine, builds the result). The caller-module
float guard is **not** the mover (relaxing it at the two FR-publishing GC doors changes nothing here and
contradicts the pinned float-collection contract, `tests/gc_collect.rs`). Closing it needs either a
**`beam_process`** seam that can shrink/reuse a retired young space or a **`beam_engine`/runner** seam
that publishes the driving machine's roots at the BIF call so the copy-out can collect — both sibling-crate
changes; the `beam_message` clean-destination ceiling below is the same lever. Figures and bisection:
[`PERF.md`](PERF.md) *COPY-OUT-CUT*.
## Heap switches
`+hms`/`+hmbs` set a `beam_gc::HeapPolicy` rounded through the growth table (`+hms 1000` → 1598),
`+hmax*` the `max_heap_size` map; a malformed value is exit 2 where the oracle exits 1.

## Cross-node handoff detail
* **The differential gate.** Every recorded non-agreeing case is an honest capability gap in
  `tools/differential/known-gaps.txt`, never a `mismatch`. The one structurally unclosable gap is
  `wire/new_fun_ext_encode` (an `erl_eval` lambda's `NEW_FUN_EXT` identity — `erl_eval` internals).
  `boot_extra/init_script_id_names_the_running_otp_release` is a **live divergence**: the route now
  answers (no longer `unsupported`), but `init:script_id()` returns the resolved installation script's
  header (`{"Erlang/OTP","29"}` on this host) while `erlang:system_info(otp_release)` returns this
  runtime's target release (`"30"`), so its `=:=` assertion is `false` here and `true` on the oracle —
  the oracle's own loaded script and its release agree, this runtime's do not. Both A.9 `selfhost/*`
  cases agree (the A.9 E0 bullet below).
  `erlang:statistics(runtime)` is served, so its case agrees. The live rollup, the allowlist state and
  the `gate.sh` exit code belong to [`../tools/differential/CONTEXT.md`](../tools/differential/CONTEXT.md).
* **A.7/A.9 boot — the boot script executes all 22 directives to completion;
  the tail's blockers are served in-node.** `Runtime::boot_from_script` runs the resolved script to
  completion, ending at the final `{progress,started}` (0-based directive index 21 of 22); the in-crate
  `BootStop` is `None` (`execute` returns `Ok`). The CLI
  `-boot bootstrap/bin/start` and the `run` action exit **0** with **empty stdout and empty stderr**
  (0 bytes each). Every CLI action but `-version` drives the walk
  (`src/cli.rs::{boot_script_to_drive, boot_then}`: the `-boot FILE` value when given, else the literal
  default `start` → `<Root>/bin/start.boot`), so a plain `-eval`/`-s`/`-run`/`run`/bare-`-noshell` route
  runs against a really booted node — measured there: `whereis(kernel_sup)`, `whereis(application_controller)`
  and `whereis(logger)` are live PIDs, `whereis(timer_server)` is `undefined`. Directive 13 leaves a live
  `logger_server` registered as `logger`, and the
  differential case `boot/proc_lib_start_link_blocks_the_boot_path` **agrees**.
  **The tail's blockers are served in-node — the native floor (`RuntimeContext::try_native`) or
  runtime state — with everything else falling through to the registry unchanged:**
  `process_flag` for the bare `monitor_nodes` atom and the `{monitor_nodes, Opts}` tuple flag
  (`src/native/process.rs::process_flag_native`; the C's `bif.c:2202-2227` and `erts_monitor_nodes`
  at `dist.c:6754-6868`), which is what `net_kernel:monitor_nodes/2` (`net_kernel.erl:673`) and
  `global:init/1` (`global.erl:802`) require; `erlang:nodes/2` (`src/native/net.rs`), which
  `global:init/1`'s `Known = lists:foldl(…, #{}, nodes(visible, #{connection_id => true}))`
  (`global.erl:810`) calls; `prim_tty:tty_select/2` over the port poller (`src/native/prim_tty.rs`);
  `process_flag(message_queue_data, on_heap|off_heap)` (parsed and accepted); and
  `erlang:system_flag(system_logger, _)` (`src/seq_trace.rs`).
  `erts_internal:scheduler_wall_time/1` is served by the `RuntimeHooks` bridge
  (`src/context/runtime_hooks.rs::scheduler_wall_time`), so `kernel_refc`'s
  `resource(scheduler_wall_time, Enable)` (`lib/kernel/src/kernel_refc.erl:141-142`) completes without a
  `start_error`; and the `erlang:hibernate/0` OTP-30 instruction (`src/native/process.rs`) answers
  the atom `true` (the C's `erts_hibernate` shrinks the heap and clears the ACTIVE flag; the caller's
  own following `receive` does the blocking), so `gen_server:loop_hibernate/4`
  (`lib/stdlib/src/gen_server.erl:2338`) does not crash `kernel_safe_sup`/`logger_sup`.
  **An external fun resolves in-machine:**
  `RuntimeContext::resolve_local_fun` (`src/context/execution.rs`) resolves an EXTERNAL fun
  (`fun M:F/A`, `EXPORT_EXT`) to the callee module's export label through the
  `external_fun_target`/`external_fun_name` helpers (`src/context/apply.rs`), so the engine enters e.g.
  `gen_server`'s callback fun IN-MACHINE instead of falling back to the value-returning `apply_fun`
  (which nested a fresh `Machine` under `may_pump_scheduler() == false` → `src/context/pump.rs` refuses
  → the uncatchable `blocked()`). Pinned by
  `src/context/tests.rs::{an_external_fun_whose_callee_blocks_resumes_on_a_scheduled_slice, resolve_local_fun_declines_the_fun_kinds_it_cannot_claim}`.
  **An uncaught raise out of the boot chain renders as the `Display` placeholder.** `src/cli.rs`'s
  `Action::Boot` arm is the only one that routes the boot-chain result through `reported_reason`, so on
  the `-eval`/`-s`/`-run`/`run` routes a structured raise reason leaving the boot chain renders as
  `beam_rt: exit <structured reason>` instead of the real term. The `-eval` action's own raise is
  unwrapped too; only the `run` route's own `run_scheduled` raise is rendered (`run_file`), and
  `tests/cli.rs` covers only that action's own raise.
* **`beam_bifs` — the registered `io:format/2` `~p`/`~w` printer raises `badarg` on any term carrying a
  fun.** The registered `io:format/2` implementation is `erts_rust/crates/beam_bifs/src/io.rs`, driven by
  `src/format.rs`, and a fun is not renderable there. Raw evidence, the same command on both runtimes:
  `beam_rt -noshell -eval 'io:format("~p~n",[fun lists:reverse/1]),halt(0).'` → `beam_rt: badarg`, exit 1,
  vs the oracle's `fun lists:reverse/1`, exit 0; and
  `beam_rt -noshell -eval 'io:format("~p~n",[[{k,fun lists:reverse/1}]]),halt(0).'` → `beam_rt: badarg`,
  exit 1, vs the oracle's `[{k,fun lists:reverse/1}]`, exit 0. The fun itself is sound — `erlang:make_fun/3`
  and `fun M:F/A` both work, only printing fails, and the failure is catchable
  (`catch io:format(...)` → `{'EXIT',badarg}`). Owner: `erts_rust/crates/beam_bifs`.
* **`beam_engine` — a stacktrace's top frame is duplicated (the same `{M,F,A}` at two adjacent lines).**
  `Machine::throw_with_trace` (`erts_rust/crates/beam_engine/src/machine/catch.rs:318-324`) pushes the
  raising frame from `Machine::current_code()` and then again from `Machine::resumption_contexts()`
  (`machine/frames.rs:316-325`), which re-yields that same function with its stale call-site CP; the same
  duplication is in `Machine::raise_stacktrace` (`machine/catch.rs:480-503`). Raw evidence:
  `try _ = inner(), erlang:error(badarg) catch error:_R:S2 -> S2 end` → oracle
  `[{probe,dup,0,[{file,…},{line,15}]}, …]`, engine
  `[{probe,dup,0,[{file,…},{line,15}]}, {probe,dup,0,[{file,…},{line,14}]}]`; the same duplication also
  appeared in `proc_lib`/`gen_server` stacktraces from a boot-time crash report —
  `[{proc_lib,exit_p,3,[{line,355}]},{proc_lib,exit_p,3,[{line,355}]}]` and
  `[{gen_server,init_it,6,[{line,2272}]},{gen_server,init_it,6,[{line,2271}]}]` — which mis-attributed
  the `error:badarg` to the re-raise site. Owner: `erts_rust/crates/beam_engine`.
* **`beam_engine` — `build_stacktrace` drops the run's entry-module frames when the handler lives in a
  non-zero module context.** `erts_rust/crates/beam_engine/src/families/exceptions/stacktrace.rs:163` plus
  the `context == 0` heuristic at lines 169-181 (`FrameOwner::Skip` for every context-0 frame when
  `handler_context != 0`); the same heuristic is in `Machine::current_stacktrace`
  (`machine/location.rs:238-241,261-268`). Raw evidence: `ent:g/0 -> mid3:run/0 -> callee:h/1 ->
  erlang:error(badarg)` with the `try … catch error:R:S -> S end` in the callee module → oracle
  `[{callee,h,…},{mid3,run,0,…},{ent,g,0,…},…]`, engine `[{callee,h,…},{mid3,run,0,…}]` (the
  `{ent,g,0,…}` entry-module frame is missing). Owner: `erts_rust/crates/beam_engine`.
* **`beam_engine` — the reported `make_small(arity)` → `NIL` degradation does not reproduce.**
  `write_item` always emits an integer arity
  (`erts_rust/crates/beam_engine/src/families/exceptions/stacktrace.rs:377-388`) and
  `beam_term::make_small` (`erts_rust/crates/beam_term/src/immediate.rs:76`) is total, so
  `erlang:make_fun/3` works; the `[]` an oracle prints for a 0-arity caller is the **oracle's own** `A`
  slot (it prints the argument LIST) where this engine prints the integer arity — the separate,
  already-documented `function_clause` `A`-slot divergence (Int vs ArgList).
  Owner: `erts_rust/crates/beam_engine`.
* **`beam_engine`/`beam_rt` — the in-machine execution-position door serves the executing process's own
  position.** A scheduler pass takes the executing process's engine **out** of `RunnerState` for the whole
  slice (`erts_rust/crates/beam_rt/src/runner/run.rs`), so the runner-state view cannot see the caller's
  own machine. `beam_engine`'s defaulted `ExecutionContext::wants_bif_in_machine`/`call_bif_in_machine`
  seam hands the host the **live `&Machine`** and the run's entry module on every BIF dispatch, and
  `src/context/position.rs` is the opt-in, so `erlang:process_info(self(), current_stacktrace)` (and the
  other execution-position items) is answered from the machine's own frames. A non-self query, a nested
  query while an outer door call is in flight, or a process with no such position still falls through to
  `RunnerState` (a typed gap where no machine is reachable).
  Owner: `erts_rust/crates/beam_rt` (`src/context/position.rs` + `src/native/memory/`); the seam lives in
  `erts_rust/crates/beam_engine`.
* **A.9 E0 — in-process compilation works, is byte-identical, and the *scheduled* route runs on both
  tiers.** `compile:file(File, [no_spawn_compiler_process, deterministic, {outdir, Dir}])` runs
  **in-process** and writes a `.beam` **byte-identical** to the oracle's identical-option run (`cmp`
  clean; the `Code` chunk matches `erlc +deterministic`) — `tests/compile_file.rs`. Both corpus cases
  (`selfhost/in_process_compile_file_no_spawn_deterministic`, that E0 shape, and
  `selfhost/scheduled_process_compile_file_deterministic`, the default `compile:file/2` shape through
  its own compiler process) drive a scheduled `run` and **agree** with the oracle on both the
  interpreter and the baseline tier. The fun-image half of `callee_observes_its_call_site` is
  **per-function** (`erlang:term_to_binary/1,2` / `erlang:external_size/1,2` are guarded only in the
  function that issues the call; the module-wide `erlang:load_nif/2` is unchanged), so a `compile:*`
  function whose own body issues no fun-image BIF is admitted in-machine and its blocking `receive`
  suspends through the ordinary `RunOutcome::Wait` path. The hazard class both cases run through — the
  monitor-DOWN branch of `beam_process`'s termination cascade relocating a **woken** monitor watcher's
  heap — is declined in this crate by the host collection gate plus the park exemption (the A.9 class in
  *GC-relocation hazards* below); mechanism:
  [`EPP_SCHEDULED_ROUTE_FINDINGS.md`](EPP_SCHEDULED_ROUTE_FINDINGS.md).
* **`file_server_2` service** (`src/native/file_server.rs`) answers the
  `{'$gen_call',{Pid,Tag},Request}` / `{file_request|io_request,From,ReplyAs,Request}` wire and the
  server/io-device request surface `lib/kernel/src/file.erl` issues (`{open,…}`, `read_file`, `list_dir`,
  `delete`, `rename`, `make_dir`, `del_dir`, `read_link`/`make_link`/`make_symlink`, `altname`, `get_cwd`,
  `close`/`sync`/`datasync`/`truncate`/`{position,At}`/`{get_chars,…}`/`{put_chars,…}`); every other
  command is the typed `unsupported`, and a `From` tag that is a process **alias** is the typed
  `unsupported` too. The `{get_until,…}`/`{get_line,…}` io requests **are** served
  (`src/native/file_server/{get_until,get_line}.rs`), so `io:scan_erl_exprs/4`, `io:parse_erl_exprs/4`
  and `file:read_line/1` complete and agree with the oracle (`tests/io_scan.rs`). Wire tables + per-request
  gaps: [`src/native/CONTEXT.md`](src/native/CONTEXT.md).
* **`beam_process` / `beam_message` split borrow.** The split accessor exists
  (`Process::heap_and_off_heap_mut()` / `heap_and_mailbox_mut()`, `beam_process/src/process.rs:579`), so
  the `beam_message::Fragments::move_into` call at receive borrow-checks. The blockers for adopting it
  here are an **owning drain for the wake list** and a `!Send`-friendly `SchedulerPool` for SMP adoption
  (`Runtime` holds `Rc`/`RefCell` tables and `*mut Eterm` heaps, so `beam_process`'s `unsafe impl Send`
  escape is unavailable). Without that drain this crate composes the public fragment primitives directly,
  and `MailboxState::remove_at` retains a removed message's heap `Fragments` and its whole off-heap
  family (the refc-binary and MSO handles — the MSO handles in the envelope's own order, so their
  `[HEADER_BIN_REF, slot, 0]` container terms resolve against the copy's rebuilt source table — and the
  external-thing node identities) so a just-bound payload stays valid. The `*_with_ports` scheduler forms
  exist, but this crate does not route its slices through them (holding the port-registry borrow across
  interpreted code turns every port BIF into the typed `unsupported`), so the pass discharges those
  obligations at its round boundaries through the exit observer (`src/runtime/ets_owner.rs`).
* **`beam_engine`** — a `MinimalContext` **arena word/byte accessor** so `erlang:memory/0,1`'s
  `processes`/`processes_used` can be real on the `-eval`/single-pass path.
* **`beam_bifs` / `src/context` — the node identity.** The live node atom is fed from
  `Runtime::local_node()` and stamped on every `call_registered` arm, so `erlang:node/0` and the node
  atom a pid image carries reflect a live `-sname`/`-name` node; the residual is the **creation** (the
  fixed `0`, see *Placeholders*).
* **Distribution, phase 2** — the inbound link/exit/monitor/alias `DOP_*` families need a
  `beam_process` **remote-peer** seam (plus an alias table); `DOP_SEND`/`DOP_REG_SEND` already reach
  local mailboxes, and a live-peer loop past the handshake needs the `FragmentAssembler` receive-buffer
  coupling.
* **`beam_io` / `beam_abi` lifecycle.** The **process-monitor family is live** (the safe
  `beam_abi::monitor_identity(mon: *const ErlDrvMonitor) -> Option<[u8; 24]>` reader backs
  `driver_monitor_process`/`_demonitor_process`/`_get_monitored_process` over the per-port monitor tree,
  and `monitor_process_result` answers from the published `ProcessLiveness` view; residual: the per-round
  snapshot freshness above). The typed asks are `driver_async` (a `beam_abi::DriverHost` seam that can
  fire `ready_async`, since `beam_rt` is `#![deny(unsafe_code)]`), a safe copy helper for a `control`
  reply the driver answered from its own memory, a per-registration `ErlDrvEvent` identity, and
  `erl_drv_monotonic_time`'s phase (`beam_abi/src/driver/drv/mem.rs` reads `CLOCK_MONOTONIC` from the
  system epoch where the oracle returns the Erlang monotonic time).
* **`beam_io` — the filesystem accessors are implemented.** `beam_io::path_access(Path)` and
  `process_credentials()` exist in `beam_io/src/fs_access.rs` and the native floor consumes them, so the
  `#file_info{}` **`access`** slot is served and `file:read_file_info/1`/`file:read_link_info/1` on an
  existing path answer `read_write`, matching the oracle.
* **`beam_bifs` — the `ets:setopts/2` heir gift copy and the tid-ref mint.** See *ETS ownership* below.
* **`beam_io` — the port table sits behind one `RefCell`.** `Runtime::with_port_table` is the short-borrow
  seam and the quiescence predicate reads `PortRegistry::has_nothing_to_poll`, but `beam_io::PortRegistry`
  keeps its `ids` port-number table private behind the same single `RefCell` as the live ports, so the
  runtime cannot hand the process table and the port table out as disjoint borrows and therefore cannot
  route its slices through `Scheduler::run_ready_with_ports`; a split accessor over the id table unblocks it.
* **`beam_message` — the copy-out's destination is never clean (the one remaining attributable copy-out
  cost, and the `ets_copy` RSS lever).** `Process::heap_mut()` always has words in use, so
  `beam_message::copy::is_clean_destination` (`erts_rust/crates/beam_message/src/copy.rs`: `to.used() == 0
  && ctx.dst_off_heap().is_fully_empty()`) rejects it and the copier's allocation-free optimistic
  `try_copy_fast` is **bypassed on every copy-out**, so each copy pays a full `preflight_words` walk before
  a single destination word is written. Relaxing the guard for this call site would remove one of the two
  remaining walks **and** the `ets_copy` retained-nursery growth (the `GC / heap model` subsection above);
  it is safe here because the destination is a `Process` young heap the caller owns, and the guard's own
  off-heap pushes are already rolled back through
  `beam_message::off_heap::OffHeapTable::{is_fully_empty, rollback}` (both crate-private). A sibling-crate
  change, reported not worked around.
* **`beam_bifs` — the hook-roster drift guard.** This crate's `uses_*` gates delegate to
  `beam_bifs::hook_sets`' exported `needs_*`/`*_BIFS` tables, so a new consumer is picked up
  automatically (no per-crate drift); `RUNTIME_HOOKS_BIFS` is pinned at **172** entries by
  `hook_sets.rs`'s `assert_eq!`.

## OTP-lifecycle survey — what a scheduled process can and cannot do today
Method: self-contained `.erl` fixtures compiled by the system `erlc` (OTP 29.1.1 / erts-17.1, the oracle)
into `$TMPDIR`, each exporting arity-0 probes, run three ways:
`erl -noshell -pa <fix> -eval 'io:format("~w~n",[M:F()])' -s init stop` (oracle),
`erts_rust/target/debug/beam_rt -noshell -pa <fix> run <fix>/M.beam M:F/0` (**scheduled** process),
and `… beam_rt -noshell -pa <fix> -eval '…'` (runtime root process, which **pumps** the scheduler).
Exit 0 = success, 1 = runtime error, 3 = capability gap.

### Works today (oracle-identical)
`code:which/1`, `code:get_object_code/1` (the oracle's path-based `{Module,Binary,Filename}`,
cross-checked against `erl`'s own `mod_to_bin/3` read), `code:load_file/1` (then `module_loaded/1`
true), `application:get_key/1,2`, `application:loaded_applications/0`,
`application:which_applications/0`, `error_logger:warning_map/0` (a `RuntimeHooks` bridge). **An
OTP-behaviour child comes up and survives**: `proc_lib:start_link/3`, `gen_server:start_link/2,3`
(unnamed and `{local,Name}`), `gen_server:start/2,3` and `supervisor:start_link/2` all return a live
`{ok, Pid}` (`is_process_alive/1` true). A `spawn`/`spawn_link` child whose **entry function** itself
blocks in a `receive` parks and is resumed (`spawn(M,F,[])` with the blocking receive at the top level
of `M:F`, incl. via a cross-module call, `erlang:apply(M,F,[])`, or a local fun), and so does a
cross-module call whose callee parks on a `receive` that a later **message** matches. Local fun
application with free variables works while the closure's body can complete without a suspension; a
closure that is the process *entry* is driven in-machine by the runner. `gen_server:cast/2` returns `ok`
(a plain `!`). Plain `!`/`receive`, `erlang:monitor(process,P)` (arity 2) and
`erlang:monitor(process,P,Options)` with a `{tag,T}` and/or `{alias,Mode}` option work.
**`gen_server:call/2,3` and `gen:call/3,4` work for a finite timeout as well as `infinity`** — for a pid
*and* a registered `{local,Name}` — because `gen:do_call/4`'s finite-timeout clause
(`lib/stdlib/src/gen.erl:251`, `erlang:monitor(process, Process, [{alias,demonitor}])`) and the
`erlang:send/3 [noconnect]` it uses are both served; `sys:get_state/1` and the `sys:*` requests work
too. A `receive … after T` that must time out delivers its timeout on **every** route, including through
a nested run.
The whole-file `file` family completes from a **scheduled** process (`read_file/1`, `write_file/2`,
`open/2` read/write, `close/1`, `list_dir/1`, `get_cwd/0`, `consult/1`, `read_file_info/1`/
`read_link_info/1` — an existing path answers `read_write` — and `filelib:is_file/1`), and the incremental
io-device read path agrees with the oracle on both routes (pure `read/2`, pure `read_line/1`, and the
mixed order) because the device is buffer-first. The one documented divergence left is `{get_chars,…}` on
a `{encoding,unicode}` device, which counts **bytes** where the oracle counts **characters**
([`src/native/file_server/CONTEXT.md`](src/native/file_server/CONTEXT.md)).

### Confirmed failing — root cause and repro
1. **`logger`/`error_logger` reports are emitted but reach no native logger sink.** A booted runtime has
   a live `logger` process and the `logger` ETS config table exists (`ets:whereis(logger)` is a `#Ref{}`),
   so `logger:info/1,2`, `warning/1`, `error/1` and `error_logger:info_msg/1,2`, `error_msg/2` all answer
   `ok`. The residual is that the `lg:*` test helpers still depend on a native logger ETS floor this
   runtime does not provide.
2. **`application:start/1,2`, `application:stop/1`, `application:ensure_all_started/1` reach a live
   controller but only the boot script's own applications are startable.** `application_controller` is a
   live PID on every CLI route now, so a request is answered rather than failing `noproc`; a start for an
   application the resolved boot script did not load still fails (measured: `ensure_all_started(crypto)`
   → `{error,{crypto,{"no such file or directory","crypto.app"}}}` on this host). The native floor answers
   only the read-only queries (items in "Works today"). Repro: `ap:start_sasl/0`, `ap:start_syntax/0`,
   `ap:ensure_syntax/0`.

### The `-eval` vs scheduled asymmetry
The `-eval` root process **pumps** the scheduler and can suspend a blocking call; a **scheduled**
process (`run`) parks at its own top-level `receive`. Both routes deliver a `receive … after T` timeout,
including one reached through a **nested** run (a cross-module call or a fun application), and a
`receive` waiting for a message resumes correctly on both — the two A.9 `selfhost/*` corpus cases are
scheduled `run`s that reach a nested `compile`/`epp` call chain and **agree** with the oracle on both
tiers. A scheduled slice is **running**, not merely runnable (`beam_process::Scheduler::slice_begin`
flips the pid to `ProcessState::Running`), and a signal- or delivery-driven collection of that process
is declined — by the host collection gate the `ptab.rs` signal sites consult for a foreign-reachable
receiver, and by the park arm's relocation exemption for the parked (plus woken-not-yet-resumed) window
— so no such collection moves its heap against an incomplete root snapshot (the A.9 bullets above and
*GC-relocation hazards* below). The incremental `file` read path agrees with the oracle on both routes — pure read, pure line
and mixed order alike (the device is buffer-first).

**A `spawn`ed `-eval` closure is run synchronously to completion**, so it is not a nested run that
suspends: the evaluator (`src/eval/mod.rs`, `apply_eval_closure`) drives the closure to its end before
the spawning expression continues, and the evaluator's `receive … after T` arm polls the port poller
once and then falls straight through to the `after` body (M1 has no host timer to block on). A `-eval`
child that "parks" therefore returns and terminates before the parent's next statement — which is why
the async-completion shape for a **live, blocked** target is pinned only on the scheduled route
(`tests/garbage_collect.rs`). Owner: `src/eval/mod.rs`'s `receive` — the `after T` deadline must
actually be waited out (or a spawned `-eval` child otherwise kept alive).

## GC-relocation hazards (what a MOVING engine heap would invalidate)
The engine's `alloc_words` seam prefers the running process's own **young heap** (a capacity refusal
is met by a no-relocation `Process::grow_nursery` and one retry) and falls back to the
pointer-stable `Runtime::arena`; the roster below splits *at risk* from *safe*, since a raw `Eterm`
pinned **outside** the GC root set is what a future **moving** heap would dangle.
**`Process::regs` is a snapshot, not a live mirror of the running machine.**
`publish_roots_full`/`scatter_roots_full` (`context/gc.rs`) copy every x register, the whole stack
and the machine's float-register file into `process.regs` (the FR words appended after the stack
words, bounded by a `fr_count` — `beam_process::Registers` has no FR container) **only** at a
heap-admission point (`gc::ensure_heap_full`, the whole file) or a park (`publish_parked_roots`, the
live prefix `Machine::fr_mark()`, recorded on `ProcessEngine::fr_published` for the resume, plus the
parked machine's live `pending_exception` as an `extra_roots` word); between those points the
snapshot is stale by up to one slice, so a collection rooted in it reaches the machine's x/stack/FR
and nothing else — a Rust local or an un-published `Machine` field is not in the root set at all.
The engine's `live` count is ignored (over-approximating a copying collector's root set is always
safe).
Three collect triggers differ in exposure. The **admission-point** collection *is* the
publish/collect/scatter hand-off (safe). The **delivery-driven** (`Process::deliver` →
`ensure_capacity` → `run_rooted_collection` over `self.regs`) and the **signal-driven** one
(`ptab.rs`'s `terminate_with_ports` → … → `grow_receiver_for_signal` → `ensure_capacity`) fire
outside an admission and are refused for a receiver with unpublished roots —
`Process::has_unpublished_roots()` (= `relocation_exempt || state == Running`, `process.rs:1098`) is
tested *first* by every caller, so a refused delivery becomes a heap-fragment delivery and a refused
ptab signal goes to `enqueue_fragment_signal`. `beam_rt` adds a **second** refusal on that seam
(`RuntimeContext::deliver_message_with_off_heap_inner` asks `delivery_collection_is_unsound`, the
valve below, and on `true` marks the **receiver** relocation-exempt) — the **fifth** collection
site; every send routes through `deliver_message_with_off_heap` (`src/context/mod.rs:590`), marking
the **sender's own pid** exempt. A signal to the running process is deferred; one to a **parked**
process is declined by the park-window exemption below; one to a **woken-but-not-yet-resumed**
(`Runnable`) receiver — the A.9 `selfhost/*` class — is declined by the **host collection gate**
every `ptab.rs` signal site asks (OR-ed with `has_unpublished_roots`) before a *collecting*
`ensure_capacity`, installed as `RuntimeCollectionGate` (`context/gc_walk.rs`, the same
roots-incompleteness walk as the admission-point valve) in `Runtime::new`. The **`Yield`** arm is
the third out-of-admission trigger and is *not* exempt: it publishes the machine's roots and then
serves `Runtime::serve_gc_request`; the **Wait** arm's own `serve_gc_request` is declined by the
exemption it has just set. Mechanism:
[`EPP_SCHEDULED_ROUTE_FINDINGS.md`](EPP_SCHEDULED_ROUTE_FINDINGS.md).
The residual exposure is a live term held **outside** `process.regs` — a Rust local, or an engine
`Machine` field with no extra-root publication — which is why `relocation_exempt` exists (a
`self()`, the wrapped `-eval`/nested-`Machine` scope, the parked window). A parked machine's
`pending_exception` is published as an `extra_roots` word at the park
(`runner/run.rs::publish_parked_roots`) and read back on resume
(`RuntimeContext::resync_parked_roots(pid, machine, fr_count)`); the parked window carries the
exemption, so that publication is never a licence for a park-window collection. Still outside the
root set: `Raise::live_reason`'s reason term between a raise and its stabilization
(`try_stabilize_escaped`, `runner/run.rs:662`). The post-slice pin scan (`runner/run.rs`) reads
`Machine::fr_mark()` **once** per slice, pins the pid iff it is `> 0`, and passes the same count as
`publish_parked_roots`' `fr_count`, so a park publishes exactly the live FR prefix and never loses a
live FR term.
**A collection is DECLINED whenever a root — or a container the process's scanned graph reaches —
lives in a region the collector does not scan (the unscanned-root valve).**
`RuntimeContext::call_registered` (`src/context/mod.rs`) hands `beam_bifs` the runtime's **pooled**
16 MiB `Runtime::bif_heaps`, and `context::gc::alloc_words`'s fallback hands blocks out of
`Runtime::arena`; both are `Target::Foreign` to the collector (`beam_gc/src/copy.rs`: a pointer
outside the `FromSet` is left as-is and **not scanned**). The valve
(`context/gc.rs::root_is_unscanned` + `src/context/gc_walk.rs`) calls a term safe iff it is an
**immediate**, a **literal**, or a pointer the process's collector **scans** (any nursery space,
incl. one a `grow_nursery` retired, or the mature space); each of the three admission points
(`ensure_heap`, `ensure_heap_full`, `alloc_words_rooted`), the runner's park arm
(`Runtime::serve_gc_request`) and the delivery seam (`delivery_collection_is_unsound`) **declines**
(`Ok(())`) a collection whose root set reaches anything else, while the unrooted
`context::gc::alloc_words` keeps its arena fallback. The valve spans the machine roots the caller
publishes (`x`/stack/FR) **and** the process's own root-set containers (`Process::collect`'s
registers, mailbox, dict, saved arguments and pending signals), and it *descends* through the
scanned graph, so a foreign container reached at depth ≥ 2 is found. The descent is gated on a
sticky, add-only per-pid latch (`Runtime::foreign_reachable`/`note_foreign_reachable` over
`GcState::foreign`), opened by every event that can put an unscanned container into the graph — the
engine's arena fallback (`note_gc_fallback`), a BIF / native-floor / NIF result built outside the
process heap (`open_gate_for_foreign_result`), a heap-fragment delivery (`send_fragmented_message`),
a spawn argument or entry fun not on the new process's own heap (`note_spawn_foreign_roots`), the
escape store / `stabilize_term` (`note_escaped_store_roots`), and the valve itself on a hit — and
pruned at the pass boundary for a process that is gone (`Runtime::prune_gc_requests`). **The holder
it was built for is `Runtime::bif_heaps`:** a container a registered BIF built there
(`erlang:'++'/2`, `lists:append/2`, `lists:reverse/1`, `lists:filter/2`, `ctx.tuple(…)`) writes the
caller's heap `Eterm`s verbatim, and a **scheduled process's** container result leaves the pool
(*The BIF-result copy-out* above), so the holder is reached through the **unrewound** paths (a **NIF
result**, an **escaping** `DispatchHooks` arm's result, the `-eval`/nested shape, an error result)
at depth ≥ 2 below a scanned root. Verified end to end: `beam_rt run w.beam w:f9/0` renders the
oracle's exact `~w` at exit 0, and `tests/gc_run.rs` passes; the descent is pinned by
`src/context/tests.rs::{the_valve_declines_a_foreign_container_nested_in_a_scanned_container,
…_a_deeply_nested_foreign_container, …_collects_when_the_nested_graph_is_fully_scanned}` (the first
two fail with the descent forced off).
* **At risk (a raw `Eterm` outside the root set):** `Runtime::persistent_terms`
(`context/persistent_term.rs`), `Runtime::trace`'s `MatchSpec::Functions`, `Runtime::seq_trace`'s
`Token{label,from}`, `Runtime::eval_closures` `FunBody.env`, `Runtime::timers`
`TimerAction::Deliver`; `MailboxState.markers[].reference` and `NameSpec.inline_options` still need
a check. A spawn argument or entry fun (`ProcessEngine.args`/`entry:Fun{fun}`, `PendingSpawn.args`)
and a stored escape-store term also sit outside those roots, but each opens the descent latch
(`note_spawn_foreign_roots`, `note_escaped_store_roots`). Safe: `RunnerState.exits`/`results` (the
stabilized `eval_heap` copy), `references`/`Timers.refs` keys, ETS (own arena), `RuntimeContext` (no
`Eterm` field), `dist`/`port` structs, `PortMeta`.
* **Envelope/retained payloads:** `Process::run_rooted_collection`
(`beam_process/src/process.rs:1166-1240`) roots each queued message's `payload`, `from` and `token`
and re-installs all three (`set_from`/`set_token`/`with_payload`), so a heap `from`/`token`
relocates too; the envelope's `off_heap`/`binaries`/`nodes` families and the
`MailboxState::remove_at` retained copies are **refcounted Rust handles** (not young-heap `Eterm`
words), so a moving heap can neither move them nor dangle them, and `with_payload` carries the
`Fragments` chain across. `beam_process`'s `deliver_down_message` sets `Message.from` to the target
pid's `to_eterm()` (an immediate) and carries a named monitor's `{Name,Node}` in the body, so
`run_rooted_collection` relocates it with the payload.
* **The arena allocation source here:** `Runtime::alloc_words` (`runtime.rs` → `self.arena`), the
fallback `RuntimeContext::alloc_words` reaches after a young-heap refusal and a `grow_nursery`, also
reached by `runner/budget.rs`'s forwarded `alloc_words` and by `Runtime::materialize`/`charlist`;
mailbox `Queue` payloads are copied in by `copy_struct`.

## Cross-node handoffs (crate index)
A by-owner index of the long forms in `## Cross-node handoff detail` above; the owning doc holds the full detail.
* **Differential gate** → [`../tools/differential/CONTEXT.md`](../tools/differential/CONTEXT.md) (re-run `gate.sh` for the live figure).
* **A.7/A.9 boot (completion)** → the bullet above + [`src/boot/CONTEXT.md`](src/boot/CONTEXT.md) + [`src/native/CONTEXT.md`](src/native/CONTEXT.md).
* **A.9 E0 (in-process `compile:file`)** → `tests/compile_file.rs`; mover: [`EPP_SCHEDULED_ROUTE_FINDINGS.md`](EPP_SCHEDULED_ROUTE_FINDINGS.md).
* **`beam_bifs` — the `io:format/2` `~p`/`~w` fun-printing `badarg`** → the bullet above (`beam_bifs/src/io.rs`).
* **`beam_engine` — the stacktrace defects** → the duplicated top frame and the dropped entry-module frame (bullets above).
* **The native `file_server_2` service** → [`src/native/CONTEXT.md`](src/native/CONTEXT.md) + [`src/native/file_server/CONTEXT.md`](src/native/file_server/CONTEXT.md).
* **`beam_process` / `beam_message`** → the wake-list drain + `!Send`-friendly `SchedulerPool` (bullet above).
* **`beam_engine`** → the `MinimalContext` arena accessor (bullet above).
* **`beam_bifs` / `src/context` — the node identity** → the creation placeholder (bullet above + *Placeholders*).
* **Distribution, phase 2** → the `beam_process` remote-peer seam (bullet above).
* **`beam_io` / `beam_abi`** → `driver_async`, the `control` reply copy, the `ErlDrvEvent` identity, `erl_drv_monotonic_time` (bullet above).
* **`beam_bifs` — `ets:setopts/2` + the tid-ref mint** → *ETS ownership* above.
* **`beam_io` — the port table's one `RefCell`** → the bullet above.
* **`beam_message` — the copy-out's never-clean destination** → *`ets_copy` peak RSS* above; `uses_*` gates delegate to `beam_bifs::hook_sets`' exported rosters.
