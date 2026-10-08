# `beam_rt::port::driver` — the runtime's OWN A.4 driver host

## Intent
`driver.rs` is the runtime's **own** driver C-ABI host for `{spawn_driver, Name}` ports (contract A.4),
written directly over `beam_abi`'s frozen `DriverHost` seam.
It is deliberately **separate** from the sibling crate `beam_io`'s own host layer (`beam_io::abi`):
`beam_rt` never references `beam_io::abi`, `make_loaded`, `AbiHost` or `DriverTable` (verified: no
matches anywhere in the crate), so end-to-end `.so`-driver traffic goes through this file, not
`beam_io`'s.
The port *provider* (open_port decode, poller/registry, message pump, `port_info`) lives one level up in
`../port.rs`; the native `port_command`/`port_info` floor lives in `../native/port.rs`.

## API Surface
- `DriverPort` (`driver.rs:235`) — `impl beam_abi::DriverHost` (`driver.rs:445`); the concrete host behind
  the opaque `*mut ErlDrvPort_` a driver callback receives. The port handle **is** the per-port
  `DriverEnv`'s address.
- `Driver { library, host: Box<DriverPort> }` (`193`) — field order is load-bearing (`library` drops first).
- `DriverPortAdapter` (`669`) — `impl beam_io::Driver` over a loaded C driver: `load`/`name`/`state`/`selected`,
  plus the callback mapping `start`/`output`/`ready_input`/`ready_output`/`timeout`/`stop`.
- `load_driver(candidates, name)` (`623`) — `dlopen` the driver object, reporting the last typed error.
- The four driver-registry slots are **all overridden** (not left at no-host defaults):
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

## Known Issues (this is a floor, not the full C `io.c`)
- The driver `control` callback is **not** routed: `DriverPortAdapter` has no `control` override, so
  `beam_io::Driver::control`'s no-reply default is used, and `erlang:port_control/3` raises
  `unsupported: erlang:port_control/3` (registered in `../error.rs`'s `KNOWN_UNIMPLEMENTED_ERLANG_BIFS`).
- `driver_async` keeps the `DriverHost` default `-1` (no async pool; `Runtime` is `!Send`).
- `driver_send_term`'s term is captured in `DriverPort::sent`, never delivered.
- `ERL_DRV_USE` is accepted but its `stop_select` callback never fires.
- `take_exit_status` is always `Ok(None)` — a driver port owns no child, so no `{Port,{exit_status,…}}`.
- A driver that armed **more than one** registration per direction is a typed `unsupported`
  (`armed_event`, `875`), never a wrong descriptor.

## Routing Table
- `driver.rs` → the A.4 host + `beam_io::Driver` lifecycle above.
- Port provider / poller / `PortRegistry` / message builders → `../port.rs`.
- Native `port_command/2,3` + `port_info` closed-port/`registered_name` floor → `../native/port.rs`.
- C fixtures + end-to-end A.4 tests → `../../tests/{driver.rs,port_wake.rs,port_parity.rs}` and
  `../../tests/fixtures/{event_driver.c,event_driver.erl,README.md}`.
