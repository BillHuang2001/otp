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
  `badarg` for an unknown key. `erlang:statistics/0,1` and the other `RuntimeHooks` families stay gaps.

## Own-state answers (differ by construction, not bugs)
* `erlang:processes/0`, `erlang:ports/0`, `system_info(port_count)`, the `persistent_term`/
  `get_module_info` figures. `erlang:whereis(init)` **does** agree (the root process; oracle `<0.0.0>`).
* **Minor known divergences (all outside this node unless noted).** `persistent_term:info/0`'s `memory`
  is this runtime's own word total (`count` is exact). The process dictionary keys by raw `Eterm` word
  identity (`beam_process::Dict`), so `erlang:put({a},1), erlang:get({a})` is `undefined` here vs `1` in
  the oracle. `register/2` of an internal PORT id is a typed gap. `process_flag(save_calls,
  <non-integer>)` returns the previous value where the oracle raises `badarg` (`bif.c:1911`).

## Process-control families (`src/native/`)
`monitor(port, Port)` stays `unsupported` (`beam_process::PortTable` is private inside
`beam_io::PortRegistry`), as do a remote `{Name, Node}` monitor and `monitor/3`'s `{tag, T}` option
(`Monitor`/`MonitoredBy` carry no tag field). `exit/2,3` matches the oracle except an internal **port**
target answers `true` without closing the port, and a foreign/decoded reference or ETS id is `badarg` vs
the oracle's `true`. `spawn_opt`'s `{scheduler, N}`/`{async_dist, B}` stay `unsupported`; every other
option is applied for real.

## ETS ownership
The runtime-global `beam_bifs::TableRegistry` ([`Runtime::tables`]) keys tables by handle only;
`ets:new/2` never records an owner (`beam_ets::Table::owner` stays `None`), so `ets:info(T, owner)`
always answers `undefined`, an installed heir renders as `undefined`, and `ets:give_away/3`/`ets:setopts/2`
are unregistered (`{heir, …}` in `ets:new` is `badarg`). **No code deletes or transfers a table when its
owner dies** (`beam_process`'s `ProcessTable::terminate_into` is the sole finalizer and calls no subsystem
back), so the `{'ETS-TRANSFER', …}` message is never produced. A future owner-death sweep needs a
`beam_process` exit-observer seam (mirroring `DistSignalRouter`, invoked from
`../beam_process/src/ptab.rs:1061`) plus an owner index + batch `owner_died()` op on the registry, wired
by this crate.

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
**returned** `{error, {Reason, Text}}`, never a raise. `term_to_binary`/`binary_to_term`/`make_ref`
remain at the `NifHost` defaults; a second `load_nif` for a loaded module is `reload`; a NIF returning a
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
`driver_output` **is** delivered to its owner as `{Port, {data, Data}}`; `driver_select`/
`driver_set_timer` are real (one poller registration per direction; the timer mirrors into the port's
`TIMEOUT` task). Typed gaps: `driver_async` (no async pool; `Runtime` is `!Send`), `port_control/3` not
reaching `control`, a captured `send_term`, `ERL_DRV_USE`'s `stop_select`, no OS child (`os_pid` is
`undefined`). With no `erl_ddll:load_driver/*`, the `Name` doubles as the *search* (a `/`-containing name
is the object path, driver name = its file stem; else `<name>.so`). The send family: `erlang:send(Port, Data)`
needs the port write path and the distributed `{Name, Node}` send is contract A.5; both raise
`unsupported: …`, never a fabricated value.

## Timers
`send_after` answers a `#Ref<>`; `cancel_timer(Pending)` the remaining ms, `cancel_timer(Fired|Cancelled|unknown-ref)`
`false`, `cancel_timer(non-ref)`/a bad time or destination `badarg`. Unimplemented: `erlang:system_info/2`,
`read_timer/1,2`, `start_timer`'s absolute form, and every other `RuntimeHooks` key.

## Heap model
`DEFAULT_HEAP_WORDS` is `1 << 24` (16 Mi words of lazily-mapped address space). The term-construction
heaps (the BIF-heap pool + the `-eval` heap) are plain bump arenas sized `config.heap_words`; `+hmax N`
is a **real** cap on that capacity (`runtime.rs::term_heap_words`). The `system_limit` translation is
applied **after** the copy engine's own retry (so `deliver_message` still matches the **raw**
`AllocKind::Message` to trigger the heap-fragment retry). This crate drives no collector (only
`HeapPolicy`/`next_heap_size`/`DEFAULT_MIN_HEAP_WORDS` plus read-only `HeapInfo`/`HeapObservable` for
`process_info`).
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
* **`beam_process` / `beam_message` split borrow.** `beam_message::send_fragmented` wants the receiver's
  **heap** and **mailbox** at once, but `Process` exposes them only through two `&mut self` accessors, so
  this crate composes the public fragment primitives directly and `MailboxState::remove_at` retains a
  removed message's `Fragments` so a just-bound payload stays valid; an owning drain for the wake list
  would delete both. **`Process` needs a split accessor handing out `&mut Heap` *and* `&mut OffHeapTable`
  together** (today `heap_mut`/`off_heap_mut` are two `&mut self` methods and the fields are private)
  before `beam_message::Fragments::move_into` can be called at receive — which also needs heap growth,
  since the fragment payload is larger than the receiver's ungrowable M1 heap. Plus a `!Send`-friendly
  `SchedulerPool` for SMP adoption (`Runtime` holds `Rc`/`RefCell` tables and `*mut Eterm` heaps, so
  `beam_process`'s `unsafe impl Send` escape is unavailable here) and a `tag` field on
  `Monitor`/`MonitoredBy` (for `monitor/3`'s `{tag, T}`).
* **`beam_io` lifecycle.** Finish the driver-port lifecycle (the `async` default, `port_control/3`
  reaching `control`, a delivered `send_term`, `ERL_DRV_USE`'s `stop_select`, a per-registration
  `ErlDrvEvent` identity), and revisit `open_port({spawn_executable, <a DIRECTORY path>}, [])` (oracle: a
  port; here: `error:eacces`). An accessor `PortRegistry::port_table_mut() -> &mut
  beam_process::PortTable` is also needed to serve `monitor(port, Port)`.
* **`beam_bifs` registry key allocation.** `BifRegistry::contains`/`lookup` build owned `(String, String)`
  key tuples per lookup, and the dispatch path does both (`contains` in `call_bif`, then `lookup` inside
  `BifRegistry::call`) — ~4 small `String` allocations per registered call, the largest attributable
  dispatch residual (measured ~60-80 ns/call alongside the caller-module clone). A borrowed-key
  `lookup(&str, &str, u32)` would remove it.
