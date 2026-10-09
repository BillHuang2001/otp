# `beam_rt` — Known M1 Gaps & Cross-Node Handoff Detail

Companion to [`CONTEXT.md`](CONTEXT.md). It holds the current-state per-item detail behind `CONTEXT.md`'s
condensed `Known Issues / Intentional M1 gaps` bullets and its longest cross-node handoffs, split out so
`CONTEXT.md` stays under the Context Tree per-file read limit. Every gap below is a typed capability gap
or a measured own-state difference — never a wrong result. Present-state documentation, not a change log.

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
* **`erlang:memory/0,1` (the `MemoryHook` seam).** `RuntimeMemory` (`src/context/memory_hook.rs`)
  returns the nine oracle keys in oracle order with `total = processes + system`, every figure
  **measured** (process heaps from the `ProcessTable`'s collectors, atoms from the shared `Rc<AtomTable>`,
  binaries from `beam_alloc::registry`, `code` from loaded `.beam` sizes, `ets` from the `TableRegistry`,
  `system` = those plus the remaining allocator live bytes excluding `Standard`/`EHeap`), snapshotted at
  construction (built only on the `uses_memory_hook` arm); `memory/1` answers in the requested order and
  `badarg` for an unknown key. `erlang:statistics/0` — and, on `statistics/1`, the keys this runtime
  cannot measure (`runtime`, `garbage_collection`, `run_queues`, `*_run_queue_lengths*`, `active_tasks*`,
  `scheduler_wall_time_all`, `microstate_accounting`) — stay gaps, as do the other `RuntimeHooks`
  families.

## Own-state answers (differ by construction, not bugs)
* `erlang:processes/0`, `erlang:ports/0`, `system_info(port_count)`, the `persistent_term`/
  `get_module_info` figures. `erlang:whereis(init)` **does** agree (the root process; oracle `<0.0.0>`).
* **Minor known divergences (all outside this node unless noted).** `persistent_term:info/0`'s `memory`
  is this runtime's own word total (`count` is exact). The process dictionary keys by raw `Eterm` word
  identity (`beam_process::Dict`), so `erlang:put({a},1), erlang:get({a})` is `undefined` here vs `1` in
  the oracle. `register/2` of an internal PORT id is a typed gap. `process_flag(save_calls,
  <non-integer>)` returns the previous value where the oracle raises `badarg` (`bif.c:1911`).

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
copy); `Runtime::own_heir_gift` mitigates only the `ets:new/2` path.

## `process_info/1,2`
The heap items read the target process's own policy (`Process::collector().policy()`), so a `spawn_opt`
min-heap / `fullsweep_after` / `max_heap_size` is observable. Two `max_heap_size`-map divergences remain:
the key order here is `kill, error_logger, include_shared_binaries, size` (oracle: `error_logger, include_shared_binaries, kill, size`)
and the per-map flags are not honoured; `message_queue_data` answers `on_heap`. ~28 items are answered
from this runtime's real state (catalog in `src/native/CONTEXT.md`); the four **execution-position**
items (`current_function`/`initial_call`/`current_location`/`current_stacktrace`) only for a scheduled
process whose entry is a module export, and any other valid oracle item it cannot answer (`catchlevel`,
`magic_ref`, `priority_messages`, `binary`, ...) raises the typed `unsupported`. `registered_name`'s
wrapping and `process_info/1`'s fixed `pi_1_keys` order are exact; `total_heap_size` is **466** (vs the
oracle's 233) and `memory`/`reductions`/`group_leader` are this runtime's own figures. A pid not in the
table is `undefined`; an unknown item or a non-pid is `badarg`.

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
admission point; a `receive ... after T` with a **non-statically-readable** delay (Y-register or
`after infinity`) still parks; a **capability gap is uncatchable** on the runner path.

## NIF (A.3)
`load_nif/2` really `dlopen`s the shared object and runs the C NIF; a load failure is the oracle's
**returned** `{error, {Reason, Text}}`, never a raise. `NifHostImpl` implements the runtime-dependent
hooks: `term_to_binary`/`binary_to_term` (ETF), `existing_atom`, `unique_integer` and `system_info`
(whose `scheduler_threads` is this runtime's **real** normal-scheduler count). `make_ref` and
`whereis_pid` need runtime state the host does not own, so they arrive through the borrowed
`NifHostServices` provider (`RuntimeNifServices`: the runtime's reference-id allocator, reference
registry and name table) threaded into the **per-call** host; the **load/unload** host lives in
`Runtime::nifs` (so it cannot borrow the runtime) and is built with `services = None`, leaving those two
at the trait default `None` there. A second `load_nif` for a loaded module is `reload`; a NIF returning a
non-term is caught by `try_call_nif` as a typed error, never an abort.

## Funs
`fun M:F/A` builds a real `FunKind::External` (`EXPORT_EXT`); a **compiled-module** `make_fun3` closure
gets its five identity fields from `FunIdentityBridge`, so `term_to_binary/1` is byte-exact
(module-scoped to the **caller**). An **`erl_eval` lambda** stays the honest `unsupported: local_fun`
(its oracle image closes over `erl_eval.erl` internals); no wrong encoding is emitted.

## Ports, drivers and the send family (A.4)
The read path is thin (only **stdout** polled, stderr discarded, one `Read::read` per readiness);
`connected` flips to `false` only at `port_close/1`; a `binary`-stream read wider than 64 bytes is the
typed *unsupported* gap. `os:cmd/1` stays the runtime-native host spawn. A `{spawn_driver, Name}` port's
`driver_output` **is** delivered to its owner as `{Port, {data, Data}}`; `driver_select` is real, and `driver_set_timer` arms for every attached port (the C substitutes a no-op
`no_timeout_callback` when the entry declares no `timeout`, so `-1` only fires for an invalid port or an
overflowing deadline). Typed gaps: `driver_async` (no async pool; `Runtime` is `!Send`), `port_control/3`
not reaching `control`, a captured `send_term`, `ERL_DRV_USE`'s `stop_select`, no OS child (`os_pid` is
`undefined`). The six `erl_ddll` registry BIFs (`loaded_drivers/0`, `try_load/3`, `try_unload/2`,
`monitor/2`, `demonitor/1`, `info/2`) stay typed `unsupported`: `loaded_drivers/0` would report this
runtime's own (empty) driver set where the oracle lists its statically-linked `tcp_inet`/`udp_inet`/
`ram_file_drv` — a by-construction own-state divergence never worth fabricating — and the corpus covers
none of them. With no `erl_ddll:load_driver/*`, the `Name` doubles as the *search* (a `/`-containing name
is the object path, driver name = its file stem; else `<name>.so`). The send family: `erlang:send(Port, Data)`
needs the port write path and the distributed `{Name, Node}` send is contract A.5; both raise
`unsupported: …`, never a fabricated value.

## Timers
`send_after` answers a `#Ref<>`; `cancel_timer(Pending)` the remaining ms, `cancel_timer(Fired|Cancelled|unknown-ref)`
`false`, `cancel_timer(non-ref)`/a bad time or destination `badarg`. Unimplemented: `erlang:system_info/2`, `read_timer/1,2`,
`start_timer`'s absolute form, and the roster keys this bridge does not serve (the `erl_ddll` registry
family, `alias/1`/`unalias/1`, `delete_module/1`/`purge_module/2`, `port_get_data/1`/`port_set_data/2`,
`term_to_iovec/1,2`, `erlang:is_builtin/3`, …); each is the typed `unsupported` gap.

## Heap model
`DEFAULT_HEAP_WORDS` is `1 << 24` (16 Mi words of lazily-mapped address space). The term-construction
heaps (the BIF-heap pool + the `-eval` heap) are plain bump arenas sized `config.heap_words`; `+hmax N`
is a **real** cap on that capacity (`runtime.rs::term_heap_words`). The `system_limit` translation is
applied **after** the copy engine's own retry (so `deliver_message` still matches the **raw**
`AllocKind::Message` to trigger the heap-fragment retry). This crate **does** drive `beam_gc`: a *scheduled* process allocates on its own young heap and is
collected at `ExecutionContext::ensure_heap` (armed by the slice's `BudgetContext`, roots = `x[0..live)`
plus the Erlang stack), pinned by `tests/gc_collect.rs` and reported by `Runtime::gc_stats()`; every
other context is a no-op on the unbounded arena. The limits are honest and measured: a workload that
allocates only through `ExecutionContext::alloc_words` (no `test_heap` admission point) always falls back
to the pointer-stable arena and is never collected, `Machine`'s float-register file is not mirrored by
this crate's root publish (so a module with a float-family instruction is never collected over), and a
failed collection halts that process's collection rather than failing the program.
**On-demand heap growth is NOT adopted in M1.** `beam_gc` exposes the growth seam
(`Collector::ensure_capacity`/`HeapPolicy::unbounded()`/`GcConfig::growable()`), but wiring it needs
sibling changes, so heap exhaustion still raises `error:system_limit`: the engine `ensure_heap` hook is a
no-op and BIFs build terms on a `beam_alloc::Heap` (`BifContext::heap()`) caching raw `hp`, so the live
set is invisible to `roots`; the bounded heaps (`Runtime::bif_heaps`/`eval_heap`) are bump arenas that
never move. To adopt: a reservation-capable `Heap` + re-derivable top/`hp`, `beam_engine` `alloc_words`
re-basing, and `beam_bifs` per-object `hp` re-fetch.

## Heap switches
`+hms`/`+hmbs` set a `beam_gc::HeapPolicy` rounded through the growth table (`+hms 1000` → 1598),
`+hmax*` the `max_heap_size` map; a malformed value is exit 2 where the oracle exits 1.

## Cross-node handoff detail
* **`beam_process` / `beam_message` split borrow.** The split accessor exists: `Process::heap_and_off_heap_mut()`
  (`beam_process/src/process.rs:579`) returns `(&mut Heap, &mut OffHeapTable)` in one borrow, and
  `Process::heap_and_mailbox_mut()` returns `(&mut Heap, &mut Queue)`, so calling
  `beam_message::Fragments::move_into` at receive is no longer blocked by borrowing.
  The remaining blockers for adopting it here are heap growth — the fragment payload is larger than the
  receiver's ungrowable M1 heap — and an owning drain for the wake list, without which this crate keeps
  composing the public fragment primitives directly and `MailboxState::remove_at` retains a removed
  message's `Fragments` so a just-bound payload stays valid.
  Plus a `!Send`-friendly `SchedulerPool` for SMP adoption (`Runtime` holds `Rc`/`RefCell` tables and
  `*mut Eterm` heaps, so `beam_process`'s `unsafe impl Send` escape is unavailable here).
* **`beam_io` / `beam_abi` lifecycle.** Finish the driver-port lifecycle (the `async` default — needs a
  `beam_abi::DriverHost` seam that can fire `ready_async`, since `beam_rt` is `#![deny(unsafe_code)]`;
  `port_control/3` reaching `control`; a delivered `send_term`; `ERL_DRV_USE`'s `stop_select`; a
  per-registration `ErlDrvEvent` identity). `erl_drv_monotonic_time`'s phase differs because
  `beam_abi/src/driver/drv/mem.rs` reads `CLOCK_MONOTONIC` from the system epoch where the oracle returns
  the Erlang monotonic time (a process-global origin shared with `beam_bifs`' `time.rs`).
* **`beam_bifs` registry key allocation.** `BifRegistry::contains`/`lookup` build owned `(String, String)`
  key tuples per lookup, and the dispatch path does both (`contains` in `call_bif`, then `lookup` inside
  `BifRegistry::call`) — ~4 small `String` allocations per registered call, the largest attributable
  dispatch residual (measured ~60-80 ns/call alongside the caller-module clone). A borrowed-key
  `lookup(&str, &str, u32)` would remove it.
