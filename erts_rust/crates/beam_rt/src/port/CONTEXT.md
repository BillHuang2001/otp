# `beam_rt::port` — the runtime's A.4 port layer (`../port.rs` provider + `driver.rs` host)

## Intent
The runtime side of contract A.4, in two files.
`../port.rs` is the **port provider**: `open_port/2`'s `Name`/`Options` decoding, the poller/registry (`PortRegistry`), the port-task round and the `{Port,{data,..}}`/`{Port,{exit_status,..}}` message pump behind `beam_io`.
`driver.rs` is the runtime's **own** driver C-ABI host for `{spawn_driver, Name}` ports (contract A.4), written directly over `beam_abi`'s frozen `DriverHost` seam.
It is deliberately **separate** from the sibling crate `beam_io`'s own host layer (`beam_io::abi`): `beam_rt` never references `beam_io::abi`, `make_loaded`, `AbiHost` or `DriverTable` (verified: no matches anywhere in the crate), so end-to-end `.so`-driver traffic goes through this file, not `beam_io`'s.
The native `port_command`/`port_info` floor lives in `../native/port.rs`.

## API Surface
### `port.rs` — the `PortHook` provider (A.4)
Public: `PORT_INFO_ATOMS` (incl. `registered_name`), `PortName{Spawn,SpawnExecutable,Fd,SpawnDriver}`, `StreamType{List,Binary}` (list default), and `RuntimePorts` (the `beam_bifs::PortHook` impl: `open_port/2`, `port_close/1`, `port_info/1,2`) built from `Rc<RefCell<PortRegistry>>` + the shared `Rc<AtomTable>` + the owning `ProcessId` (the `Rc<RefCell<..>>` is the ETS pattern — `FunBridge` needs `&mut Runtime` at the same time; a nested borrow is a typed gap, never a panic).
`PortRegistry` (private `{poller, registry, meta, heap}`): `new()` (anchors the `driver_*` codegen unit), the registry queries/commands (`is_live`/`contains`, `write_command`, `len`/`is_empty` = `system_info(port_count)`, `ids()` = `erlang:ports/0`, `close`, `driver_lookup`), and the private pass (`register`/`pump(timeout) -> Vec<PortEvent>`/`take_due_exit_statuses`/`take_driver_outputs`/`info`); consumers are `Runtime::deliver_port_messages` + the `-eval` trio `poll_port_messages`/`port_data_message`/`port_exit_message`.
`SpawnDriver` is the **single** OS-child implementation (this crate owns only `open_port/2`'s decoding into a term-free `SpawnSpec`/`SpawnOptions`, plus `PortMeta`). `pump` polls in `PORT_POLL_SLICE` (25 ms) slices clamped by the earliest driver deadline, runs the port tasks, harvests each driver's `driver_output`, and reports `PortEvent::{Data,Exited}`; `deliver_port_messages` delivers each `{Port,{data,..}}`/`{Port,{exit_status,..}}` into the **owning** process's mailbox, and `close` calls `beam_io`'s `notify_port_exit(…, Closed)` **before** `registry.close(...)`.

### `port/driver.rs` — the driver C ABI host (A.4)
`Driver{library, host: Box<DriverPort>}` pairs a `dlopen`ed driver with the host its callbacks run against (**field order is load-bearing**: the library's `Drop` runs the entry's `finish` before the host drops). `DriverPort` is the concrete `beam_abi::DriverHost` behind the opaque `ErlDrvPort_`: the handle **is** the per-port `DriverEnv`'s address (a driver-private term arena, the shared `AtomTable` + a `PendingInterner`, the owning `ProcessId`, and the output/pid services). `load_driver(candidates, name)` tries each candidate (the loader appends `.so`) and reports the **last** typed error. The `DriverHost` registry hooks carry the C's exact codes: `add_driver_entry`/`remove_driver_entry` (`add` returns `0`; `remove` `1`/`0`/`-1` per `io.c:7698`), `lock_driver` keyed off the port's load state (`io.c:7324`) and `create_port` = the invalid-port sentinel.
`DriverPortAdapter` is the **port lifecycle** the host alone lacks — a `beam_io::Driver` over a loaded C driver: `start` runs the entry's `start` (the `ErlDrvPort` **is** the environment's address, kept in a `Box` that never moves), `output` hands the port's queued bytes to the entry's `outputv` when it declares one (**the C's preference**, `io.c:1782`/`io.c:1597`) and to `output` otherwise (`ErlIOVec` = an empty writable header `iov[0]` + the data `iov[1]`, parallel all-NULL `binv`, built on the caller's stack exactly as `io.c:1806/1842-1864`), queues the `driver_output` reply as the port's **input** (surfaced by `take_driver_outputs` as `{Port,{data,Data}}`), `stop` runs the entry's `stop`, and the drop runs `finish` + `dlclose`. `build.rs`'s `-Wl,--export-dynamic` publishes the `driver_*`/`erl_drv_*` symbols; `open_port({spawn_driver, Name}, Options)` routes here, and an unknown or empty driver name is `badarg`.

### Driver-emitted terms (`erl_drv_output_term` / `erl_drv_send_term`) — `TermQueue`
`beam_abi` decodes the driver's term-data stack machine into a real term on the host's arena and calls `DriverHost::send_term(port, to: Option<Eterm>, term)`. The C *delivers* there and then (`driver_deliver_term`, `io.c:5664`: the decoded term itself, `from = prt->common.id`); the frozen seam gives the host no route to a receiver's mailbox, so the term is **queued**: `TermQueue` (`pub struct`, `Rc<RefCell<Vec<(Option<Eterm>, Eterm)>>>`) is per port and shared by handle — `DriverPort::term_queue()` returns the handle, `send_term` pushes onto it (while still recording the accumulate-only `DriverPort::sent` view the A.4 test reads), `take()` drains, `is_empty()` reports. `None` = the port's connected process (`erl_drv_output_term`, `io.c:6254`), `Some(pid)` = an explicit destination (`erl_drv_send_term`, `io.c:6289`). **Lifetime invariant:** a queued term's words live on the host arena, so `DriverPortAdapter` **empties** the queue both in `beam_io::Driver::stop` (the C refuses a delivery while `ERTS_PORT_SFLG_CLOSING`, `deliver_term_check_port`, `io.c:6213`) and in its `Drop`; a late drain observes an empty queue, never a freed arena.
**The drain is wired**: `PortRegistry` seizes the handle as `meta.driver_terms` at `open_port/2` and `take_driver_terms` (`../port.rs`) surfaces each queued term as `PortEvent::Term{to, term, owner}`, which the pump copies onto the port heap (`RuntimeContext::stabilize_driver_term`) and delivers to the destination — the connected process for `None`, the explicit pid for `Some`.

The precise symbols of `driver.rs`:
- `DriverPort` (`driver.rs:235`) — `impl beam_abi::DriverHost` (`driver.rs:445`); the concrete host behind
  the opaque `*mut ErlDrvPort_` a driver callback receives. The port handle **is** the per-port
  `DriverEnv`'s address.
- `Driver { library, host: Box<DriverPort> }` (`193`) — field order is load-bearing (`library` drops first).
- `DriverPortAdapter` (`669`) — `impl beam_io::Driver` over a loaded C driver: `load`/`name`/`state`/`selected`,
  plus the callback mapping `start`/`output`/`ready_input`/`ready_output`/`timeout`/`stop`.
- `load_driver(candidates, name)` (`623`) — `dlopen` the driver object, reporting the last typed error.
- The timer family matches the C for **every** live port: the C substitutes a silent `no_timeout_callback`
when the entry declares no `timeout` slot (`io.c:7601`), so `io.c:6918`'s `-1` stands only for an invalid
port (`DriverControl::attached == false`); `driver_set_timer` arms the deadline and answers `0`, and a
fired deadline runs the entry's `timeout` or, when absent, is the C's silent no-op.
The four driver-registry slots are **all overridden** (not left at no-host defaults):
  `add_driver_entry` (`565`, → `registry_add_entry`, `0`/`1`), `remove_driver_entry` (`571`, →
  `registry_remove_entry`, C's `1`/`0`/`-1`), `lock_driver` (`584`, `0` iff `control.driver_loaded`
  else `-1`), `create_port` (`601`, always `ERTS_INVALID_ERL_DRV_PORT`).

## Constraints
- Crate is `#![deny(unsafe_code)]`: the registry slots operate over a **thread-local** entry list
  `DRIVER_ENTRIES` (`358`), not a process-global `DriverTable`. The runtime drives every port on one thread.
- Every driver callback runs under `publishing` → `beam_abi::with_current_host` (`440`), used at
  `start` (`994`), `ready_*` (`902`), `output` (`1036`), `timeout` (`1088`) and `stop` (`1129`) — this is
  what lets the two port-less registry slots reach a host during a callback.
- Never a fabricated success: failures are typed `BeamError`s, and a state/lifecycle it cannot serve
  returns the C's own failure code.
- The `Rc<RefCell<..>>` handles are the ETS pattern so a port operation never needs a runtime borrow a
  `fun` argument already holds — a nested borrow is a typed gap, never a panic.

## Known Issues (this is a floor, not the full C `io.c`)
- The driver `control` callback is **not** routed: `DriverPortAdapter` has no `control` override, so
  `beam_io::Driver::control`'s no-reply default is used, and `erlang:port_control/3` raises
  `unsupported: erlang:port_control/3` (registered in `../error.rs`'s `KNOWN_UNIMPLEMENTED_ERLANG_BIFS`).
- `driver_async` keeps the `DriverHost` default `-1` (no async pool; `Runtime` is `!Send`).
- A queued `TermQueue` entry carries no sending-port identity (the C stamps `from = prt->common.id`).
- **A driver-emitted term is delivered end to end**: the handle is seized at `open_port/2`
  (`meta.driver_terms`), `PortRegistry::take_driver_terms` drains it as `PortEvent::Term`, and the
  pump stabilizes it onto the port heap (`RuntimeContext::stabilize_driver_term`) and delivers it to
  the destination (`None` = the port's connected process, `Some(pid)` = an explicit one).
- `ERL_DRV_USE` is accepted but its `stop_select` callback never fires.
- `take_exit_status` is always `Ok(None)` — a driver port owns no child, so no `{Port,{exit_status,…}}`.
- A driver that armed **more than one** registration per direction is a typed `unsupported`
  (`armed_event`, `875`), never a wrong descriptor.
- **The `stderr_to_stdout` and `eof` spawn-stream options are accepted but inert here.** Both are on
  the whitelist (`PORT_OPTION_ATOMS`, `../port.rs:221`) and fall into the recognized-without-effect
  `_ => {}` arm (`../port.rs:1250`); `SpawnOptions` is built (`../port.rs:1357-1363`) with no
  `stderr_to_stdout`/`eof` field and `SpawnDriver::with_stream_options` is **never** called from this
  crate, so `beam_io`'s `StreamOptions`/`take_eof` (implemented + tested in `beam_io`) are unreachable.
  A child's stderr is `Stdio::inherit` by default, matching the oracle's `dup(2)`.

## Test Strategy
- Colocated units in `../port.rs`: every `Name` form + option (`{fd,N}` over a socket pair), an empty
  name → `BadArg` with no port number consumed, the `port_info` vocabulary, `pump`'s once-only
  `{exit_status,…}`, the `next_deadline` clamp and `take_driver_outputs`.
- Colocated units in `driver.rs`: the registry-slot codes, the thread-local entry list, and the
  `armed_event` rejection.
- Integration: `../tests/driver.rs` (a real C driver via `event_driver.c`) + `../tests/port_wake.rs`
  (the owner is woken by a port message through `beam_process`'s resume-on-send) + the differential
  `driver/*` and `port_info`/`port_command` cases.

## Routing Table
- `driver.rs` → the A.4 host + `beam_io::Driver` lifecycle above.
- `../port.rs` → the A.4 `PortHook` provider, the poller/`PortRegistry`, the port-task round and the message builders.
- `../native/port.rs` → the native `port_command/2,3` + `port_info` closed-port/`registered_name` floor.
- `../context/` → installs `RuntimePorts` as the `PortHook` on every registered-BIF arm.
- `../runtime/CONTEXT.md` → the scheduler pass that pumps the registry.
- C fixtures + end-to-end A.4 tests → `../../tests/{driver.rs,port_wake.rs,port_parity.rs}` and
  `../../tests/fixtures/{event_driver.c,event_driver.erl,README.md}`.
