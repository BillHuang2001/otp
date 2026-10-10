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
- **`PortNotification` carries a bare `PortId`, but port numbers are reused.** `PortTable::alloc`
  pops a freed number off its free list (`../../beam_process/src/port.rs:104-118`) and
  `PortRegistry::close` releases the id immediately (`registry.rs:548-558`) while the id is queued in
  `self.closed` for a *later* round. A `PortNotification::Closed { port }` produced by
  `execute_port_tasks` (`registry.rs:261-265`) can therefore name a **different, re-opened** port.
  Consumers that act on `Closed` (today `beam_rt` ignores it) must not trust the bare number; a
  generation/ref in the notification, or non-reusing ids, is the fix.
- **`PortRegistry::handle_events` never drains `self.closed`.** Only `execute_port_tasks` drains it
  (`registry.rs:261-265`), so a caller that drives the registry through `handle_events` alone
  accumulates `self.closed` without bound and never sees a `Closed` notification. `handle_events`
  also returns the count of **events naming a live port** (`registry.rs:191-195`), not the number of
  tasks run. `beam_rt` avoids this by using `schedule_events` + `execute_port_tasks` directly.
- **The driver `consume()`-vs-returned-count invariant is contract-only.** A driver's `output` /
  `ready_output` must shorten the queue by calling `ctx.output().consume(n)` for every byte it
  returns; nothing checks it (`port.rs` merely forwards the returned count — `Port::run_task`
  discards it, `port.rs:342-345`). A driver that returns `Ok(n)` without consuming leaves the bytes
  queued, so they are written **again** on the next drive (duplicated bytes). All in-tree drivers do
  consume (`drivers/fd.rs:413`, `drivers/spawn.rs:999`), so this is a trap for new drivers only.
- **No `Driver::flush`; a close drops queued bytes.** `Port::close` / `PortRegistry::close`
  (`port.rs:597-615`, `registry.rs:548-558`) run `stop` and release the id without draining either
  queue, whereas the C calls `flush` before `terminate_port` and parks a port with a non-empty `ioq`
  in `ERTS_PORT_SFLG_CLOSING` (`../../../erts/emulator/beam/io.c:3659-3700,3946-3956`). Mitigated at
  the call site only (`beam_rt` sends `notify_port_exit` before `registry.close`).

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
