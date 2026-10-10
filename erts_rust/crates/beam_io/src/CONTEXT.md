# `beam_io/src` — crate source root (module wiring)

## Intent
Source root of `beam_io` (the I/O subsystem of the Rust ERTS rewrite) plus the three sub-nodes that
carry the bulk of the code. The **authoritative** description of this crate — intent, the frozen API
surface per module, constraints, test strategy and design decisions — lives in the parent
`../CONTEXT.md`; read it before changing anything here. This file only routes and records gotchas.

## API Surface
Module wiring + root re-exports + `prelude`: `src/lib.rs`. Every module below is re-exported at the
crate root and in `prelude`.

## Known Issues (durable gotchas — not fixed by changing this file)
- **A `PortNotification::Closed` names a number that is retired *with the report*.** `PortRegistry::close`
  queues the id in `self.closed` and keeps it **allocated** in `ids`; the reporting round
  (`execute_port_tasks` and `handle_events`, both via `take_closed_notifications`) mints the `Closed`
  and releases the number there. So a `close` + `open` before the next round cannot make the deferred
  `Closed` name a reused, live port. A consumer must act on a `Closed` in the round that delivered it —
  the number may be handed out again immediately afterwards.
- **No `Driver::flush`; a close drops queued bytes.** `Port::close` / `PortRegistry::close`
  (`port.rs`, `registry.rs`) run `stop` and queue the close without draining either queue, whereas the C
  calls `flush` before `terminate_port` and parks a port with a non-empty `ioq` in
  `ERTS_PORT_SFLG_CLOSING` (`../../../erts/emulator/beam/io.c:3659-3700,3946-3956`). Mitigated at the
  call site only (`beam_rt` sends `notify_port_exit` before `registry.close`).

## Routing Table
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `poller.rs` → `Poller` (`EINTR`-tolerant `poll`, `libc::poll(2)` regular-file fallback), `Token`, `Interest`, `Ready`, `Event`, `Waker`
- `port.rs` → `Port`, `PortState`, `PortControl`, `PortCommandOptions`, `DEFAULT_READINESS_RED_BUDGET`, `refresh_timeout`
- `registry.rs` → `PortRegistry` (id allocation over `beam_process::PortTable`, reactor glue, `port_command`/`port_control`, the exit/eof/async seams, `port_table`/`port_table_mut`)
- `port_task.rs` → `PortTaskKind`/`PortTask`/`PortTaskQueue`, reduction constants, `Readiness`, `PortNotification`, `PortTaskOutcome`
- `queue.rs` → `ByteQueue`; `timer.rs` → `Timeout`, `Timer`
- `driver.rs` → `Driver`, `DriverContext`, `DriverFlags`, `DriverRegistry`, `PortExitReason`, `PortArms`
- `error.rs` → `pub(crate)` typed error constructors
- `signal.rs` → `Signal`/`Disposition`/`SignalFacility`; `cpu_time.rs` → `CpuTimes`/`RuntimeClock`; `ddll.rs` → `DynLib`/`DllError`
- `drivers/` → the built-in drivers (`fd`, `efile`, `spawn`, `inet`) + framing — child node `drivers/CONTEXT.md`
- `abi/` → the driver C-ABI host layer — child node `abi/CONTEXT.md`
- `io_device/` → the `{get_until,…}` io-device request layer — child node `io_device/` (files listed in `../CONTEXT.md`)

## See Also
- `../CONTEXT.md` → the authoritative crate doc (frozen API surface, constraints, test strategy, handoffs).
- `../../../erts/emulator/beam/{io.c,erl_port_task.c,erl_port_task.h}` and
  `../../../erts/emulator/sys/common/{erl_poll.c,erl_check_io.c}` → the legacy C ground truth.
