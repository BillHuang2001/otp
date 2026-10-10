# `beam_io` built-in drivers (`src/drivers/`)

## Intent
The built-in **port drivers** of the Rust ERTS rewrite: the byte-stream `fd` driver, the `efile`
file driver composed on top of it, the OS-child `spawn` driver, the `tcp_inet` TCP socket driver, and
the two port-input framing facilities (`{line,N}` buffering, the ERTS text↔binary *phase change*).
Every driver implements the frozen [`crate::driver::Driver`] trait and touches the port only through
[`crate::driver::DriverContext`] (`input`/`output`/`register`/`reregister`/`deregister`). Each owns
the descriptors it was given; nothing else may close them. Only `spawn.rs` may fork/`Command::spawn`.

Legacy C ground truth (read-only): `erts/emulator/beam/io.c` (the `LineBuf` `LINEBUF_*` machine, the
`ioq`), `erts/emulator/nifs/common/prim_file_nif.c` + `erts/emulator/nifs/unix/unix_prim_file.c` (the
`efile` mode-list/verb/syscall layer), `erts/emulator/drivers/unix/spawn_driver.c` +
`erts/emulator/sys/unix/{sys_drivers.c,erl_child_setup.c}`, `erts/emulator/drivers/common/inet_drv.c`
+ `erts/emulator/beam/packet_parser.{c,h}`.

## API Surface (summary — the crate root `../CONTEXT.md` carries the frozen detail)
- `fd.rs` → `FdDriver` (`pipe`/`connected`/`read_only`/`write_only`, line/phase modes, the
  `line`/`noline`/`text`/`binary`/`getopts` control verbs, EOF de-registration, the `pub(crate)`
  read/write-slot seams `adopt_read_fd`/`release_read_fd`/`adopt_write_fd`/`release_write_fd`/
  `has_*_fd`/`attached_fd`), `set_nonblocking`.
- `efile.rs` → `EfileDriver`, `make_efile`, `FileDataMode`; the `open` flag grammar and `Access`
  translation reproduce the C's `efile_translate_modelist` + `get_flags`.
- `spawn.rs` → `SpawnDriver`, `SpawnSpec`/`SpawnOptions`/`StreamOptions`/`SpawnStream`,
  `build_command`, the deferred `fork_exec_child`, `ChildProc`/`ForkedChild`.
- `line_buffer.rs` → `LineBuffer`, `DEFAULT_MAX_LINE_LEN`; `binary_io.rs` → `Phase`, `PhaseChange`.
- `mod.rs` → module wiring/re-exports only.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only (`BadArg`/`Io`/`SystemLimit`, or `Raise` for the
  spawn errno atom). Never panic/`unwrap`/`todo!` on runtime data; all `unsafe` (raw syscalls) lives in
  these modules with a `// SAFETY:` rationale per block.
- The byte path is **edge-triggered**: every `ready_input`/`write_pending` must drain to `EAGAIN`.
  A short write must leave the exact unwritten remainder queued and arm write readiness; a short read
  must never drop or duplicate bytes. `ByteQueue::front()` is always the whole contiguous remainder.
- The `line_buffer.rs` state machine and the `\r\n`-terminator / over-long-line-split rules are
  arm-for-arm ports of the C `read_linebuf` (`io.c:3252`) — change them only against that.

## Known Issues
- **`PhaseChange::flush()` loses text-phase bytes once the phase is `Binary`.** `flush`
  (`binary_io.rs:172-177`) returns no frames in `Phase::Binary`, but `binary_io.rs:36-40` states a
  partial line "survives a switch to binary and is delivered by the next `flush`". Reachable path:
  the driver buffers a partial line in `Text`, a `binary`/`noline` control command flips the phase
  (`fd.rs:568-571`/`182-184`), then EOF or `stop` calls `flush_input` → `phase.flush()` → nothing, so
  the held bytes are silently dropped. Fix: `flush` should drain the line buffer regardless of the
  current phase (the buffer is empty in `Binary` unless it was populated in `Text`); the existing
  `binary_io` tests still pass because they switch back to `Text` before flushing.
- **`{fd,N}` spawn port holds an `Interest::BOTH` registration for its life.** `arm_output`/
  `disarm_output` are no-ops for `duplex` (`spawn.rs:913-941`), so the single adopted descriptor stays
  registered both ways after read EOF (`release_stdout` is a no-op for duplex). Harmless under the
  edge-triggered poller, but a spurious `OUTPUT` task is scheduled on every write-readiness event.
- `inet/` carries its own current defects (no send-side `{packet,N}` framing; `verb_send` ignores a
  deferred `shutdown`) — see `inet/CONTEXT.md` › *Known Issues*.
- Non-obvious framing divergences recorded in code: an out-of-range `{packet,N}` code is treated as
  **no framing** where the C's `packet_get_length` `default:` rejects it (`inet/frame.rs:96-105`); an
  unknown packed-address family is `badarg` where the C answers `eafnosupport` (`inet/wire.rs`).

## Notes for Agents
- `EfileDriver` composes an `FdDriver`; a read+write `open` attaches **two** `dup(2)` descriptors (one
  per slot), each closed exactly once (`efile.rs:383-401`).
- The `line` control argument accepts the `getopts` reply shape (`line,<N>`) as well as a bare decimal.
- Tests that use the real `erl`/`cc` oracle skip loudly when it is absent; never fail.

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` (sandbox `$HOME`
is read-only — always the wrapper). Real-pipe/socketpair/reactor round-trips for `fd`/`efile`, real
children for `spawn`, loopback TCP + `erl`-oracle cross-checks for `inet`. Byte-framing is asserted
against the C `read_linebuf` cases in `line_buffer.rs`'s in-module suite.

## Routing Table
- `fd.rs` → `FdDriver` + `set_nonblocking` (the shared non-blocking wrapper)
- `efile.rs` → `EfileDriver`/`make_efile`/`FileDataMode`, the `open` grammar + `Access`/`OpenFlags`
- `spawn.rs` → `SpawnDriver`/`SpawnSpec`/`SpawnOptions`/`StreamOptions`/`build_command`
- `line_buffer.rs` → `LineBuffer`; `binary_io.rs` → `Phase`/`PhaseChange`
- `inet/` → the `tcp_inet` driver + read-path framing (child node `inet/CONTEXT.md`)
- `../abi/` → the driver C-ABI host layer (sibling child node, not a built-in driver)

## See Also
- `../CONTEXT.md` → the `beam_io` crate root: the frozen `Driver`/`DriverContext` seam, `PortRegistry`,
  reducer constants, the per-driver API surface and the driver handoffs.
- `../../CONTEXT.md` → crate-layer contracts, the A.4 driver C-ABI contract.
