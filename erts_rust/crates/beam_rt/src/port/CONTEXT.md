# `beam_rt::port` — the runtime's A.4 port layer (`../port.rs` provider + `driver.rs` host + `watch.rs` signal half)

## Intent
The runtime side of contract A.4, in three files.
`../port.rs` is the **port provider**: `open_port/2`'s `Name`/`Options` decoding, the poller/registry (`PortRegistry`), the port-task round and the `{Port,{data,..}}`/`{Port,{exit_status,..}}` message pump behind `beam_io`.
`driver.rs` is the runtime's **own** driver C-ABI host for `{spawn_driver, Name}` ports (contract A.4), written directly over `beam_abi`'s frozen `DriverHost` seam.
`watch.rs` is the **delivery half** of `os:set_signal/2`: the process's one `beam_io::signal::SignalFacility`, its registration with the runtime's one poller, and the drain that turns a handled signal into the `{notify, Signal}` the port pumps deliver.
It is deliberately **separate** from the sibling crate `beam_io`'s own host layer (`beam_io::abi`): `beam_rt` never references `beam_io::abi`, `make_loaded`, `AbiHost` or `DriverTable` (verified: no matches anywhere in the crate), so end-to-end `.so`-driver traffic goes through this file, not `beam_io`'s.
The native `port_command`/`port_info` floor lives in `../native/port.rs`.

## API Surface
### `port.rs` — the `PortHook` provider (A.4)
Public: `PORT_INFO_ATOMS` (incl. `registered_name`), `PortName{Spawn,SpawnExecutable,Fd,SpawnDriver,SpawnDriverBuiltin}`, `StreamType{List,Binary}` (list default), `IoCounters` (the node-global counters behind `erlang:statistics(io)`; the runtime keeps a clone of the same `Rc` so the arm reads them without borrowing the registry), and `RuntimePorts` (the `beam_bifs::PortHook` impl: `open_port/2`, `port_close/1`, `port_info/1,2`) built from `Rc<RefCell<PortRegistry>>` + the shared `Rc<AtomTable>` + the owning `ProcessId` (the `Rc<RefCell<..>>` is the ETS pattern — `FunBridge` needs `&mut Runtime` at the same time; a nested borrow is a typed gap, never a panic).
`PortRegistry` (private `{poller, registry, meta, builtin_drivers, heap, io_counters, watch}`): `new()` (anchors the `driver_*` codegen unit and builds the **one** `beam_io::DriverRegistry::with_builtins` `fd`/`efile`/`tcp_inet` table), the registry queries/commands (`is_live`/`contains`, `write_command`, `port_control`, `len`, `is_empty`, `has_nothing_to_poll`, `io_counters`, `ids`, `close`, `driver_lookup`, `create_builtin_driver`), and the private pass (`register`/`pump(timeout) -> Vec<PortEvent>`/`take_due_exit_statuses`/`take_driver_outputs`/`take_driver_terms`/`take_signals`/`info`); consumers are `Runtime::deliver_port_messages` + the `-eval` trio `poll_port_messages`/`port_data_message`/`port_exit_message`.
**Built-in driver resolution (the `{spawn_driver, Name}` fallback).** `RuntimePorts::open_port`'s `PortName::SpawnDriver` arm resolves the name against `create_builtin_driver` **before** any `dlopen`, exactly as the C resolves it against the loaded-driver list (`io.c:599-628`), so `open_port({spawn_driver,"tcp_inet"},[])` opens a real live port (the oracle answers `#Port<…>`) rather than failing to load a `.so` that does not exist. A **hit** registers the built-in `Box<dyn Driver>` through the same `PortRegistry::register`/`PortMeta` path with `kind = PortName::SpawnDriverBuiltin`; a **miss** keeps the `dlopen` path byte-identical (the existing `.so` driver tests are unchanged), and an unknown name is still `badarg` — a built-in whose factory fails surfaces that typed failure, never a fabricated port.
The two `PortName` predicates route the byte paths (`PortName::has_driver_control`/`bytes_from_driver_output`, both exhaustive `match`es so a new variant is a compile error): a built-in driver port is a **driver port** for `is_driver_port`/`port_control` (its driver has a `control` callback) but its bytes are **not** drained by `take_driver_outputs` — they arrive through its poller-registered descriptor on the port-task round, like every non-`.so` port, and `write_command` takes the ordinary `port_command` path for it.
`PortRegistry::port_control(id, op, arg)` (`../port.rs:629`) and `RuntimePorts::port_control(port, op, arg)` (`../port.rs:1280`) forward the verb **verbatim** to `beam_io::PortRegistry::port_control` (`beam_io/src/registry.rs:357`, which reserves the `"close"` verb), so for a `{spawn_driver, Name}` port the operation reaches `DriverPortAdapter::control` and a C driver's verb is the decimal `c_uint` string `erts_internal:port_control/3` produces from its integer `Command`. The **reachable** entry is `PortRegistry::port_control`: the native floor's `erlang:port_control/3` (`../native/port.rs`) calls it directly, so `RuntimePorts::port_control` is retained but has **no caller** (a `runtime.rs`-internal seam; `beam_bifs::PortHook` exposes no `port_control` method and needs none).
Two "is it empty" predicates: `is_empty()` counts **ports only** (`system_info(port_count)`), while `has_nothing_to_poll()` is the **nothing-pollable** predicate — no live port **and** no pollable signal watch — i.e. the honest "a blocked process can no longer be woken" test (a registered `handle` signal keeps the runtime awake with zero ports).
`pump` first `watch.sync`s the signal self-pipe into the poller, harvests the already-queued driver output/terms and any handled signal, then polls in `PORT_POLL_SLICE` (25 ms) slices clamped by the earliest driver deadline, runs the port tasks, harvests each driver's `driver_output`, and reports `PortEvent::{Data,Exited,Term,Signal}`; the sentinel `watch::SIGNAL_KEY` readiness event is filtered out before the port consumers see it (no port ever sees the signal pipe).
`SpawnDriver` is the **single** OS-child implementation (this crate owns only `open_port/2`'s decoding into a term-free `SpawnSpec`/`SpawnOptions`, plus `PortMeta`). `deliver_port_messages` delivers each event into the **owning** process's mailbox: `{Port,{data,..}}`/`{Port,{exit_status,..}}` to `owner`, a driver-emitted term (stabilized onto the pointer-stable arena) to `to.unwrap_or(owner)`, and `{notify, Name}` (sender `system`) to the pid `whereis` resolves for the interned `erl_signal_server` atom — each through `deliver_simple`, so a blocked receiver is woken; `close` calls `beam_io`'s `notify_port_exit(…, Closed)` **before** `registry.close(...)`.

The **read path and the child streams**: one `{Port,{data,Data}}` is built per driver read (`RuntimeContext::port_data_message` in `../port.rs`): a `binary` stream delivers a `binary` **at whatever width the child wrote** — up to `beam_io`'s `READ_CHUNK` (64 KiB) per `read(2)` — built by `beam_codec::term::make_binary` on the pointer-stable `eval_heap` (the on-heap `ErlHeapBits` layout at every width, so `terms::make_heap_binary`'s 64-byte limit never applies), and a `list` stream a char list. The child's **stderr is inherited** from the emulator by default — it is never port data — and the `stderr_to_stdout` option (`PortOptions::stderr_to_stdout`, `PortOptions::overlay`, `PortOptions::stream_options()` → `beam_io::StreamOptions` → `SpawnDriver::with_stream_options`) makes the child's descriptor 2 an exact duplicate of descriptor 1, so its diagnostics arrive as port data interleaved with stdout in true write order (the C's `stderrfd = opts->redir_stderr ? ifd[1] : dup(2)`, `erts/emulator/sys/unix/sys_drivers.c:500`).

### `port/driver.rs` — the driver C ABI host (A.4)
`Driver{library, host: Box<DriverPort>}` pairs a loaded driver with the host its callbacks run against (**field order is load-bearing**: the library's `Drop` runs the entry's `finish` before the host drops). `DriverPort` is the concrete `beam_abi::DriverHost` behind the opaque `ErlDrvPort_`: the handle **is** the per-port `DriverEnv`'s address (a driver-private term arena, the shared `AtomTable` + a `PendingInterner`, the owning `ProcessId`, and the output/pid services). `load_driver(candidates, name)` delegates to the **dedicated driver loader `beam_abi::driver::loader::load`** (a real `ErlDrvEntry` loader, distinct from the NIF loader and needing no host), tries each candidate (the loader appends `.so`) and reports the **last** typed error.
`DriverPort::binary_to_term` (the `DriverHost` hook `erl_drv_binary_to_term` reaches) decodes the driver's bytes with `beam_codec::decode_term`: the decoder needs `&mut AtomTable`, so the shared table is **cloned** (the `beam_bifs::binconv` precedent) and a name the clone appended is recorded on the pending interner at its predicted index (via `intern`, never `intern_atom` — the interner's `base` was fixed when the host was built) for the port's atom-commit step; an invalid image is the C's `ERTS_DDT_FAIL` (`None`), never a raise.
The `DriverHost` registry hooks carry the C's exact codes: `add_driver_entry`/`remove_driver_entry` (`add` returns `0`; `remove` `1`/`0`/`-1` per `io.c:7698`), `lock_driver` keyed off the port's load state (`io.c:7324`) and `create_port` = the invalid-port sentinel.
`DriverPort` also owns the per-port **driver-private state** the frozen seam has no term/home for, in `driver/state.rs`: the driver's private byte queue — the C's `prt->ioq` (`io.c:6790-6919`), an ordered list of enqueue **chunks**, one per successful `driver_enq`/`driver_pushq` — behind `enq`/`pushq`/`deq`/`sizeq`/`peekq`, plus the `pdl_created` one-PDL guard behind `pdl_create`. This queue is distinct from both the Erlang→driver output queue `beam_io` owns (`prt->queue`) and the port's *input* queue (what `driver_output` answers with). `DriverPort::with_port_flags` carries the port's **state** stream bits (`StreamType::port_flags` → `PORT_FLAG_BINARY` (1) for a `binary` port, else `0`), which `get_port_flags` reports — a **different** cell from the `control_flags` `set_control_flags` records for the `port_control` reply form (`io.c:5325-5344` vs `:5318`); `clear_driver_state` (called from `DriverPortAdapter::stop` **and** its `Drop`) drops the queue so a late drain never reads bytes of a port that has gone away.
`DriverPortAdapter` is the **port lifecycle** the host alone lacks — a `beam_io::Driver` over a loaded C driver: `start` runs the entry's `start` (the `ErlDrvPort` **is** the environment's address, kept in a `Box` that never moves), `output` hands the port's queued bytes to the entry's `outputv` when it declares one (**the C's preference**, `io.c:1782`/`io.c:1597`) and to `output` otherwise — so an **`outputv`-only** entry (an entry whose `output` slot is `NULL`) really receives `port_command` (`ErlIOVec` = an empty writable header `iov[0]` + the data `iov[1]`, parallel all-NULL `binv`, built on the caller's stack exactly as `io.c:1806/1842-1864`), queues the `driver_output` reply as the port's **input** (surfaced by `take_driver_outputs` as `{Port,{data,Data}}`), `stop` runs the entry's `stop`, and the drop runs `finish` + `dlclose`. It also seizes the host's `Rc<DriverMonitors>` at `load` and overrides `has_pending_async`/`drive_async`: that per-round drive point fires the entry's `process_exit` once for each marked monitor (`driver/monitor.rs`), and the tree is cleared in `stop` **and** at the top of `Drop`, so no callback can run after the library unloads. An `OUTPUT` port task (`Port::run_task` → `DriverPortAdapter::ready_output`) **always** drains the port's output queue through the single write path (`DriverPortAdapter::output` → the entry's `outputv`/`output`, the C's `port_command` delivery, `io.c:1782`/`io.c:1597`) and *then* runs the entry's `ready_output` slot, when the entry declares one, as the write-**readiness** notification (the C's `ERTS_PORT_TASK_OUTPUT` arm, `erl_port_task.c:1793-1797`) — never as a second write path; the write-armed descriptor handed to that notification is captured **before** the write, because `output`/`outputv` are free to re-arm with `driver_select`. `build.rs`'s `-Wl,--export-dynamic` publishes the `driver_*`/`erl_drv_*` symbols; `open_port({spawn_driver, Name}, Options)` routes here for a name no built-in driver answers, and an unknown or empty driver name is `badarg` (a name a built-in does answer is registered as a `SpawnDriverBuiltin` port instead, never `dlopen`ed — see the API Surface above).

### Driver-emitted terms (`erl_drv_output_term` / `erl_drv_send_term`) — `TermQueue`
`beam_abi` decodes the driver's term-data stack machine into a real term on the host's arena and calls `DriverHost::send_term(port, to: Option<Eterm>, term)`. The C *delivers* there and then (`driver_deliver_term`, `io.c:5664`: the decoded term itself, `from = prt->common.id`); the frozen seam gives the host no route to a receiver's mailbox, so the term is **queued**: `TermQueue` (`pub struct`, `Rc<RefCell<Vec<(Option<Eterm>, Eterm)>>>`) is per port and shared by handle — `DriverPort::term_queue()` returns the handle, `send_term` pushes onto it (while still recording the accumulate-only `DriverPort::sent` view the A.4 test reads), `take()` drains, `is_empty()` reports. `None` = the port's connected process (`erl_drv_output_term`, `io.c:6254`), `Some(pid)` = an explicit destination (`erl_drv_send_term`, `io.c:6289`). **Lifetime invariant:** a queued term's words live on the host arena, so `DriverPortAdapter` **empties** the queue both in `beam_io::Driver::stop` (the C refuses a delivery while `ERTS_PORT_SFLG_CLOSING`, `deliver_term_check_port`, `io.c:6213`) and in its `Drop`; a late drain observes an empty queue, never a freed arena.
**The drain is wired end to end**: `PortRegistry` seizes the handle as `meta.driver_terms` at `open_port/2`, `take_driver_terms` surfaces each queued term as `PortEvent::Term{port, to, term, owner}`, and the pump copies it onto the pointer-stable arena (`RuntimeContext::stabilize_driver_term`) and delivers it to the destination — the connected process for `None`, the explicit pid for `Some`. `tests/driver.rs` pins both paths against the oracle with `EVENT_DRIVER_MODE` 2/3 (`erl_drv_output_term` / `erl_drv_send_term`).

The precise symbols of `driver.rs`:
- `DriverPort` (`driver.rs:481`) — `impl beam_abi::DriverHost` (`driver.rs:720`); the concrete host behind
  the opaque `*mut ErlDrvPort_` a driver callback receives. The port handle **is** the per-port
  `DriverEnv`'s address. Its `binary_to_term` (`driver.rs:760`) is the `beam_codec`-backed decode above.
- `Driver { library, host: Box<DriverPort> }` (`364`) — field order is load-bearing (`library` drops first).
- `DriverPortAdapter` (`999`) — `impl beam_io::Driver` over a loaded C driver: `load`/`name`/`state`/`selected`,
  plus the callback mapping `start`/`output`/`ready_input`/`ready_output`/`control`/`timeout`/`stop`.
  `control(command: &str, arg: &[u8])` (`1633`) parses the verb as a `c_uint` and runs the entry's `control`,
  with the C's codes (see Known Issues); its reply source is resolved by the associated
  `control_reply(reply, request, rbuf, len)` (`1420`).
- `notify_ready(&mut self, ctx, fd: c_int, read: bool)` (`1326`) — runs the entry's `ready_input`/`ready_output`
  for one **already-resolved** descriptor (the C's `(ErlDrvEvent) state->fd`); `dispatch_ready(ctx, read)`
  (`1297`) keeps the resolution half, and an entry with no callback for the direction gets the C's
  `no_ready_*_callback` (log *and* deselect, `io.c:7523-7538`) before `settle`. `ready_output` resolves the
  fd itself so it can capture it before the write.
- `load_driver(candidates, name)` (`949`) — `beam_abi::driver::loader::load` per candidate, reporting the last typed error.
- The timer family matches the C for **every** live port: the C substitutes a silent `no_timeout_callback`
when the entry declares no `timeout` slot (`io.c:7601`), so `io.c:6918`'s `-1` stands only for an invalid
port (`DriverControl::attached == false`); `driver_set_timer` arms the deadline and answers `0`, and a
fired deadline runs the entry's `timeout` or, when absent, is the C's silent no-op.
The four driver-registry slots are **all overridden** (though `create_port`'s override is behaviourally identical to the trait default — both answer `ERTS_INVALID_ERL_DRV_PORT`):
  `add_driver_entry` (`891`, → `registry_add_entry`, `0`/`1`), `remove_driver_entry` (`897`, →
  `registry_remove_entry`, C's `1`/`0`/`-1`), `lock_driver` (`910`, `0` iff `control.driver_loaded`
  else `-1`), `create_port` (`927`, always `ERTS_INVALID_ERL_DRV_PORT`).
`DriverPort` implements **26 of the 40** `DriverHost` hooks: the 4 required ones (`heap`/`intern_atom`/`output`/`send_term`), `binary_to_term`, the timer family (`select`/`set_timer`/`cancel_timer`/`read_timer`), `connected_pid`/`caller_pid`, the driver-private queue family (`enq`/`pushq`/`deq`/`sizeq`/`peekq` → `driver/state.rs`'s `DriverQueue`), the port flags (`set_control_flags`, which stores into the `control_flags` cell; `get_port_flags`, which reports the port **state** flags `port_flags` — **not** `control_flags`), `pdl_create`, the monitor family (`monitor_process`/`demonitor_process`/`get_monitored_process`, over the `driver/monitor.rs` tree), and the 4 registry slots above. The other **14** keep the frozen trait default (the C's own failure code): `r#async` (`-1`), `set_busy_port`, the lifecycle family (`failure`/`port_exit`/`busy_msgq_limits`/`init_ack`/`set_os_pid`/`async_port_key`), the PDL refcount family (`pdl_lock`/`pdl_unlock`/`pdl_get_refc`/`pdl_inc_refc`/`pdl_dec_refc`) and `now`. They stay defaults because each needs runtime structure a per-port host holds no handle to (the port's busy flag and exit/OS-pid lifecycle state, which live in `beam_io::Port`; the `ProcessTable` for monitors, behind a seam that cannot even name the caller's `ErlDrvMonitor*`; a refcounted PDL *lock object* to lock/refcount — `pdl_create` hands out the port's own identity instead; `std::time` for `now`); `beam_io::abi`'s own host implements several of them, but `beam_rt` never references that host. A C driver calling any of the 17 gets the C's failure code (accepted, honest behaviour), never a fabricated success.

### `port/watch.rs` — the `os:set_signal/2` delivery half (`pub(crate)`)
One `thread_local!` `FACILITY: RefCell<FacilitySlot>`, holding the facility created lazily by the first `os:set_signal/2` and **never dropped in production** (a per-call facility would be dropped at the end of the call and `SignalFacility::drop` restores the prior dispositions), so a later call observes the disposition an earlier one installed. `HANDLED: RefCell<BTreeSet<Signal>>` mirrors which signals currently hold `Disposition::Handle` (a later `ignore`/`default` removes the entry), so `SignalWatch::is_pollable` answers without a syscall. Accessors `with_facility` (creates on first use) / `with_existing_facility` (never creates). `install(signal, disposition)` sets the disposition and updates the mirror; a failed install leaves the mirror untouched.
`SignalWatch { token: Option<Token>, fd: Option<RawFd> }` is the poller registration, owned by `PortRegistry`. `sync(poller)` registers the facility's `read_fd()` under the sentinel key `SIGNAL_KEY` (= `u64::MAX - 1`, distinct from the poller's own `WAKER_KEY`) on the first poll round after a `handle` disposition — idempotent, and a no-op when nothing is handled. `is_pollable()` is true once registered **or** a `handle` disposition exists (so a caught signal is observed even with zero live ports); a `handle` disposition the OS accepted but whose registration fails is surfaced, never silently leaving signals undelivered. `is_registered()`/`ready(events)`/`drain() -> Vec<&'static str>` (the `Signal::name()` atoms) round it out. `crate::context::signal` owns the oracle answer surface (`{ok, OldDisposition}`/`badarg`, the kernel-only `sigstop` rule) and delegates the facility here (Rust privacy: `context::mod` declares `signal` privately, so `port` cannot name `context::signal`, but `context::signal` can name `pub mod port`'s child).
The message itself is `RuntimeContext::signal_notify_message(name)` → `(from, {notify, Name})` with `subject` interned as `system`; the caller resolves the recipient, so the same builder serves any delivery path.

## Constraints
- Crate is `#![deny(unsafe_code)]`: the registry slots operate over a **thread-local** entry list
  `DRIVER_ENTRIES` (`642`), not a process-global `DriverTable`. The runtime drives every port on one thread.
- Every driver callback runs under `publishing` → `beam_abi::with_current_host` (`715`), used at
  `start` (`1485`), `ready_*` (`1337`), `output` (`1554`), `timeout` (`1696`) and `stop` (`1737`) — this is
  what lets the two port-less registry slots reach a host during a callback.
- The `SignalFacility` is a `thread_local` (not a `Runtime` field) for the same reason `context::trace`'s slot is: the runtime is single-threaded (`!Send`), so the slot *is* the process's one facility. A **second** live facility would re-point `beam_io`'s trampoline write end at its own pipe; the one `FacilitySlot` the slot holds — created lazily by the first `os:set_signal/2` and **never dropped** (production never drops the runtime's facility, and a test build `mem::forget`s it in `Drop` so a libtest thread teardown cannot restore the process-global dispositions under a live signal test) — is what prevents that.
- Never a fabricated success: failures are typed `BeamError`s, and a state/lifecycle it cannot serve
  returns the C's own failure code.
- The `Rc<RefCell<..>>` handles are the ETS pattern so a port operation never needs a runtime borrow a
  `fun` argument already holds — a nested borrow is a typed gap, never a panic.

## Known Issues (this is a floor, not the full C `io.c`)
- **`erlang:port_control/3` is live end to end**: the native floor (`../native/port.rs`'s `port_control`,
  dispatched from `../native.rs`'s `try_native` — it is *not* a `PortHook` method and *not* in `../error.rs`'s
  `KNOWN_UNIMPLEMENTED_ERLANG_BIFS`) validates `Port`/`Operation`/`Data` (`term_to_Uint` + `erts_iolist_to_buf`,
  `erl_bif_port.c:322-360`) and forwards the verb to `PortRegistry::port_control`, which reaches the driver as
  the decimal `c_uint` string via `DriverPortAdapter::control`. The callback reproduces the C's codes exactly —
  an **absent** `control` slot and a **negative** `cres` are both `badarg` (`io.c:4035`/`4074`) — and the reply
  bytes come from the buffer the driver was handed. An **internal** port
  (`{spawn,_}`/`{spawn_executable,_}`/`{fd,_}`) is `badarg` for **every** verb, the C's own
  `if (!prt->drv_ptr->control) return ERTS_PORT_OP_BADARG;` (`io.c:4035`); a `{spawn_driver, Name}`
  port — a loaded `.so` **or** a built-in driver such as `tcp_inet` — has a `control` callback, so the
  verb reaches the driver.
  **Reply encoding** (`write_port_control_result`, `io.c:4098-4175`): the bytes render as a **list of byte
  integers** at every length — there is no size threshold — and as a **binary** only when the driver set
  `PORT_CONTROL_FLAG_BINARY` through `set_port_control_flags`, a per-port flag that **persists** across calls;
  a `NULL` reply pointer is the C's `NIL` (the bare `[]`). The one residual gap is a reply the driver answers
  from **its own** memory (a pointer addressing neither the reply buffer nor the request buffer), which is the
  typed *unsupported* — reading it needs the raw access `beam_rt`'s `#![deny(unsafe_code)]` forbids — never a
  fabricated or truncated reply; closing it needs a safe copy helper on `beam_abi`'s `DriverLibrary` (see
  Handoffs).
- **A NULL driver reply with `PORT_CONTROL_FLAG_BINARY` set answers `<<>>` where the C answers `[]`**:
  `write_port_control_result` (`io.c:4126`) returns `NIL` for a NULL reply pointer **before** it looks at
  the flag, so the oracle answers the bare `[]`; here `DriverPortAdapter::control` collapses "the driver
  left `*rbuf` NULL" and "the driver answered zero bytes" into one empty `Vec`, which
  `PortRegistry::control_binary` then renders as `<<>>`. Closing it needs the "pointer was NULL" bit
  threaded from `DriverPortAdapter::control` to `PortRegistry::port_control` (a shared
  `Rc<Cell<bool>>` beside `driver_control_flags`, `../port.rs`), because the `beam_io::Driver::control ->
  Result<Vec<u8>>` seam carries no such bit.
- **`driver_async` schedules nothing**: `DriverHost::r#async` keeps the frozen default `-1`. The async callbacks are `Option<unsafe extern "C" fn(*mut c_void)>`, which `beam_rt` (`#![deny(unsafe_code)]`) cannot call; `beam_abi` exposes **no safe runner** for the `invoke`/`free` pair (`DriverLibrary::ready_async(data, tdata)` *is* safely callable, and `beam_io`'s `Driver::drive_async` exists). The C's in-band behaviour when the async pool is empty (`erl_async.c:604-613`: `(*invoke)(data)`, then `async_ready` → the entry's `ready_async`, then `free(data)` if a `ready_async` slot exists) is the reference. Returning a handle without running the job would be fabricated success, so the honest `-1` stands (see Handoffs — the `beam_abi` async-runner item).
- A queued `TermQueue` entry carries no sending-port identity (the C stamps `from = prt->common.id`).
- **Driver-interned atoms land at the port boundary, not inline**: a name first interned *inside* a driver callback (`erl_drv_output_term`/`binary_to_term`/`intern_atom` — all sharing this path) gets a **predicted** index, because the live table is borrowed for the whole callback and this crate is `#![deny(unsafe_code)]`. `PortRegistry::commit_driver_atoms` — driven from `RuntimePorts::deliver_port_messages`, committing `code_server.atoms_mut()` — lands it at exactly that index; a prediction the table has moved past is **reported** (`dropped` and printed), never interned at a foreign index (which would make the driver's already-built term name a different atom).
- The exact success code of the four term-emitting calls differs: the C returns `1` for a well-formed spec, this runtime's `send_term` returns `0` (the **accept/refuse split** agrees and is pinned by the `driver_abi` corpus).
- `take_exit_status` is always `Ok(None)` — a driver port owns no child, so no `{Port,{exit_status,…}}`.
- **The monitor family is live** (`monitor_process`/`demonitor_process`/`get_monitored_process`): `DriverPort` keeps the per-port monitor tree (`driver/monitor.rs`'s `DriverMonitors`, keyed on the C's 24-byte reference identity, read/written only through the safe `beam_abi::monitor_identity`/`monitor_from_identity`), and `DriverPortAdapter` fires the entry's C `process_exit` for each marked monitor at its per-round drive point (`has_pending_async`/`drive_async` over `beam_io`'s existing probe, `beam_io/src/registry.rs:291-317`), the tree cleared in `stop` and at the top of `Drop` so no callback can run after the library unloads.
  The C's `1` ("not alive") is **not expressible**: the frozen hook returns `Option` and `beam_abi`'s wrapper maps `None` → `ERL_DRV_MONITOR_ERROR` (`beam_abi/src/driver/drv/pdl.rs:405-412`), so a pid that is not a local process answers `-1` where the oracle answers `1` (a local pid still answers `0`). A local pid's liveness is not probed at all — the host holds only its owning `ProcessId`, never the `ProcessTable`. Handoff: a 3-way hook result on `beam_abi`.
- **Nothing marks the tree yet.** The runtime does not call `DriverMonitors::note_process_exit`: the marking needs `PortMeta.driver_monitors` (seized at `open_port/2` beside `PortMeta.driver_terms`, in `../port.rs`) plus a scheduler-pass drain holding the live `ProcessTable` (the `drain_ets_owner_exits`/`drain_port_monitor_exits` pattern — an exit observer may only **record**). Until then a `process_exit` fires only for a directly marked monitor, one port-task round after the C's synchronous exit path.
- **`PORT_FLAG_LINE` (2) is unreachable**: `[line]`/`{line,N}` is whitelisted-but-inert in `open_port/2` (`../port.rs`), so no stream resolves to `StreamType`'s line form, and a `[line]` driver port answers `0` where the oracle answers `2` (oracle-verified: a C driver's `get_port_flags` from a `control` callback answers `2` for a `[line]` port, `1` for `[binary]`, `0` with no stream option).
- **`get_port_flags` is answerable from inside `start` where the oracle answers `0`.** The oracle ORs `ERTS_PORT_SFLG_BINARY_IO`/`_LINEBUF_IO` into `port->state` only **after** `open_driver` returns (`erl_bif_port.c:1139-1143`), so a driver calling `get_port_flags` in its own `start` sees `0` for every stream option (oracle-verified for `[binary]`/`[line]`/none; visible only from a *later* callback, e.g. `control`). `DriverPort::with_port_flags` sets the bits at host construction (`../port.rs:1651-1659`), **before** the adapter's `start` runs, so `beam_rt` answers the stream's real flags there. No corpus case calls `get_port_flags` from `start`, so the gate does not see it; making the bits take effect after `start` would be a behaviour change needing its own A.4 case.
- **`driver_async_port_key` stays unimplemented** (`async_port_key` keeps the trait default `-1`): the key is an internal PTab data word, unpinnable from the seam (a fabricated key would be a wrong one). `driver_async/5` is likewise untouched — a sibling owns the safe async runner (Handoffs).
- A driver that armed **more than one** registration per direction is a typed `unsupported`
  (`armed_event`, `1264`), never a wrong descriptor.
- `os:set_signal/2`'s notification is **dropped when no `erl_signal_server` is registered** — the C's own behaviour (`signal_notify_requested`, `sys.c:589-604` sends nothing when the process is absent), never an error. The `-eval` receive path (`poll_port_messages`) can only observe its own mailbox, so it drops a `PortEvent::Signal`; the scheduler path (`deliver_port_messages`) is the one that delivers it.

## Notes for Agents
- The `erlang:port_control/3` BIF arm is the **native floor** (`../native/port.rs`'s `port_control`, dispatched from `../native.rs`'s `try_native`), which calls `PortRegistry::port_control` directly; `RuntimePorts::port_control` is retained-but-uncalled (see the API Surface note).- The colocated signal tests share **one** `watch::test_guard()` lock so they serialize on the process-global signal dispositions.
- Signal tests send with `Command::new("kill") -USR1 <pid>` on `std::process::id()` — **never** `libc::raise`, which would need `unsafe`. A test that calls `run_scheduled` must account for the port loop parking (the scheduler's quiescence predicate — `Runtime::is_nt` reads `PortRegistry::has_nothing_to_poll`; see `../runtime/CONTEXT.md`).

## Test Strategy
- Colocated units in `tests.rs` (the `#[cfg(test)] mod tests;` child of `../port.rs` — a separate file
  so the suite does not grow the provider past its production size, as with `driver/*_tests.rs`): every
  `Name` form + option (`{fd,N}` over a socket pair), an empty
  name → `BadArg` with no port number consumed, the **built-in-driver** `{spawn_driver,Name}` open
  (`a_builtin_driver_name_opens_a_live_driver_port` — `tcp_inet` opens a live port whose `name` is
  `tcp_inet`, no OS child, a **driver** port for `port_control` but not for `take_driver_outputs`, and
  `port_close/1` succeeds; oracle-cross-checked) and its unknown-name counterpart
  (`an_unknown_spawn_driver_name_is_still_badarg`), the `port_info` vocabulary, `pump`'s once-only
  `{exit_status,…}`, the `next_deadline` clamp, `take_driver_outputs`/`take_driver_terms`, the
  `TermQueue` drain, `stabilize_driver_term`, and the signal delivery end to end
  (`a_handled_signal_is_delivered_to_the_registered_signal_server`, the no-server drop, a second signal,
  `ignore`/`default` leaving nothing pollable, and a zero-port runtime still polling a registered watch).
- The **read path** is pinned colocated too, over a real child and the real pump the scheduler pass
  drives: `a_binary_stream_message_is_a_binary_of_every_width` (`port_data_message`'s two stream shapes
  plus the interned `data` atom), `a_wide_binary_stream_read_is_delivered_at_full_width` (a 200-byte child
  write arrives in full, oracle-cross-checked on the width) and
  `the_stderr_to_stdout_option_merges_the_child_stderr_into_the_port_stream` (`out\nerr\n` with the option,
  `out\n` without it — the same child, only the option list differs). They share `pump_events_until_exit`
  (one real `PortRegistry::pump` per round, returning the target port's chunks up to its `Exited`), its
  `pump_until_exit` and `Fixture::args_option` helpers; the helper requires the target port to be the
  fixture's **only live port** (a round drains every live port and `beam_io::PortRegistry::take_exit_status`
  hands a status out once, so a foreign event would be lost) and fails loudly rather than starving the
  caller. The oracle cross-checks run through `erl_eval`, which prints a **loud** skip banner when no
  usable `erl` is on `PATH`.
- Colocated units in `driver.rs`: the registry-slot codes, the thread-local entry list, the
  `armed_event` rejection, and `binary_to_term` (a decoded image, an invalid one → `None`).
- The A.4 **driver-private state suite** `driver/hooks_tests.rs` pins `DriverPort`'s `DriverHost`
  hooks for the driver-private queue (`driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/
  `driver_peekq`), the one-PDL rule (`driver_pdl_create`) and the port state flags
  (`driver_get_port_flags`) — pinned at the **seam** level, because the C entry points themselves are
  not nameable from `beam_rt` (`drv` re-exports only `mem`/`port`; the `queues`/`pdl`/`lifecycle`
  families are private modules and every entry point is `unsafe`). Cases: a fresh port's `sizeq` is
  `0` (not `NOT_A_SIZE`), `enq`-tail vs `pushq`-head order via different-length chunks (so the iovec
  lengths discriminate without dereferencing `iov_base`), the byte count, `deq` from the head plus its
  whole-queue refusal (queue untouched) and its negative-`c_int` boundary, `peekq`'s one entry per
  enqueue + per-chunk `iov_base` identity + `None` when empty, the empty-enqueue no-op,
  `clear_driver_state`, the non-null/one-per-port/last call and two-distinct-ports PDL contract, and
  that `get_port_flags` reads the port **state** bit and **not** the `control_flags` cell (driven in
  both directions). `beam_abi`'s own suites pin each wrapper's translation *onto* this seam
  (`queues.rs`'s `NOT_A_SIZE`, `pdl.rs`'s `NULL`, `tests/driver_queues.rs`'s whole C queue surface).
- The A.4 **write-path regression suite** `driver/ready_output_tests.rs` (`a_queued_port_command_reaches_output_with_a_writable_descriptor_armed`
  and `ready_output_delivers_the_payload_before_its_notification`) compiles a real C driver `.so` at test time
  (`RT_READY_OUTPUT_MODE` 0/1: mode 0 leaves `ready_output` `NULL`, mode 1 declares it and appends a `"W"`
  marker) and drives the real `port_command` → `OUTPUT`-task path. It discriminates on exactly one property:
  with a writable descriptor armed (`driver_select(…, ERL_DRV_WRITE, 1)` in `start`), a queued `port_command`
  byte still reaches the driver's `output` callback (the echo lands on the port's input queue) — a
  notification-only `ready_output` returns `Ok(0)` and writes nothing, leaving `drain_input()` empty. Mode 1
  additionally pins the payload-before-notification order (`pingW`). Because a `--lib` unit-test link does not
  export the dynamic symbol table (`cargo`'s `rustc-link-arg-tests` covers only `tests/` targets), the fixture
  imports no `driver_*` symbol and is handed `driver_select`/`driver_output` as two `%lx` addresses in the
  loader's `command` string, which its `start` calls through; the test skips **loudly** without `cc` or
  `erl_driver.h`.
- The two sibling end-to-end driver suites share that fixture pattern (a real C driver `.so` compiled at
  test time from an `RT_*` env-var-selected source, a **loud** `SKIP:` banner without `cc`/`erl_driver.h`,
  and the same `--lib`-link constraint: the fixture imports no `driver_*` symbol and receives the host
  functions it needs — plus a **marker file** path — in the loader's `command` string).
  `driver/control_tests.rs` pins the port-control matrix of `DriverPortAdapter::control`
  (`call_driver_control`, `io.c:4024-4078`) over the real `PortRegistry::port_control` entry point: the
  reply verbs `RT_CTL_REVERSE`/`FIXED`/`NIL`/`FOREIGN`/`NEGATIVE`, the absent-slot `badarg`
  (`RT_CTL_HAS_CONTROL=0`), and the `C:<cmd>:<len>` marker proving the verb arrived as the **parsed
  integer** with the request's real length. `driver/stop_select_tests.rs` pins the
  `driver_select(port, event, ERL_DRV_USE, 0)` → `stop_select` handback (`erl_check_io.c:1177-1193`,
  `:1212-1216`), discriminating the `ERL_DRV_USE_NO_CALLBACK` cancel bit from a plain direction deselect;
  a `stop_select` callback is handed an `ErlDrvEvent` and **no** `ErlDrvData`, so its marker file records
  both the armed descriptor (`A:<fd>`) and the handed-back event (`S:<event>`).
- Colocated units in `watch.rs`: nothing-to-poll with no disposition, the self-pipe registered once, and
  `ready`/`drain` over a really-sent signal; plus `#[cfg(test)]` `test_guard`/`pending`/`send_to_self`/
  `wait_until_pending` helpers shared by the three signal test modules. Every watch assertion about a
  signal that was really sent (**not** one asserting nothing has arrived) first waits, bounded, for the
  trampoline to *catch* it — `kill(1)` returns as soon as the kernel has queued the signal while the
  handler that writes the self-pipe runs asynchronously, so `drain`/`ready` immediately after
  `send_to_self` would be a race.
- Integration: `../tests/driver.rs` (a real C driver via `event_driver.c`, modes 0–3 — the event/timer
  round, the `outputv`-only entry, and the two driver-emitted-term destinations — plus `reg.c` for the
  registry hooks and `../tests/port_wake.rs` for the resume-on-send wake) and the differential `driver/*`
  / `driver_c/*` / `driver_abi/*` cases.

## Routing Table
- `driver.rs` → the A.4 host + `beam_io::Driver` lifecycle above.
- `driver/state.rs` → the per-port driver-private state: the driver's private byte queue (`prt->ioq`) as an ordered list of enqueue chunks (hook-level units colocated).
- `driver/hooks_tests.rs` → the A.4 queue / one-PDL / port-flags hook regression suite (seam level).
- `driver/ready_output_tests.rs` → the A.4 write-path regression suite (a real C driver `.so` compiled at test time).
- `driver/control_tests.rs` → the A.4 port-control regression suite (the reply-verb matrix + the absent slot).
- `driver/stop_select_tests.rs` → the A.4 `ERL_DRV_USE`/`stop_select` handback regression suite.
- `watch.rs` → the `os:set_signal/2` delivery half (facility + `SignalWatch` + the `{notify,Signal}` name builder).
- `../port.rs` → the A.4 `PortHook` provider, the poller/`PortRegistry` (its `builtin_drivers` `beam_io::DriverRegistry` and the `{spawn_driver, Name}` built-in fallback included), the port-task round and the message builders.
- `tests.rs` → that provider's unit suite (`open_port/2`'s decode — incl. the built-in-`{spawn_driver,Name}` open and the unknown-name `badarg` — the `port_info` vocabulary, the port-message pump, the `next_deadline` clamp and the read path).
- `../native/port.rs` → the native `port_command/2,3` + `port_control/3` + `port_info` closed-port/`registered_name` floor (the `/3` arity's list-vs-binary reply rule included).
- `../context/signal.rs` → the `os:set_signal/2` answer surface (delegates the facility to `watch.rs`).
- `../context/` → installs `RuntimePorts` as the `PortHook` on every registered-BIF arm.
- `../runtime/CONTEXT.md` → the scheduler pass that pumps the registry.
- C fixtures + end-to-end A.4 tests → `../../tests/{driver.rs,port_wake.rs,port_parity.rs}` and
  `../../tests/fixtures/{event_driver.c,event_driver.erl,reg.c,README.md}`.

## Known divergence from the oracle: no child-exit auto-close
- ERTS **auto-closes** a `{spawn, Program}` port once its child exits: after the child is reaped, `erlang:port_info(P)` is `undefined`, `lists:member(P, erlang:ports())` is `false`, and — crucially — `erlang:port_close(P)` then raises `error:badarg` (measured on the erts-17.1 oracle: `open_port({spawn,"true"},[]), timer:sleep(200), port_close(P)` ⇒ `error:badarg`; the same at a `true`-child exit of a few ms).
- This runtime does **not** auto-close on child exit. `PortMeta.notify_exit` is only set by the `exit_status` option, so nothing reaps/removes the port until an explicit `port_close` (or registry drop): `PortRegistry::close` (`../port.rs`) kills+reaps, `meta.remove`s and releases the id, and `port_close` is `badarg` iff `!is_live` — the whole close is **synchronous**, never deferred to a scheduler pass, so `port_close`→`port_info` cannot be order/timing-sensitive here.
- Consequence for the compatibility gate: the `driver/*_after_close*` cases (`port_info_after_close_is_undefined`, `ports_excludes_a_closed_port`, `port_info2_after_close_is_undefined`) assume an explicit close **succeeds**, and the `driver/port_close_through_spawn_driver` golden assumes the same. If a `{spawn,"true"}` child is reaped before `port_close` runs, the **oracle** raises `badarg` (its own `port_close`) and drifts from its own golden → reported as a *mismatch*; the sibling `port_command_after_close_is_badarg` case has a `badarg` golden and therefore still agrees, so exactly the three "expect a successful close" cases diverge. The symmetric Rust-side shape would additionally require `open_port` to raise while `port_command_after_close_is_badarg` still agreed, which it cannot.

## Handoffs (owned by OTHER nodes)
- **`beam_abi`** — a **three-way** monitor result (e.g. `monitor_process(&mut self, pid: Eterm) -> MonitorResult` distinguishing success / not-alive / error). The frozen `Option` hook cannot express the C's `1` (`ERL_DRV_MONITOR_NOT_ALIVE`): its wrapper maps `None` → `ERL_DRV_MONITOR_ERROR` (`beam_abi/src/driver/drv/pdl.rs:405-412`), so a pid that is not a live local process answers `-1` where the oracle answers `1`. The reader/writer the family needed (`monitor_identity`/`monitor_from_identity`) exist and are in use.
- **`beam_abi`** — `DriverHost::r#async` needs a **safe** runner for the async `invoke`/`free` pair (e.g. `DriverLibrary::invoke_async(invoke, data)` + a `free` counterpart); `ready_async` is already safely callable, so only those two are missing.
- **`beam_abi` / `beam_io`** — a **safe reply-copy** helper for a `control` reply the driver answered from memory of its own, so the one remaining driver-`control` gap can close. The copy already exists as an `unsafe` slice read inside `impl Driver for DriverPort` (`beam_io/src/abi/port.rs:710-747`; after `control` returns `cres ≥ 0` and `rbuf` non-null, `core::slice::from_raw_parts(rbuf, n)` at `:745`), but `beam_rt` never names `beam_io::abi` and is `#![deny(unsafe_code)]`, and the frozen `beam_abi::DriverLibrary::control` (`driver/loader.rs:288`) reports only the `cres` and the re-pointed `*mut c_char` — dereferencing it is the caller's `unsafe`. The helper belongs on `beam_abi`'s `DriverLibrary` (e.g. a `control_reply` that returns the copied bytes) or on `beam_io`'s public wrapper, never as a re-derived `unsafe` block in `port/driver.rs`; until it exists `DriverPortAdapter::control_reply` answers the typed *unsupported*.
- **`beam_io` — closed, no seam needed.** Firing the driver's `process_exit` does not need a new `beam_io::Driver` method: `PortRegistry::execute_port_tasks` already probes `Driver::has_pending_async` and calls `Driver::drive_async` for every live port each round (`beam_io/src/registry.rs:291-317`, `beam_io/src/port.rs:548/562`), and `DriverPortAdapter` overrides both — `has_pending_async` reports the monitor tree, `drive_async` fires every pending monitor's `process_exit` and returns the count. `Port::notify_exit`/`Driver::port_exit` stay the *port's own* exit channel (once-only, `PortExitReason` carries no monitor), unrelated to a monitored process's death.
- **`beam_process`'s exit observer is a single slot** (`exit_observer: Option<Box<dyn ProcessExitObserver>>`, `beam_process/src/ptab.rs:349`; `set_process_exit_observer` overwrites, `:2044`), and `beam_rt` already installs exactly one (`EtsExitObserver`, `src/runtime.rs:568`). A driver-monitor observer must therefore **compose into `EtsExitObserver`** (or share its `OwnerIndex`-style cell) rather than call `set_process_exit_observer` a second time — a second install would silently drop the ETS/owner-death obligation. The observer runs inside the finalizer (`terminate_into`, `ptab.rs:1593`) while the dying heap is still live, so it may **only record** (borrow `self.runtime.ports` is unsafe there); the driver call must be deferred to a scheduler-pass boundary where the port registry is free — the `drain_ets_owner_exits`/`drain_port_monitor_exits` pattern (`src/runtime/scheduler.rs:573/580`).
