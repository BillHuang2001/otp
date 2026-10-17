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

### The statically-linked driver set (`driver.rs` — the C `driver_tab.c` seam)
Only the drivers the emulator **statically links** are addressable through
`open_port({spawn_driver, Name}, …)`; `erts-*/lib/` ships zero `.so`, and the oracle's
`erl_ddll:loaded_drivers/0` answers exactly `{ok,["tcp_inet","udp_inet","ram_file_drv"]}`
(OTP 29.1.1 / erts-17.1). `driver.rs` exposes that set and nothing else there:
- `STATIC_SPAWN_DRIVERS: [&str; 3]` — `["ram_file_drv","tcp_inet","udp_inet"]`, sorted; the
  emulator's `driver_tab` entries, **not** `.so`-loadable.
- `is_static_spawn_driver(name) -> bool` — membership in that set.
- `DriverRegistry::with_static_spawn_drivers()` — a registry whose `names()` is exactly the three.
- `DriverRegistry::create_static_spawn(name)` — the `{spawn_driver, Name}` resolution path: a
  non-member is `BeamError::BadArg` (the oracle's `badarg` for `{spawn_driver,"fd"}` / `"efile"`)
  *even on a registry that registers it*, then the ordinary factory lookup (so a static name
  missing from *this* registry is also `BadArg`, never a wrong driver).
`DriverRegistry::with_builtins()` still registers five names — `fd` and `efile` are this crate's
own drivers (its internal users and tests build ports from them) — so a runtime must gate
`{spawn_driver, Name}` on `create_static_spawn`, **not** on `with_builtins`/`create`.

### `{line, N}` line framing (`open_port(…, [line])` / `[binary, {line, N}]`)
The C splits lines **in the port**, not in the driver: `open_port`'s bare `line` option sets
`ERTS_PORT_SFLG_LINEBUF_IO` with a 512-byte line buffer and `{line, N}` sets it to `N`
(`../../../erts/emulator/beam/erl_bif_port.c:862-869,986`), and the port wraps every frame it splits
out of a read as `{eol | noeol, Data}` inside `{Port, {data, …}}` (`deliver_linebuf_message`,
`io.c:3454`). The Rust side reproduces that placement — the driver still banks raw bytes; the
**port** frames them — across four modules:
- `drivers/line_buffer.rs` → `LineFrame { bytes, eol }` (the C `LINEBUF_EOL`/`LINEBUF_NOEOL` result)
  plus the frame-level API `take_frames`/`drain_frames`/`flush_frames`/`flush_into_ready`. The
  byte-only views (`take_lines`/`drain`/`flush_all`) are the same frames with the marker dropped and
  are what the out-of-scope `drivers/binary_io.rs` still uses. The byte stream itself is unchanged
  from before the marker was added.
- `port_task.rs` → `LineNotification { port, bytes, eol }` — a **separate** struct, deliberately
  **not** a third `PortNotification` variant (a line-mode port's input never rides the raw
  `{Port, {data, Data}}` channel, and `PortNotification` keeps exactly its two variants so an
  exhaustive consumer stays exhaustive). Unlike `Data`, an **empty payload is a real frame**
  here — a blank line is `{data, {eol, []}}`.
- `port.rs` → `Port::set_line_mode(Option<usize>)` / `line_mode()` (the `SFLG` bit + `linebuf->max_len`;
  `Some(0)` = no length limit), `Port::flush_linebuf()` (the C `flush_linebuf_messages`), and
  `take_input_notifications()` (raw mode: 0/1 `Data`; line mode: **no** notification, the frames are
  staged and drained by `take_line_frames()`).
  `Port::take_eof()` is the crate's **`driver_eof` observation point** and runs the flush.
- `registry.rs` → `PortRegistry::set_line_mode(id, Option<usize>)` / `flush_linebuf(id)` (the runtime's
  adoption seams) plus `take_line_notifications()` (drains every port's staged frames in port-number
  order — the accessor style of `take_eof`/`take_closed_notifications`); `execute_port_tasks` still
  harvests one `PortNotification` per port and its `notifications` carry only `PortNotification`s,
  so a single read can stage many frames (597 bytes at `{line,4}` ⇒ 150, on the line channel).

### A driver's write path — the reply round trip and a refused command
`PortRegistry::port_command` only **queues** the bytes and schedules one `Output` task; the write
belongs to the round (`execute_port_tasks` → `run_scheduled_tasks` → `Port::run_task(Output)` →
`Port::ready_output`, whose `Driver` default forwards to `output`). Whatever a driver pushes onto
`ctx.input()` — a built-in driver's `port_command` reply, or a `driver_output` made from
`start`/`control`/`timeout`/`output` — is harvested for **every** live port after the round and
reported as **exactly one** `PortNotification::Data`, drained once per byte (no double report on a
later round; a port in `{line, N}` mode reports through the separate `LineNotification` channel
instead). A `ram_file_drv` port needs no descriptor, so the whole chain is testable with no
child process and no poller wait.
A driver that **refuses** a write (`Err(BeamError::BadArg)` from `output`/`ready_output`, e.g. an
`inet` port whose socket is not open) is surfaced as that error: it short-circuits
`Port::check_write_accounting` (`result?` runs first, so the driver's reported byte count is never
inspected) and propagates out of `PortRegistry::execute_port_tasks` (`Err` — **not** a
`PortTaskOutcome`). Because the command was queued before it was driven, a refused command's bytes
stay in the output queue and the failing task is already popped, so a later round neither retries
nor re-reports it. A loaded C driver cannot refuse at all (`output`/`outputv` are `void`,
`prt->bytes_out += size`), so this shape is Rust-side: the runtime must read the error as "the
command was refused" and render the oracle's `badarg`.

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
- **The partial-line flush rides `take_eof`, so it needs the `eof` *stream option* in this crate.** The
  port's flush is triggered when `Driver::take_eof` reports `true`, but `drivers/spawn.rs`
  (`SpawnDriver::take_eof`) reports only when `StreamOptions.eof` was set — the driver latches the end of
  file either way. The oracle flushes at `driver_eof` regardless of the option, so a `[line]` port opened
  without `eof` would hold its final partial line here. Closing that needs a driver-side input-EOF
  accessor independent of the notification option (an additive `Driver` seam), and the runtime's
  adoption of `set_line_mode`/`flush_linebuf` on the `open_port` path.
- **A close never flushes the line buffer** (`io.c:7211` runs the flush from `driver_eof` only), and the
  partial line is neither reported nor discarded by it — it is still held afterwards, so an explicit
  `flush_linebuf` still releases it. Pinned by
  `port::tests::closing_a_line_port_never_flushes_the_pending_partial_line`.

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
- **Line framing** is pinned at three levels. `drivers/line_buffer.rs` unit-tests the marker per frame
  (`eol` only for a newline terminator; the limit split and the CR_AFTER split are `noeol`), the
  `{eol, []}` blank line, that every flushed frame is `noeol`, and `flush_into_ready`'s arrival order.
  `port.rs` unit-tests the port-level rule that a **close never flushes** (the held partial survives the
  close and an explicit flush still releases it — impossible had the close reported it) and that the
  `take_eof` flush releases a blank-line frame. `registry.rs` unit-tests the round-level shapes over a
  real pipe port (three `eol` lines, the blank line, the `{line,4}` split, one read ⇒ 150 frames, raw
  mode off by default, a line-mode port reporting **nothing** on `PortNotification`, `BadArg` for an
  unknown id, and the `take_eof`-driven flush) — all through the separate
  `take_line_notifications` channel. `tests/spawn.rs` (the integration node) cross-checks the whole
  pinned oracle table byte-for-byte against the system `erl` through a real `cat` child behind a real
  `[line]` / `[binary, {line, N}]` port, plus the `[binary]` no-wrapper row and the
  close-does-not-flush rule, skipping **loudly** when `erl` is absent.

## Routing Table
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `poller.rs` → `Poller` (`EINTR`-tolerant `poll`, `libc::poll(2)` regular-file fallback), `Token`, `Interest`, `Ready`, `Event`, `Waker`
- `port.rs` → `Port`, `PortState`, `PortControl`, `PortCommandOptions`, `DEFAULT_READINESS_RED_BUDGET`, `refresh_timeout`; the line-framing state + `set_line_mode`/`line_mode`/`flush_linebuf`/`take_input_notifications`/`take_line_frames`
- `registry.rs` → `PortRegistry` (id allocation over `beam_process::PortTable`, reactor glue, `port_command`/`port_control`, the exit/eof/async seams, `port_table`/`port_table_mut`, `set_line_mode`/`flush_linebuf`/`take_line_notifications`)
- `port_task.rs` → `PortTaskKind`/`PortTask`/`PortTaskQueue`, reduction constants, `Readiness`, `PortNotification` (`Data` + `Closed`), `LineNotification` (the separate `{line,N}` channel), `PortTaskOutcome`
- `queue.rs` → `ByteQueue`; `timer.rs` → `Timeout`, `Timer`
- `driver.rs` → `Driver`, `DriverContext`, `DriverFlags`, `DriverRegistry`, `PortExitReason`, `PortArms`
- `error.rs` → `pub(crate)` typed error constructors
- `signal.rs` → `Signal`/`Disposition`/`SignalFacility`; `cpu_time.rs` → `CpuTimes`/`RuntimeClock`; `ddll.rs` → `DynLib`/`DllError`
- `fs_access.rs` → the safe `access(2)`/`geteuid(2)`/`getegid(2)` floor: `AccessMode` (`Exists`/`Read`/`Write`/`Execute`, `ALL`, `libc_mode()`), `try_access` (errno-preserving `Result<()>`, the one `unsafe` `access(2)` call site), `access` (`Result<bool>`, refusal ⇒ `false`), `path_access` (`Result<(read, write)>` — the `#file_info{}` `access` slot, two independent probes) and `process_credentials` (`Result<(euid, egid)>`)
- `drivers/` → the built-in drivers (`fd`, `efile`, `spawn`, `tcp_inet`, `udp_inet`, `ram_file_drv`) + framing, incl. `line_buffer.rs`'s `LineFrame`/`LineBuffer` (the port-level `{eol | noeol, Data}` splitter) — child node `drivers/CONTEXT.md`
- `abi/` → the driver C-ABI host layer — child node `abi/CONTEXT.md`
- `io_device/` → the `{get_until,…}` io-device request layer — child node `io_device/` (files listed in `../CONTEXT.md`)

## See Also
- `../CONTEXT.md` → the authoritative crate doc (frozen API surface, constraints, test strategy, handoffs).
- `../../../erts/emulator/beam/{io.c,erl_port_task.c,erl_port_task.h,erl_bif_port.c}` and
  `../../../erts/emulator/sys/common/{erl_poll.c,erl_check_io.c}` → the legacy C ground truth
  (`io.c:3140-3307` the `LineBuf` state machine, `:3454` `deliver_linebuf_message`, `:7211` the
  `driver_eof` flush; `erl_bif_port.c:862-869,986` the `line`/`{line,N}` option decoding).
