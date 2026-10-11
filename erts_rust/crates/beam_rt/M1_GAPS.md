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
spent (one reduction per heap-admission point, `ensure_heap`/`ensure_heap_full`), keeping the parked `Machine` at
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
`{spawn_driver, Name}` resolves the name against the **built-in driver table** (`tcp_inet`/`fd`/`efile`)
before any `dlopen` exactly as the C does (`io.c:599-628`) — an unknown name is still `badarg`. A
`{spawn_driver, Name}` port's `driver_output` **is** delivered to its owner as `{Port, {data, Data}}`,
and a driver-emitted `erl_drv_output_term`/`erl_drv_send_term` term reaches the connected process or the
explicit destination (the `TermQueue` drain is wired end to end). `driver_select` is real, and
`driver_set_timer` arms for every attached port (the C substitutes a no-op `no_timeout_callback` when
the entry declares no `timeout` slot, so `-1` only fires for an invalid port or an overflowing
deadline). An `OUTPUT` port task always drains the port's output queue through the single write path
(the entry's `outputv`/`output`) and *then* runs the entry's `ready_output` as the write-**readiness**
notification, so a queued `port_command` buffer reaches the driver whether or not the driver armed a
writable fd. The driver's `control` callback **is** routed behind `erlang:port_control/3`, and
`ERL_DRV_USE`'s `stop_select` **fires** on the cancel/deselect handback. The **driver-private queue**
(`driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/`driver_peekq` — a fresh port's `sizeq` is `0`),
`driver_pdl_create` and the port **state** flags (`get_port_flags`; `set_control_flags` stores into the
port's own control-flags cell) are implemented, so the host covers **23 of the 40** `DriverHost` hooks.
Typed gaps: `driver_async` (no async pool; `Runtime` is `!Send` and the `invoke`/`free` pair is
`unsafe`); the process-monitor family (`driver_monitor_process`/`_demonitor_process`/
`_get_monitored_process` — a returned monitor could never be correlated again without a safe
`monitor_identity` reader on `beam_abi`); a `control` reply the driver answered from **its own** memory
(needs a safe copy helper on `beam_abi`); an OS child (`os_pid` is `undefined` for a driver port, and
`take_exit_status` is always `Ok(None)`); a built-in (`spawn`/`spawn_executable`/`fd`) port's
`port_control/3` answering `[]` where the oracle raises `badarg` (a gap owned by `beam_io`'s default
`Driver::control`); and a driver arming **more than one** registration per direction (the typed
`unsupported`).
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
`beam_process`); `node/0` and the node atom a pid image carries **do** reflect the live
`-name`/`-sname` node (`Runtime::local_node()` is stamped on every `call_registered` arm), but the
`NodeIdentity` reports creation `0` (the placeholder above), so a decoded local id is still external
(see the `beam_codec`/`beam_bifs` handoff). `DOP_SEND`/`DOP_REG_SEND` already reach
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
collected at `ExecutionContext::ensure_heap`/`ensure_heap_full` (sharing `collect_or_grow`; the latter is the
full-root door `test_heap` reaches, handing over the FR file too; armed by the slice's `BudgetContext`, roots
= `x[0..live)` plus the Erlang stack), pinned by `tests/gc_collect.rs` and reported by `Runtime::gc_stats()`; every
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
**On-demand heap growth is adopted at the engine's heap-admission point and on the `beam_process`
message-delivery path.** `src/context/gc.rs::ensure_heap`/`ensure_heap_full` (sharing `collect_or_grow`) run the collector's on-demand **growth**
(`Process::ensure_capacity`) whenever the process's own heap-size policy can reach the request
(`HeapPolicy::next_words(need, 0) >= need`) and a plain `Process::collect` otherwise, so a `test_heap`
request larger than the nursery is admitted by growing it in place (`tests/heap_growth.rs`).
A bounded `+hmax N` policy the request cannot reach is refused **before** collecting, keeping the
fallback behaviour exactly (the collection still runs, the allocation falls back to the arena).
`Process::deliver` grows the nursery the same way for a refused message copy; the refusal is what
reproduces the raw `AllocKind::Message` that `deliver_message` keys its heap-fragment fallback on (an
unbounded receiver would instead absorb the payload into its grown nursery). A **relocation-exempt**
receiver skips the collect-and-grow retry entirely. Allocation **outside** the admission point still does
not grow, so exhaustion there still raises `error::system_limit`: BIFs build terms on a `beam_alloc::Heap`
(`BifContext::heap()`) caching raw `hp`, so the live set is invisible to `roots`, and the bounded heaps
(`Runtime::bif_heaps`/`eval_heap`) are bump arenas that never move. To adopt: a reservation-capable
`Heap` + re-derivable top/`hp`, `beam_engine` `alloc_words` re-basing, and `beam_bifs` per-object `hp`
re-fetch.

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
  `-boot FILE` drives it (`src/cli.rs::drive`), a plain start keeps the preloaded-only floor, and the
  boot leaves a live `logger_server` registered as `logger`. Remaining cross-node gaps: (1)
  **`beam_engine`** — a nested run's `receive … after T` timeout does not fire, so it folds into
  `error::blocked()` (the OTP-lifecycle item 7); (2) **`beam_bifs`** — whatever the started applications
  still need. Detail: [`src/boot/CONTEXT.md`](src/boot/CONTEXT.md).
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
  `MailboxState::remove_at` retains a removed message's heap `Fragments` and its whole off-heap
  family so a just-bound payload stays valid — the refc-binary and MSO handles (the MSO handles in
  the envelope's own order, so their `[HEADER_BIN_REF, slot, 0]` container terms resolve against the
  copy's rebuilt source table) and the external-thing node identities (`NodeEntry` share handles).
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
* **`beam_bifs` / `src/context` — the node identity.** The live node atom is fed from
  `Runtime::local_node()` and stamped on every `call_registered` arm, so `erlang:node/0` and the node atom
  a pid image carries reflect a live `-sname`/`-name` node; the residual is the **creation** — the hook
  still reports the fixed `0` where a live `crate::dist::DistNode::creation()` exists, so an internal
  pid/port image on a wired node encodes creation `0`.
* **Distribution, phase 2** — the inbound link/exit/monitor/alias `DOP_*` families need a
  `beam_process` **remote-peer** seam (plus an alias table); `DOP_SEND`/`DOP_REG_SEND` already reach
  local mailboxes, and a live-peer loop past the handshake needs the `FragmentAssembler` receive-buffer
  coupling.
* **`beam_io` / `beam_abi` lifecycle.** The **process-monitor family** is blocked on a safe `beam_abi`
  reader `monitor_identity(mon: *const ErlDrvMonitor) -> Option<[u8; 24]>`: the frozen hook
  `monitor_process(&mut self, pid: Eterm) -> Option<ErlDrvMonitor>` never sees the caller's out-parameter,
  and `demonitor_process`/`get_monitored_process` receive an `*const ErlDrvMonitor` whose 24-byte identity
  may not be read under `#![deny(unsafe_code)]`, so the three stay the C's failure code. `driver_async`
  needs a `beam_abi::DriverHost` seam that can fire `ready_async` (since `beam_rt` is
  `#![deny(unsafe_code)]`); a `control` reply the driver answered from its own memory needs a safe copy
  helper; a per-registration `ErlDrvEvent` identity is still owed. `erl_drv_monotonic_time`'s phase differs
  because `beam_abi/src/driver/drv/mem.rs` reads `CLOCK_MONOTONIC` from the system epoch where the oracle
  returns the Erlang monotonic time (a process-global origin shared with `beam_bifs`' `time.rs`).
* **`beam_io` — a safe filesystem accessor.** A safe `path_access(Path)` (a `libc::access` wrapper) plus
  `process_credentials()` (effective uid/gid) would close the `#file_info{}` **`access`** slot, and so the
  `filelib`/`file:read_file_info` existing-path **success** cases; only the stat-failure path is served
  today (`src/native/file_server/file_info.rs`), because `access(2)` is reachable only through `libc::access`,
  which `#![deny(unsafe_code)]` forbids.
* **`beam_message` — an off-heap refc-binary copy mode.** `copy_struct_simple` builds **empty** source and
  destination off-heap tables, so `copy_refc_binary` cannot resolve a `RefcBinary` payload's backing store
  (`binary_by_data_ptr` misses) and returns `bad_term` (`BeamError::BadArg`). A self-contained "flatten"
  copy mode would let `src/runtime.rs::stabilize_term` carry an off-heap result across; the `beam_rt` half
  is that single `copy_struct_simple` call, pinned red by `tests/empty_binary_result.rs`'s `#[ignore]`d
  Test B.
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

## OTP-lifecycle survey — what a scheduled process can and cannot do today
Method: self-contained `.erl` fixtures compiled by the system `erlc` (OTP 29.1.1 / erts-17.1, the oracle)
into `$TMPDIR`, each exporting arity-0 probes, run three ways:
`erl -noshell -pa <fix> -eval 'io:format("~w~n",[M:F()])' -s init stop` (oracle),
`erts_rust/target/debug/beam_rt -noshell -pa <fix> run <fix>/M.beam M:F/0` (**scheduled** process),
and `… beam_rt -noshell -pa <fix> -eval '…'` (runtime root process, which **pumps** the scheduler).
Exit 0 = success, 1 = runtime error, 3 = capability gap.

### Works today (oracle-identical)
`code:which/1`, `code:load_file/1` (then `module_loaded/1` true), `application:get_key/1,2`,
`application:loaded_applications/0`, `application:which_applications/0`, `error_logger:warning_map/0`
(a `RuntimeHooks` bridge). **An OTP-behaviour child comes up and survives**: `proc_lib:start_link/3`,
`gen_server:start_link/2,3` (unnamed and `{local,Name}`), `gen_server:start/2,3` and
`supervisor:start_link/2` all return a live `{ok, Pid}` (`is_process_alive/1` true). A
`spawn`/`spawn_link` child whose **entry function** itself blocks in a `receive` parks and is resumed
(`spawn(M,F,[])` with the blocking receive at the top level of `M:F`, incl. via a cross-module call,
`erlang:apply(M,F,[])`, or a local fun), and so does a cross-module call whose callee parks on a
`receive` that a later **message** matches. Local fun application with free variables works while the
closure's body can complete without a suspension; a closure that is the process *entry* is driven
in-machine by the runner. `gen_server:cast/2` returns `ok`
(it is a plain `!`). Plain
`!`/`receive`, `erlang:monitor(process,P)` (arity 2) and `erlang:monitor(process,P,[{tag,T}])` work, as
does a `gen_server:call(Pid, Req, infinity)` / `gen:call(Pid, '$gen_call', Req, infinity)` round-trip.
The whole-file `file` family completes from a **scheduled** process (`file:read_file/1`,
`file:write_file/2`, `file:open/2` for read and write, `file:close/1`, `file:list_dir/1`,
`file:get_cwd/0`, `file:consult/1`, and `filelib:is_file/1` — an existing path `true`, an absent one
`false`); from `-eval` (pumped) the incremental io-device read path agrees too (see "Path asymmetry").

### Confirmed failing — root cause and repro
1. **`gen_server:call/2,3` and `gen:call/3,4` with a finite timeout raise `badarg`.** Root cause:
   `gen:do_call/4`'s finite-timeout clause (`lib/stdlib/src/gen.erl:251`) does
   `erlang:monitor(process, Process, [{alias,demonitor}])`; this runtime's `monitor/3` serves only the
   `{tag,T}` option, so the alias option raises `badarg` (and the reply tag `[alias|Mref]` is then
   `badarg` too). `erlang:send/3` (the `[noconnect]` send the same clause uses) is the typed gap
   `unsupported: erlang:send/3`. **`gen_server:call/2` is not the infinity form**: `gen:call/3` is
   `gen:call(Process, Label, Request, ?default_timeout)` with `?default_timeout = 5000`, so the most
   common call form takes the finite clause too. Repro: `mon:alias_mon/0`, `mon2:alias_reply/0` →
   `badarg` (oracle: a `#Ref{}`); `gcx:call_pid_2/0` → `{'EXIT',{badarg,{gen_server,call,[Pid,ping,5000]}}}`;
   `gcx:call_name_2/0` → `{'EXIT',{badarg,{gen_server,call,[Name,ping]}}}`. The measured form split
   (oracle prints `pong`/`{ok,pong}` in every row; fixture = a hand-rolled `{'$gen_call', {From, Mref},
   ping}` server, each call `catch`-wrapped):
   | call | beam_rt, scheduled process |
   |---|---|
   | `gen_server:call(Pid, Req)` (arity 2 ⇒ `gen:call/3` ⇒ finite 5000) | `{'EXIT',{badarg,{gen_server,call,[Pid,Req,5000]}}}` — **caught** by the caller |
   | `gen_server:call(Pid, Req, 5000)` | the same `badarg` (caught) |
   | `gen_server:call(Pid, Req, infinity)` / `gen:call(Pid, '$gen_call', Req, infinity)` | **`pong` / `{ok,pong}` — works** (the pid clause of `gen:call/4` calls `do_call/4` directly, so the blocking `receive` is ordinary machine code and suspends/resumes normally) |
   | `gen_server:call(Name, Req)` / `(Name, Req, 5000)` | `{'EXIT',{badarg,{gen_server,call,[Name,…]}}}` — **caught** (the alias `badarg` fires) |
   | `gen_server:call(Name, Req, infinity)` / `gen:call(Name, '$gen_call', Req, infinity)` | **`pong` / `{ok,pong}` — works** |
   So the split is by **timeout**, not by pid-vs-name: **`infinity` works** for both a pid and a
   registered name; **any finite timeout** dies of the `monitor/3` alias gap (a `beam_rt`-visible floor
   gap, whose real fix — alias refs — is `beam_process`/`beam_bifs` work).
2. **`sys:get_state/1` and every `sys:*` request raise `badarg`.** `sys` issues
   `gen:call(Name, system, Request)` (`lib/stdlib/src/sys.erl:749`) → the same monitor-alias `badarg`
   (its default timeout is finite). Repro: `sy:gostate/0` → `{'EXIT',badarg}` (oracle: `[]`).
3. **`logger` / `error_logger` cannot emit a report on a plain runtime.** `logger:warning/1`,`notice/1`,
   `error/1` and `error_logger:info_msg/1,2`,`error_msg/2` raise `badarg`. Root cause: a plain
   `run`/`-eval` invocation loads only the preloaded modules — it does not run the boot script, so no
   `logger` process exists (the boot-script path *does* register one), the `logger` ETS config table
   never exists, `ets:whereis(logger)` is `undefined` (oracle: a `#Ref{}`), `logger_config:allow(Level)`
   defaults the primary level to `notice` and admits a `notice`+ event, then
   `logger_backend`→`logger_config:get(Tid=undefined, primary)` → `ets:lookup(undefined, primary)` →
   `badarg` (`etsl:lookup_undefined/0` = `{error,badarg}` in **both** runtimes). `logger:info/1,2`
   returns `ok` **only because it is filtered** below the default `notice` level (`allow(info)` = false).
   The residual is the missing native logger ETS floor: `lg:warn/0` → `{'EXIT',badarg}`, `lg:info/0` →
   `ok`, `lg:elog_info/0` → `{'EXIT',badarg}`.
4. **`application:start/1,2`, `application:stop/1`, `application:ensure_all_started/1` fail.**
   On a plain runtime no `application_controller` process exists (it is spawned only on the `-boot`
   path): `{'EXIT',{noproc,{gen_server,call,[application_controller,{load_application,sasl},infinity]}}}`
   (scheduled) / `exit <structured reason>` (`-eval`). The native floor answers only the read-only
   queries (items in "Works today"); no application's `start/2` runs and no supervision tree becomes
   live. Repro: `ap:start_sasl/0`, `ap:start_syntax/0`, `ap:ensure_syntax/0`.
5. **The incremental io-device read path from a scheduled process exits 3** — `file:read/2` and
   `file:read_line/1` both `unsupported: opcode: run: process blocked (no message available)`; each
   agrees with the oracle from `-eval` (see the path asymmetry). The whole-file `file` family (item in
   "Works today") completes from a scheduled process.
6. **`code:get_object_code/1` raises `badarg`** (oracle: `{Module,Binary,Filename}`); `code:which/1` and
   `code:load_file/1` are fine. `erlang:module_loaded/1` is truthful to this runtime's own loaded set, so
   `module_loaded(lists)` is `false` until `lists` is demanded (oracle: `true`).
7. **A `receive … after T` that must actually time out, reached through a nested run, folds into the
   typed `blocked()` gap.** `beam_rt -noshell -eval 'ev:mailbox()'`, where `ev:mailbox/0` is
   `receive M -> {got,M} after 100 -> none end`, exits 3 `unsupported: opcode: run: process blocked (no
   message available)` — a module export runs as a nested run whose `after` timeout does not fire —
   while the same export as the `run` entry (`run ev.beam ev:mailbox/0`) returns `none`. A
   **cross-module** call behaves the same way: `mwc:call_hard/0` → `mw:to_hard/0`
   (`receive M -> M after 20 -> timeout end`, no message) exits 3, whereas the same clause as a
   same-module local call (`mm:go/0` → `helper/0`) returns `timeout`, and a nested call woken by a
   **message** parks and resumes correctly (`mwc5:go/0` → `mw:wait_or_msg/0` returns the message). An
   inline `-eval 'receive … after T …'` works (the evaluator's own receive driver).

### The `-eval` vs scheduled asymmetry (the single most load-bearing observation)
The `-eval` root process **pumps** the scheduler and can suspend a blocking call; a **scheduled**
process (`run`) parks at its own top-level `receive` (a `receive … after T` timer fires there, and a
`receive` waiting for a message resumes correctly), but **a nested run it enters — a cross-module call,
or a fun application — does not deliver a `receive … after T` timeout**: it folds into the typed
`blocked()` gap instead. Hence the incremental `file` read path **agrees under `-eval` and exits 3
under `run`**, while a top-level `receive … after 100` **returns `none` under `run` and exits 3 under
`-eval` in a called module function** — the two directions of the same seam, not a per-API bug.
