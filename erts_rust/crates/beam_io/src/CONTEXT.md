# `beam_io/src` — crate source root (module wiring)

## Intent
Source root of `beam_io` (the I/O subsystem of the Rust ERTS rewrite) plus the three sub-nodes that
carry the bulk of the code. The **authoritative** description of this crate — intent, the frozen API
surface per module, constraints, test strategy and design decisions — lives in the parent
`../CONTEXT.md`; read it before changing anything here. This file only routes and records gotchas.

## API Surface
Module wiring + root re-exports + `prelude`: `src/lib.rs`. Every module below is re-exported at the
crate root and in `prelude`.

Four modules are **standalone raw-OS facilities** (safe wrappers over syscalls `beam_rt`, which
denies `unsafe_code`, cannot issue itself) rather than parts of the I/O pipeline: `signal.rs`
(`os:set_signal/2`), `cpu_time.rs` (`erlang:statistics(runtime)`), `ddll.rs` (`erl_ddll`) and
`fs_access.rs` (`access(2)` / `geteuid(2)` / `getegid(2)`, for the `file`/`filelib` floor). Their
frozen APIs are documented in the parent `../CONTEXT.md` and in each module's own rustdoc.

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

## Test Strategy
In-module `#[cfg(test)]` unit tests only — the sibling `tests/` directory belongs to the
integration-test node, and the crate-level policy (real fds, loud skips, oracle cross-checks) is in
`../CONTEXT.md`.
- `fs_access.rs` (10 tests) covers the mode→`<unistd.h>`-constant table, the refusal-vs-error split,
  the interior-NUL `badarg` and real fixtures under `$TMPDIR`: a `0644` file, a `0755` directory, a
  missing path, an executable file, a file with no execute bit, and a mode-`000` file. One test
  cross-checks `path_access`/`process_credentials` against the **`erl` oracle** —
  `file:read_file_info/1`'s `access`/`uid`/`gid`, read by field position per
  `lib/kernel/include/file.hrl` — and skips **loudly** when `erl` is not on `PATH`.
- **uid robustness:** `access(2)` answers differently for a privileged process, so the mode-`000`
  read/write refusal is asserted only when `euid != 0` and that sub-case skips loudly under a uid-0
  container. The missing-path refusal (`ENOENT`) and the no-execute-bit case are asserted
  unconditionally (POSIX fixes the latter for every uid, root included). The oracle test needs no
  guard at all: it compares two implementations of the *same* syscall, so they must agree for any uid.

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
- `fs_access.rs` → the safe `access(2)`/`geteuid(2)`/`getegid(2)` floor: `AccessMode` (`Exists`/`Read`/`Write`/`Execute`, `ALL`, `libc_mode()`), `try_access` (errno-preserving `Result<()>`, the one `unsafe` `access(2)` call site), `access` (`Result<bool>`, refusal ⇒ `false`), `path_access` (`Result<(read, write)>` — the `#file_info{}` `access` slot, two independent probes) and `process_credentials` (`Result<(euid, egid)>`)
- `drivers/` → the built-in drivers (`fd`, `efile`, `spawn`, `inet`) + framing — child node `drivers/CONTEXT.md`
- `abi/` → the driver C-ABI host layer — child node `abi/CONTEXT.md`
- `io_device/` → the `{get_until,…}` io-device request layer — child node `io_device/` (files listed in `../CONTEXT.md`)

## See Also
- `../CONTEXT.md` → the authoritative crate doc (frozen API surface, constraints, test strategy, handoffs).
- `../../../erts/emulator/beam/{io.c,erl_port_task.c,erl_port_task.h}` and
  `../../../erts/emulator/sys/common/{erl_poll.c,erl_check_io.c}` → the legacy C ground truth.
