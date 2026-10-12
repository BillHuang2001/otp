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
  translation reproduce the C's `efile_translate_modelist` + `get_flags`, and `open(2)` is retried on
  `EINTR` exactly as the C's `do { … } while (fd == -1 && errno == EINTR)` does
  (`retry_on_eintr`, `unix_prim_file.c:188-191`).
- `spawn.rs` → `SpawnDriver`, `SpawnSpec`/`SpawnOptions`/`StreamOptions`/`SpawnStream`,
  `build_command`, the deferred `fork_exec_child`, `ChildProc`/`ForkedChild`.
- `line_buffer.rs` → `LineBuffer`, `DEFAULT_MAX_LINE_LEN`; `binary_io.rs` → `Phase`, `PhaseChange`.
- `mod.rs` → module wiring/re-exports only.

## Constraints
- Errors are `beam_common::{Result, BeamError}` only (`BadArg`/`Io`/`SystemLimit`, or `Raise` for the
  spawn errno atom). Never panic/`unwrap`/`todo!` on runtime data; all `unsafe` (raw syscalls) lives in
  these modules with a `// SAFETY:` rationale per block — except the `access(2)` behind
  `spawn.rs`'s `check_executable`, which delegates to `crate::fs_access::try_access` (the crate's
  single `access(2)` call site) and only renders the returned errno as the oracle's raise.
- The byte path is **edge-triggered**: every `ready_input`/`write_pending` must drain to `EAGAIN`.
  A short write must leave the exact unwritten remainder queued and arm write readiness; a short read
  must never drop or duplicate bytes. `ByteQueue::front()` is always the whole contiguous remainder.
- The `line_buffer.rs` state machine and the `\r\n`-terminator / over-long-line-split rules are
  arm-for-arm ports of the C `read_linebuf` (`io.c:3252`); `LineBuffer::flush_all` likewise mirrors
  `flush_linebuf` (`io.c:3211-3237`), including delivering a full line at the limit and its held `\r`
  as **two** frames (the C's two calls). Change them only against those.
- `PhaseChange::flush` drains the line buffer **whatever the current phase is** (`binary_io.rs`): a
  `binary`/`noline` control command only flips the phase, it never discards input the driver already
  read in the text phase, so the held bytes must still reach the port queue at EOF/`stop`.

## Known Issues
- **Line framing (`[line]` / `[binary,{line,N}]`) is implemented at the PORT; the remaining half is upstream in `beam_rt`.** The C splits lines in the port, not the driver (`deliver_linebuf_message`, `io.c:3454`), so `SpawnDriver::ready_input` pushing raw bytes into `ctx.input()` is correct, and `FdDriver` carrying the `line`/`text` framing (`spawn.rs:100` says so explicitly) is a driver-level facility for `fd`/`efile`, not the mechanism the oracle's message shape needs.
  `LineBuffer` reproduces the C `read_linebuf`/`flush_linebuf` byte stream and its `LINEBUF_EOL`/`LINEBUF_NOEOL` marker (`io.c:3252-3307`) arm-for-arm — the suspected trailing-`\r` divergence is a **non-issue**: the C appends a `\r` exactly when it is followed by a non-`\n` (`LINEBUF_CR_INSIDE`, `io.c:3290`) and `LineBuffer` does the same (`line_buffer.rs:150-154`; `flush_all` restores the held `\r`), verified against the oracle (`abcd\r` then `x` ⇒ one `{noeol,[13,120]}` frame carrying `\r`+`x`).
  The `{eol|noeol,Data}` wrapper's port half is done: `Port::set_line_mode` drives the framing, the frames ride the separate `PortRegistry::take_line_notifications` channel (`LineNotification`, many per read, empty `eol` frame kept) rather than the raw `PortNotification::Data` one, `take_eof` runs the `driver_eof` flush, and a close never flushes. The whole pinned oracle table is cross-checked byte-for-byte in `tests/spawn.rs::line_framing_matches_the_oracle`.
  What is still missing is `beam_rt`'s: the `{Port,{data,{eol|noeol,Data}}}` **term rendering**, the `open_port/2` `line`/`{line,N}` **option parsing** and draining `take_line_notifications` after each round (`beam_rt/src/port.rs::port_options` whitelists-but-ignores the options today, so `[line,{packet,2}]` and `[line,stderr_to_stdout,nouse_stdio]` succeed where the oracle raises `error:badarg`), so the gap is **not** end-to-end closable inside `beam_io`.
  **Oracle stream (OTP 29 / erts-17.1):** `[line]` `printf 'a\nb\nc\n'` ⇒ `{data,{eol,"a"}}`,`{data,{eol,"b"}}`,`{data,{eol,"c"}}`; `printf 'a\nb'` ⇒ `{eol,"a"}` then `{noeol,"b"}` (the partial flushes at **child EOF**, as `noeol`); `[binary]` ⇒ plain `{data,<<…>>}` (no wrapper); `[binary,{line,4}]` over `abcdefgh\n` ⇒ `{noeol,<<abcd>>}` then `{eol,<<efgh>>}`; a blank line ⇒ `{data,{eol,[]}}`; the `{eol|noeol,_}` wrapper is applied in **both** list and binary mode; the split is state-dependent, not write-chunk-dependent (`\n` alone still completes the pending line as `eol`), and `port_close` does **not** flush (only child EOF does — `io.c:7211`).
- **`{fd,N}` spawn port holds an `Interest::BOTH` registration for its life.** `arm_output`/
  `disarm_output` are no-ops for `duplex` (`spawn.rs:913-941`), so the single adopted descriptor stays
  registered both ways after read EOF (`release_stdout` is a no-op for duplex). Harmless under the
  edge-triggered poller, but a spurious `OUTPUT` task is scheduled on every write-readiness event.
- Non-obvious framing divergences recorded in code: an out-of-range `{packet,N}` code is treated as
  **no framing** where the C's `packet_get_length` `default:` rejects it (`inet/frame.rs:96-105`); an
  unknown packed-address family is `badarg` where the C answers `eafnosupport` (`inet/wire.rs`).

## Notes for Agents
- **Waiting on a spawn child (from a test): poll the driver's own accessors, never `waitpid(2)`
  directly.** `SpawnDriver` reaps eagerly, and **both** child kinds answer `try_wait` from a *cached*
  status — `ChildProc::Standard` is a `std::process::Child` (whose `try_wait` caches) and
  `ChildProc::Forked` is a `ForkedChild`, which stores the first `waitpid` result in `self.status`.
  Repeating `SpawnDriver::exited`/`take_exit_status` is therefore **idempotent**: it can neither
  double-reap nor race the driver's teardown (the teardown's `kill_and_reap` reuses that cache and so
  needs no kill). A raw `waitpid(child_pid, ..)` issued from a test would steal the reap out from
  under the driver and hit `ECHILD`.
- **The spawn suite's waits poll a *condition* under a 60 s hang guard, never a 5 s budget.** The
  in-module helpers `WAIT_HANG_GUARD`, `drive_to_eof` (end-of-file latch) and `wait_for_exit_status`
  (once-only status) end the instant the condition holds, so a loaded host cannot fail them; the guard
  only stops a genuinely broken driver from hanging the suite. A fixed 5-second cap of exactly that
  shape is what made the `a_bad_cd…` / `a_file_with_no_executable_format…` tests flaky under load — on
  a starved host the child needed just over 5 s to be scheduled, run and be reaped. Keep the `EINTR`
  retry in the poll loop (`SIGCHLD` deliveries make it a normal outcome).
- `EfileDriver` composes an `FdDriver`; a read+write `open` attaches **two** `dup(2)` descriptors (one
  per slot), each closed exactly once (`efile.rs:383-401`).
- The `line` control argument accepts the `getopts` reply shape (`line,<N>`) as well as a bare decimal.
- Tests that use the real `erl`/`cc` oracle skip loudly when it is absent; never fail.

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` (sandbox `$HOME`
is read-only — always the wrapper). Real-pipe/socketpair/reactor round-trips for `fd`/`efile`, real
children for `spawn`, loopback TCP + `erl`-oracle cross-checks for `inet`. Byte-framing is asserted
against the C `read_linebuf` cases in `line_buffer.rs`'s in-module suite. The `spawn` suite's
child-exit and end-of-file waits are **condition-based under a 60 s hang guard**, never a wall-clock
budget tuned for an idle host (see *Notes for Agents*).

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
