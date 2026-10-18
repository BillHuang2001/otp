# `beam_io` built-in drivers (`src/drivers/`)

## Intent
The built-in **port drivers** of the Rust ERTS rewrite: the byte-stream `fd` driver, the `efile`
file driver composed on top of it, the OS-child `spawn` driver, the `tcp_inet` TCP socket driver and
its `udp_inet` datagram half, the in-memory `ram_file_drv` file driver, and the two port-input framing
facilities (`{line,N}` buffering, the ERTS text↔binary *phase change*).
Every driver implements the frozen [`crate::driver::Driver`] trait and touches the port only through
[`crate::driver::DriverContext`] (`input`/`output`/`register`/`reregister`/`deregister`). Each owns
the descriptors it was given; nothing else may close them. Only `spawn.rs` may fork/`Command::spawn`.

Legacy C ground truth (read-only): `erts/emulator/beam/io.c` (the `LineBuf` `LINEBUF_*` machine, the
`ioq`), `erts/emulator/nifs/common/prim_file_nif.c` + `erts/emulator/nifs/unix/unix_prim_file.c` (the
`efile` mode-list/verb/syscall layer), `erts/emulator/drivers/unix/spawn_driver.c` +
`erts/emulator/sys/unix/{sys_drivers.c,erl_child_setup.c}`, `erts/emulator/drivers/common/inet_drv.c`
+ `erts/emulator/beam/packet_parser.{c,h}`, `erts/emulator/drivers/common/ram_file_drv.c` (the
`ram_file` command protocol).

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
- `ram_file.rs` → `RamFileDriver` (`"ram_file_drv"`), `make_ram_file`, `DRIVER_NAME` (aliased
  `RAM_FILE_DRIVER_NAME` at the crate root) and the `pub` `RAM_FILE_*` command/mode/whence/response
  constants. A **command-driven** port holding its whole "file" in memory (no descriptor, nothing
  registered with the poller): [`Driver::output`](crate::Driver::output) drains the port's output
  queue, reads the **first byte as the command word** and the rest as its operands, and appends the
  reply to the port's input queue (the C's `rfile_command` in the `output` slot +
  `driver_output2`/`driver_output_binary`); [`Driver::control`](crate::Driver::control) is the
  named-verb form of the same core (`open`/`read`/`write`/`pread`/`pwrite`/`lseek`|`position`/
  `truncate`/`fsync`|`sync`/`fdatasync`|`datasync`/`get`|`get_file`/`size`|`get_size`/`advise`/
  `allocate`/`close`, plus the **decimal opcode** itself). **Operation-level failures are replies,
  not Rust errors**: a wrong access mode / a negative offset / an allocation failure emits the C's
  `ERROR <errno>` reply (`errno_atom`); only an unknown command word or a too-short argument is the
  typed `badarg`. The logical file is a `Vec<u8>` whose `len()` **is** the C's `end` (the C's
  separate allocation `size` field is unobservable and has no counterpart); a write past the end
  zero-fills the gap (the C's `ram_file_expand`).
- `inet/` → the `tcp_inet`/`udp_inet` socket drivers. [`Driver::control`](crate::Driver::control) takes
  the C's packed request payload in **two** wire forms: a **decimal** command is
  `erlang:port_control/3`'s integer `Op`, forwarded by the runtime as its decimal string and decoded
  by `opcode.rs` (accumulated mod 2³², `wrapping_sub(CONTROL_MAGIC_NUMBER = 0x03f1_a300)`, then the
  C's three switches) onto the **same** verb body the named form uses, so the numeric and named
  routes answer identical bytes; any other command is a named verb. The **operand** decode stays the
  runtime's. A number no table entry matches — including a case of the other driver's switch, and
  every op below the magic (`"1"`, `"99"`) — answers the C's `[0]exbadport`; an opcode the C serves
  but this floor does not is the typed `Opcode::not_served()` unsupported raise. `inet/CONTEXT.md`
  carries the full served-vs-refused opcode table. A command on a socket-less or peerless port is the
  typed `BeamError::BadArg` refusal of *Known Issues* below.
  `setopts`/`getopts` serve the C's whole option surface in two families — `opt_state.rs` (the codes
  the `inet`/`tcp`/`udp` descriptor keeps: `active`, `packet`, `mode`, `deliver`, the watermarks,
  `delay_send`, `send_timeout*`, `show_econnreset`, `read_ahead`, `read_packets`, `netns`,
  `line_delim`, the membership pair) and `opt_sys.rs` (everything that reaches
  `setsockopt(2)`/`getsockopt(2)`, incl. `linger`'s `struct linger` shapes, `bind_to_device`, `raw`,
  `pktoptions`, `multicast_if`) — so `prim_inet:accept_opts/3`, the `getopts`+`setopts` pair a
  `gen_tcp:accept/1` ends in, completes. 42 codes are byte-compared against the oracle across the
  three port states (no socket / open stream / post-`close`).
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
- **The C `ram_file_drv` answers its two reply families through two different output calls.** `OK`/`ERROR`/`NUMBER` go through `driver_output2` (a plain byte list) while `DATA` goes through `driver_output_binary` (an improper `[header | binary]` iolist on a `[binary]` port), so an oracle probe that reads the raw port message must normalise with `binary_to_list(iolist_to_binary(D))` before comparing — and `io:format("~p", [D])` on the raw term raises. In this crate both families simply become bytes appended to [`DriverContext::input()`](crate::DriverContext::input), which is what the port delivers to the owner.
- **`ram_file_drv` is a registered built-in with a live runtime path.** `DriverRegistry::with_builtins()`
  (in `../driver.rs`) registers `"fd"`/`"efile"`/`"tcp_inet"`/`"udp_inet"`/`"ram_file_drv"`, and
  `beam_rt` opens a port on it through `open_port({spawn_driver,"ram_file_drv"}, …)` (its
  `src/port/ram_file_matrix_tests.rs` cross-checks this driver's whole command/reply matrix against the
  live oracle). The port's reserved `"close"` is the teardown op for a raw `open_port` port; the
  driver's own `"close"` verb exists for a direct caller and is what
  [`Driver::stop`](crate::Driver::stop) runs.
- **Line framing (`[line]` / `[binary,{line,N}]`) is applied at the PORT, not in the driver.** The C
  splits lines in the port, not the driver (`deliver_linebuf_message`, `io.c:3454`), so
  `SpawnDriver::ready_input` pushing raw bytes into `ctx.input()` is correct, and `FdDriver` carrying
  the `line`/`text` framing is a driver-level facility for `fd`/`efile`, not the mechanism the oracle's
  message shape needs.
  `LineBuffer` reproduces the C `read_linebuf`/`flush_linebuf` byte stream and its
  `LINEBUF_EOL`/`LINEBUF_NOEOL` marker (`io.c:3252-3307`) arm-for-arm — the trailing-`\r` case needs no
  divergence: the C appends a `\r` exactly when it is followed by a non-`\n` (`LINEBUF_CR_INSIDE`,
  `io.c:3290`) and `LineBuffer` does the same (`line_buffer.rs:150-154`; `flush_all` restores the held
  `\r`), verified against the oracle (`abcd\r` then `x` ⇒ one `{noeol,[13,120]}` frame carrying `\r`+`x`).
  The port half is [`Port::set_line_mode`](crate::Port::set_line_mode) plus the separate
  [`PortRegistry::take_line_notifications`](crate::PortRegistry::take_line_notifications) channel
  (`LineNotification`, many per read, an empty `eol` frame kept) rather than the raw
  `PortNotification::Data` one; `take_eof` runs the `driver_eof` flush and a close never flushes.
  `beam_rt` adopts the whole chain (its `open_port/2` option parsing, the `{Port,{data,{eol|noeol,Data}}}`
  rendering and the per-round drain); the pinned oracle table is cross-checked byte-for-byte in
  `tests/spawn.rs::line_framing_matches_the_oracle`.
  **Oracle stream (OTP 29 / erts-17.1):** `[line]` `printf 'a\nb\nc\n'` ⇒ `{data,{eol,"a"}}`,`{data,{eol,"b"}}`,`{data,{eol,"c"}}`; `printf 'a\nb'` ⇒ `{eol,"a"}` then `{noeol,"b"}` (the partial flushes at **child EOF**, as `noeol`); `[binary]` ⇒ plain `{data,<<…>>}` (no wrapper); `[binary,{line,4}]` over `abcdefgh\n` ⇒ `{noeol,<<abcd>>}` then `{eol,<<efgh>>}`; a blank line ⇒ `{data,{eol,[]}}`; the `{eol|noeol,_}` wrapper is applied in **both** list and binary mode; the split is state-dependent, not write-chunk-dependent (`\n` alone still completes the pending line as `eol`), and `port_close` does **not** flush (only child EOF does — `io.c:7211`).
- **`{fd,N}` spawn port holds an `Interest::BOTH` registration for its life.** `arm_output`/
  `disarm_output` are no-ops for `duplex` (`spawn.rs:913-941`), so the single adopted descriptor stays
  registered both ways after read EOF (`release_stdout` is a no-op for duplex). Harmless under the
  edge-triggered poller, but a spurious `OUTPUT` task is scheduled on every write-readiness event.
- **A command on a socket-less inet port is a typed `BeamError::BadArg` refusal with the queued bytes
  retained** — `inet/mod.rs::write_pending` (`connected_socket()?`, the C's `!IS_CONNECTED`) and its
  `udp.rs` counterpart (a `self.socket`-less `Err`, the C's `!IS_OPEN`), reached from both `output` and
  `ready_output`. Nothing is consumed, so a re-driven `OUTPUT` task refuses identically instead of
  losing data; the `Err` short-circuits `Port::check_write_accounting` and surfaces out of
  `PortRegistry::execute_port_tasks`, and `erlang:port_command/2` raises. An **open but unconnected**
  datagram port really sends (the destination rides the command's address field), so only a socket-less
  one refuses. The oracle's two shapes are a caller-ref-prefixed payload (answer `true`, keep the port
  alive, deliver `{error,enotconn}` out of band) and a prefix-less one (`init_caller` ⇒ the port dies
  through `driver_failure_posix(EINVAL)`). The second **is** expressible now
  (`DriverContext::signal_failure`); this driver does not take it — it answers the typed `BadArg`
  instead, because the first shape is still unreachable (it needs the C's `desc->caller`, i.e. a
  caller notion on the frozen `Driver`/`DriverContext` seam, not just the owner-addressed completion
  channel), and a refusal must not count bytes for a write that never happened. The measured oracle
  shapes are in `inet/CONTEXT.md` › *Known Issues*.
- **A driver closes its own port through the failure channel, never by panicking or returning an
  error.** `DriverContext::signal_failure(DriverFailure)` latches the C's `driver_failure*`/`driver_exit`
  request from inside a callback (exactly one latch; later deliveries from the same callback are
  refused, the C's `-1`); `PortRegistry::reap_failures` then tears the port down through the ordinary
  close path after the round, and the runtime renders the owner's reason from
  `PortRegistry::take_failures()` (`FailureReason::atom_name`/`code`, gated on
  `DriverFailure::signals_owner`). `src/port_failure.rs` carries the whole mapping; the loaded-C-driver
  half (the `driver_failure*`/`driver_exit` hooks) is `../abi/`'s.
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
children for `spawn`, loopback TCP/UDP + `erl`-oracle cross-checks for `inet`. Byte-framing is asserted
against the C `read_linebuf` cases in `line_buffer.rs`'s in-module suite. The `spawn` suite's
child-exit and end-of-file waits are **condition-based under a 60 s hang guard**, never a wall-clock
budget tuned for an idle host (see *Notes for Agents*).
`ram_file.rs`'s suite (the sibling `ram_file_tests.rs`, 23 tests) needs no OS resource: the reply
encodings, the mode/whence bit tables, the zero-filled-gap/truncate semantics, the argument faults
and the two transports' consume-what-you-report accounting are pure in-memory assertions driven
through a real `DriverContext`, plus a hard-coded matrix of the exact reply bytes the real
`ram_file_drv` produces. Two tests spawn the system `erl` (through the same `mpsc` + timeout helper
`inet/tests.rs` uses; a hung/absent `erl` skips **loudly**): one drives every command of the matrix
through a raw `open_port({spawn,"ram_file_drv"},[binary])` and compares **77 replies byte-for-byte**,
the other pins the high-level `ram_file:*` API shapes (`get_size`/`get_file`/`position`/`read`/
`pread`/`pwrite`/`truncate`/`sync`/`datasync`/`advise`/`allocate`/`ipread_s32bu_p32bu`/`close`).
The crate-level integration suites for this node are the sibling `../../tests/` ones: `inet.rs` (8), `udp_inet.rs` (5) and `ram_file.rs` (12).

## Routing Table
- `fd.rs` → `FdDriver` + `set_nonblocking` (the shared non-blocking wrapper)
- `efile.rs` → `EfileDriver`/`make_efile`/`FileDataMode`, the `open` grammar + `Access`/`OpenFlags`
- `spawn.rs` → `SpawnDriver`/`SpawnSpec`/`SpawnOptions`/`StreamOptions`/`build_command`
- `ram_file.rs` → `RamFileDriver`/`make_ram_file`/`DRIVER_NAME` + the `RAM_FILE_*` command/mode/whence/response
  constants (the in-memory command-driven file driver); its suite is the sibling `ram_file_tests.rs`
  (`#[cfg(test)] #[path = "ram_file_tests.rs"] mod tests;`, the `inet/` split pattern)
- `line_buffer.rs` → `LineBuffer`; `binary_io.rs` → `Phase`/`PhaseChange`
- `src/drivers/inet/` → the `tcp_inet` + `udp_inet` socket drivers; the option vocabulary
  (`opt.rs` — the five-byte/one-byte record walk over `Step`, the `SocketOptions` store, the
  `OptScope { stype, family, protocol }` gates — split into `opt_state.rs`'s driver-state codes and
  `opt_sys.rs`'s syscall-backed codes, incl. `linger`/`bind_to_device`/`raw`/`pktoptions` and the
  `propagate = 1` nine), `sys.rs`'s `get_socket_linger`/`set_socket_linger`/`get_socket_bytes`/
  `set_socket_bytes`, `adopt_probe.rs`'s `INET_REQ_FDOPEN` errno rendering, the numeric
  `erlang:port_control/3` opcode bridge (`opcode.rs` — the `0x03f1a300` magic, wrapping unsigned
  subtraction, the C's three switches), and `{packet, N}` framing (read + send path); the option
  surface is oracle-pinned byte-for-byte (child node `src/drivers/inet/CONTEXT.md`)
- `../abi/` → the driver C-ABI host layer (sibling child node, not a built-in driver)

## See Also
- `../CONTEXT.md` → the `beam_io` crate root: the frozen `Driver`/`DriverContext` seam, `PortRegistry`,
  reducer constants, the per-driver API surface and the driver handoffs.
- `../../CONTEXT.md` → crate-layer contracts, the A.4 driver C-ABI contract.
