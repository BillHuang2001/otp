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
- `fd.rs` → `FdDriver` (`"fd"`; `pipe`/`connected`/`read_only`/`write_only`, line/phase modes, the
  `line`/`noline`/`text`/`binary`/`getopts` control verbs — an unknown verb is `BadArg` — EOF
  de-registration, the `pub(crate)`
  read/write-slot seams `adopt_read_fd`/`release_read_fd`/`adopt_write_fd`/`release_write_fd`/
  `has_*_fd`/`attached_fd`), `set_nonblocking`; **inherits the default `Driver::flush`**,
  deliberately (see *The close path, per driver*).
- `efile.rs` → `EfileDriver` (`Driver::name() == "efile"`), `make_efile` (its `DriverFactory`),
  `FileDataMode`; the `open` flag grammar and `Access`
  translation reproduce the C's `efile_translate_modelist` + `get_flags`, and `open(2)` is retried on
  `EINTR` exactly as the C's `do { … } while (fd == -1 && errno == EINTR)` does
  (`retry_on_eintr`, `unix_prim_file.c:188-191`). A failed syscall ⇒ the raw `BeamError::Io` errno
  (the `{error,atom}` rendering is the runtime's `prim_file` floor); an unopened port / unparsable
  argument ⇒ `BadArg`.
- `spawn.rs` → `SpawnDriver` (`"spawn"` — the only module here that may spawn OS children),
  `SpawnSpec`/`SpawnOptions`/`StreamOptions`, `SpawnStream { Binary, List }` (**`Default = List`**, the
  `binary`/`list` `open_port` option; the **runtime** builds `{Port,{data,Data}}`), `build_command`
  (the term-free `open_port/2` shape — an empty command/path or an empty argv ⇒ `BadArg`), the
  deferred `fork_exec_child`, `ChildProc`/`ForkedChild`; constructed via `with_stream_options`,
  **really spawning in the constructor** and reaping eagerly (`take_exit_status()`/`take_eof()` once
  only). `stop` deregisters both directions, gives the child its end of file **before anything is
  killed** and only then reaps it (bounded), closing the descriptors last (see *The close path, per
  driver*); `Drop` does the same without the wait, and an adopted `{fd,N}` is **never** closed.
- `ram_file.rs` → `RamFileDriver` (`"ram_file_drv"`; a `Vec<u8>` "file", the C's **13 opcodes** and
  its `OK`/`NUMBER`/`DATA`/`ERROR` reply encodings), `make_ram_file`, `DRIVER_NAME` (aliased
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
  zero-fills the gap (the C's `ram_file_expand`). **Inherits the default `Driver::flush`** — the C
  entry is `flush = NULL`, so the port's own `Closing` drain is what runs a queued command at close
  (see *The close path, per driver*).
- `inet/` → the `tcp_inet`/`udp_inet` socket drivers. [`Driver::control`](crate::Driver::control) takes
  the C's packed request payload in **two** wire forms: a **decimal** command is
  `erlang:port_control/3`'s integer `Op`, forwarded by the runtime as its decimal string and decoded
  by `opcode.rs` (accumulated mod 2³², `wrapping_sub(CONTROL_MAGIC_NUMBER = 0x03f1_a300)`, then the
  C's three switches) onto the **same** verb body the named form uses, so the numeric and named
  routes answer identical bytes; any other command is a named verb. The **operand** decode stays the
  runtime's. A number no table entry matches — including a case of the other driver's switch, and
  every op below the magic (`"1"`, `"99"`) — answers the C's `[0]exbadport`; an opcode the C serves
  but this floor does not is the typed `Opcode::not_served()` unsupported raise. `inet/CONTEXT.md`
  carries the full served-vs-refused opcode table. A **control** verb on a socket-less or peerless
  port answers the C's control reply — `[0]enotconn` on a stream port (`tcp_inet`'s `send`/`recv`/
  `shutdown`/`accept`), `[0]einval` on a datagram one — never a typed refusal; the
  `erlang:port_command/2` write path is the separate, `init_caller`-shaped one of *Known Issues*
  below.
  `setopts`/`getopts` serve the C's whole option surface in two families — `opt_state.rs` (the codes
  the `inet`/`tcp`/`udp` descriptor keeps: `active`, `packet`, `mode`, `deliver`, the watermarks,
  `delay_send`, `send_timeout*`, `show_econnreset`, `read_ahead`, `read_packets`, `netns`,
  `line_delim`, the membership pair) and `opt_sys.rs` (everything that reaches
  `setsockopt(2)`/`getsockopt(2)`, incl. `linger`'s `struct linger` shapes, `bind_to_device`, `raw`,
  `pktoptions`, `multicast_if`) — so `prim_inet:accept_opts/3`, the `getopts`+`setopts` pair a
  `gen_tcp:accept/1` ends in, completes. Every code is byte-compared against the oracle per port
  state — no socket, open stream, open datagram, post-`close`, bound-and-listening — plus the
  datagram `post-close` twin (`opt_state_tests.rs`/`opt_sys_tests.rs`; `opt_oracle.rs` is the one
  shared harness).
  `SocketOptions::linger_zero` is a stored flag with **no** `getopts` code: it is the C's
  `TCP_ADDF_LINGER_ZERO` (`inet_drv.c:864`), latched by `opt_sys.rs` on the `INET_OPT_LINGER` set as
  `value != 0 && linger == 0` **before** the `setsockopt` — so a failing syscall (which `linger` does
  not propagate) still records it — and only under `scope.is_tcp_protocol()` (`:7382`-`:7386`).
  `SocketOptions::inherited_from` carries it to an accepted socket inside the C's flag mask
  (`:11829`-`:11831`), so a listener created with `{linger, {true, 0}}` hands the flag to the sockets
  it accepts.
  Its one reader is [`Driver::flush`](crate::Driver::flush)'s `discard_output` in `tcp_inet` (see
  *The close path, per driver*).
  `tcp_inet` also overrides [`Driver::flush`](crate::Driver::flush) — the only driver here that
  discards queued output — while `udp_inet` inherits the default.
- `line_buffer.rs` → `LineBuffer`, `LineFrame`, `DEFAULT_MAX_LINE_LEN = 1024` (the C's
  `LINEBUF_EOL`/`LINEBUF_NOEOL` marker rules and the `CR_AFTER` split); `binary_io.rs` → the
  `erl_child_setup` text⇄binary machine, `Phase { Text, Binary }` (**default `Binary`** = raw
  passthrough), `PhaseChange`.
- `mod.rs` → module wiring/re-exports only.

## The close path, per driver (the mechanism is `../CONTEXT.md`'s — this is only each driver's half)
`Driver::flush` is the **additive, defaulted** trait member the port layer's close path calls first, and it is the C's `drv->flush` (called from `flush_port`, `io.c:3659-3700`: the `flush != NULL` test at `:3664` and the call at `:3687`), which `erts_deliver_port_exit` reaches from its CLOSING arm (`io.c:3946-3954`, the `else` at `:3955` being `terminate_port`).
It is a driver's one chance to **discard** queued output — never a second write loop.
So the question a driver answers here is never "how do I write my queue at close?" (the port's `Closing` drain does that through the ordinary `OUTPUT` task → `ready_output` → `output` → `check_write_accounting`) but "may I throw any of it away?".
`Driver::stop` is the C's `terminate_port` (`io.c:3704-3795`, whose sole `drv->stop` call is at `:3752`) and is reached only once that queue **emptied**; otherwise the port parks in [`crate::PortState::Closing`](crate::PortState) — the C's Limbo, `io.c:3968-3971` — its deferred half being `erts_port_driver_callback_epilogue` (`erts/emulator/beam/erl_port.h:798-812`, "CLOSING and the ioq is empty ⇒ terminate") and this crate's `PortRegistry::finish_closing_ports`, while the forced teardown after a `driver_failure*` (`io.c:7220`'s `prt->ioq.size = 0`) runs `stop` with the remainder dropped.
A command on a `Closing` port is the C's `badarg` (`io.c:1440-1441`, `ERTS_PORT_SFLGS_INVALID_LOOKUP` includes `CLOSING`, `erl_port.h:351-353`).

### Which driver overrides `Driver::flush`, and what each does with the queue

| driver | `Driver::flush` | queued output at close | C counterpart |
|---|---|---|---|
| `tcp_inet` | **overridden** | **discarded** when write readiness is unarmed or `{linger, {true, 0}}` is in effect, written out otherwise | `tcp_inet_flush` (`inet_drv.c:12708`-`:12731`, the entry's `flush` slot at `:1368`) |
| `spawn` | **overridden** | kept; the override only latches `terminating` | `fd_flush` (`erts/emulator/sys/unix/sys_drivers.c:1029`-`1034`) |
| `fd` | inherited (default) | kept, written by the port layer's drain | `fd_flush` latches only (`sys_drivers.c:1029`-`1034`) |
| `ram_file_drv` | inherited (default) | kept; the drain executes the queued command | `flush = NULL` (`ram_file_drv.c:120`) |
| `udp_inet` | inherited (default) | kept, written by the port layer's drain | `flush = NULL` (`inet_drv.c:1406`-`:1443`) |

- `tcp_inet` — **overrides `Driver::flush`** (`inet/mod.rs`), the C's `tcp_inet_flush` (`inet_drv.c:12708`-`:12731`), and it is the one driver here that discards queued output.
  The whole decision is `discard_output = !(token.is_some() && interest.is_writable()) || options.linger_zero`, and a true answer runs `ctx.output().clear()` (the C's `tcp_clear_output`).
  `interest.is_writable()` is the C's `event_mask & FD_WRITE` half — the `Interest::WRITABLE`/`BOTH` that `write_pending` arms when a remainder is queued — and the `token.is_some()` conjunct models `erl_inet_close` (`inet_drv.c:5101`-`:5117`), whose two arms both zero the event mask where this floor instead deregisters the token away, so a closed or peer-closed descriptor counts as unarmed even though the last `Interest` snapshot still says writable.
  `options.linger_zero` is the C's `TCP_ADDF_LINGER_ZERO` arm (`:12714`-`:12716`): a zero-linger close aborts with an `RST`, so the queued bytes have nowhere to go.
  The discard is the whole effect: `tcp_clear_output`'s `send_empty_out_q_msgs` (`:11724`, the `INET_REQ_SUBSCRIBE` notification half) has no counterpart here, and `sendfile`'s forcing arm (`:12717`-`:12723`) is unreachable because `TCP_REQ_SENDFILE`(45) is refused outright.
  A retained queue is written by the port layer's `Closing` drain — the ordinary `OUTPUT` task → `ready_output` → `output` → `check_write_accounting` path.
- `tcp_inet` — **`stop` is `release_socket`, not `desc_close`.**
  `TcpInetDriver::stop` drains the pending async ops with `{error, closed}` and then calls `release_socket(ctx)`, because that is the C's `tcp_inet_stop` (`inet_drv.c:11895`-`:11924`: `tcp_close_check`, `tcp_clear_input`, `inet_stop`), which **never touches the `ioq`**.
  The unconditional discard is `tcp_desc_close`'s (`:11932`-`:11939`), and that body belongs to the `close` **verb** (`INET_REQ_CLOSE`, `:12258`-`:12266`) and to every peer-close teardown the C routes through it — reached here only from `TcpInetDriver::verb_close` via `desc_close`.
  Routing `stop` through `desc_close` would throw away a remainder the flush deliberately kept.
- `spawn.rs` — **overrides `flush`, latching only**: the C's `fd_flush` sets `dd->terminating = 1` and writes nothing (`erts/emulator/sys/unix/sys_drivers.c:1029`-`1034`; the "guarantee that things such as escripts … really have outputted all data before terminating" is documented at `:900`-`:907`).
  Its consumer is `SpawnDriver::stop`, which runs in order: deregister both directions (the taken `in_token`/`out_token`, each with its fd), `close_stdin_for_eof()` (the child's end of file — the effect the C `fd` driver reaches by closing the write descriptor in its own `stop`, `sys_drivers.c:1037`-`1052`), then `reap_after_stdin_eof(ctx)` when the flush latched `terminating` and `reap_child()` otherwise, and `close_fds()` last.
  `reap_after_stdin_eof` is a non-blocking `try_wait` loop bounded by `CHILD_EXIT_WAIT_MS` (250 ms) with a `CHILD_EXIT_POLL_MS` (5 ms) sleep between probes, draining the child's stdout on every iteration (`drain_stdout_for_exit`) so a `tee`-shaped child cannot block on a full stdout pipe with its stdin still unread; expiry takes the abrupt `reap_child()`.
  An unflushed teardown therefore takes the abrupt kill path, and a child that outlives the bound is a **documented divergence** (the oracle would leave it running for `erl_child_setup` to reap).
  The teardown deliberately does **not** route the C's `driver_failure_atom(port,"normal")` epilogue (`:1468`-`:1469`, the `ready_output` loop's exit once the queue emptied with the latch set): this floor's `stop` is synchronous — the port has already drained the queue before calling it — and a caller's `port_close/1` must not turn into an owner `{'EXIT',…}` message the oracle never sends.
  `Drop` gives the child its end of file and then takes the abrupt path without the wait (no `DriverContext`, and an arbitrary thread must not stall for the bound); a duplex `{fd,N}` port has no separate stdin to close and no child to wait for, so only its registration/descriptor teardown applies.
- `fd.rs` — **inherits the default, deliberately**: the C's `fd_flush` latches only and never writes (`sys_drivers.c:1029`-`1034`), its `ready_output` keeps writing while the port is `CLOSING` (`:1448`-`:1471`), and the port layer's `Closing` state plus its drain loop **is** that loop, so an override could only discard bytes the C keeps.
  `FdDriver::stop` is consequently reached only once the queue emptied (or from the forced `close_now` after a `driver_failure*`), which is why it can close the descriptors without rescuing anything; its rustdoc records the reasoning so nobody re-investigates.
- `ram_file.rs` — **inherits the default**, because the C entry is literally `flush = NULL` (`erts/emulator/drivers/common/ram_file_drv.c:120`), so `flush_port` calls nothing and the port's own drain runs the queued command through the driver's `output` slot: a command queued by `erlang:port_command/2` and never driven by a round still **executes** at close, its reply staged as a completed read.
  Because `output` consumes the whole payload the queue empties inside the drain, so this driver never parks — the parked half of the shape is unreachable from a `ram_file_drv` port.
- `udp_inet` — **inherits the default**, because its C entry's `flush` slot is NULL (`inet_drv.c:1406`-`:1443`, the position `tcp_inet`'s entry at `:1368` fills with `tcp_inet_flush`), so `flush_port`'s `flush != NULL` test (`io.c:3664`) calls nothing.
  A datagram port's queued commands are therefore written by the port layer's `Closing` drain and never thrown away at a close, which is the C's own asymmetry: `packet_inet_ctl`'s `INET_REQ_CLOSE` (`:14701`) discards nothing either, because a datagram write cannot block on a peer's window the way a stream write can.
  `UdpInetDriver::stop` is the C's `packet_inet_stop` (`:14533`-`:14549`): answer every pending operation with `{error, closed}`, then `release_socket`.

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
- **`FdDriver` is deliberately minimal.** Byte-stream only: line mode frames **input** but does **not**
  line-frame output; there are no `inet`/`forker` drivers here and no packet framing (that lives in
  `inet/`). The `{line,N}` message shape the oracle needs is applied at the **port**, not in this
  driver (next bullet).
- **`SpawnDriver` is the OS-child floor, not the whole `spawn` driver.** Synthesizing
  `{Port,{data,…}}`/`{Port,{exit_status,…}}`/`{Port,eof}` from its bytes, its latched EOF and its
  once-only exit status is the runtime's job. Writing to a child that closed its stdin fails with a
  typed `BeamError::Io` (the unwritten remainder stays queued); adoption needs a non-blocking
  descriptor, and a descriptor that refuses adoption (some inherited ones ⇒ `EPERM`) fails the open.
  Input is framed at the **port**, not in the driver (there is no `line` verb): `binary`/`list` rides
  `SpawnStream`, `[line]`/`[binary,{line,N}]` goes through `Port::set_line_mode` +
  `take_line_notifications`.
- **The C `ram_file_drv` answers its two reply families through two different output calls.** `OK`/`ERROR`/`NUMBER` go through `driver_output2` (a plain byte list) while `DATA` goes through `driver_output_binary` (an improper `[header | binary]` iolist on a `[binary]` port), so an oracle probe that reads the raw port message must normalise with `binary_to_list(iolist_to_binary(D))` before comparing — and `io:format("~p", [D])` on the raw term raises. This crate keeps that split: `RamReply { header, body }` is delivered through [`DriverContext::deliver_read`](crate::DriverContext::deliver_read), so `OK`/`ERROR`/`NUMBER` are header-only (`body: None`) and a `DATA` reply is the 5-byte `[RESP_DATA, len:i32be]` header plus `Some(payload)` — an empty payload included (`Some(vec![])`, the C's `driver_output_binary(port, hdr, 5, bin, 0, 0)`).
- **`ram_file_drv` is a registered built-in with a live runtime path.** `DriverRegistry::with_builtins()`
  (in `../driver.rs`) registers `"fd"`/`"efile"`/`"tcp_inet"`/`"udp_inet"`/`"ram_file_drv"`, and
  `beam_rt` opens a port on it through `open_port({spawn_driver,"ram_file_drv"}, …)` (its
  `src/port/ram_file_matrix_tests.rs` cross-checks this driver's whole command/reply matrix against the
  live oracle). The port's reserved `"close"` is the teardown op for a raw `open_port` port; the
  driver's own `"close"` verb exists for a direct caller (it resets the file and answers the C's `OK`),
  and [`Driver::stop`](crate::Driver::stop) resets the same state without a reply.
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
  `PortNotification::Data` one; `take_eof` runs the `driver_eof` flush, and a close never flushes the
  **input** line buffer (the close path's `Driver::flush` is the unrelated output-side seam — see *The
  close path, per driver*).
  `beam_rt` adopts the whole chain (its `open_port/2` option parsing, the `{Port,{data,{eol|noeol,Data}}}`
  rendering and the per-round drain); the pinned oracle table is cross-checked byte-for-byte in
  `tests/spawn.rs::line_framing_matches_the_oracle`.
  **Oracle stream (OTP 29 / erts-17.1):** `[line]` `printf 'a\nb\nc\n'` ⇒ `{data,{eol,"a"}}`,`{data,{eol,"b"}}`,`{data,{eol,"c"}}`; `printf 'a\nb'` ⇒ `{eol,"a"}` then `{noeol,"b"}` (the partial flushes at **child EOF**, as `noeol`); `[binary]` ⇒ plain `{data,<<…>>}` (no wrapper); `[binary,{line,4}]` over `abcdefgh\n` ⇒ `{noeol,<<abcd>>}` then `{eol,<<efgh>>}`; a blank line ⇒ `{data,{eol,[]}}`; the `{eol|noeol,_}` wrapper is applied in **both** list and binary mode; the split is state-dependent, not write-chunk-dependent (`\n` alone still completes the pending line as `eol`), and `port_close` does **not** flush the line buffer (only child EOF does — `io.c:7211`).
- **`{fd,N}` spawn port holds an `Interest::BOTH` registration for its life.** `arm_output`/
  `disarm_output` are no-ops for `duplex` (`spawn.rs:978-1006`), so the single adopted descriptor stays
  registered both ways after read EOF (`release_stdout` is a no-op for duplex). Harmless under the
  edge-triggered poller, but a spurious `OUTPUT` task is scheduled on every write-readiness event.
- **The `port_command/2` write path reproduces the C's `init_caller` split, failure kill included.**
  Both inet drivers run `init_caller`/`init_caller_iov` (`inet_drv.c:1820`-`:1890`) on every queued
  command **before** they look at the socket, with the C's **size-only** rule: the first two bytes
  are a big-endian 16-bit tag length `n`, `n` tag bytes must follow, and the tag's content is never
  inspected (so a *plain* payload reads as a huge length). The strip runs **once per command**, never
  per short write, and the four rows are each oracle-measured. Once stripped, a command either writes
  and stages `{inet_reply, Port, ok|{error,Reason}, Tag}` on the completion channel or is answered out
  of band:
  (a) **malformed** (too short for the length word, or an `n` that does not fit) ⇒ the queue is
  consumed and charged (`prt->bytes_out += size`), nothing is written, and
  `DriverContext::signal_failure(DriverFailure::posix(EINVAL))` is latched — the C's
  `driver_failure_posix(EINVAL)` (`:12634`/`:12667` tcp, `:15063` udp) — so `PortRegistry`'s
  `reap_failures` closes the port after the round: `erlang:port_command/2` still answers `true`, the
  owner's exit reason is `einval`, and every later command is the oracle's `badarg`;
  (b) a **well-formed** prefix on a socket-less or unconnected port ⇒ consumed and answered out of
  band with `InetReply::Error(ENOTCONN)` for `tcp_inet` (`:12674`-`:12683`), and on a **socket-less**
  `udp_inet` with `Error(EINVAL)` (its `!IS_OPEN` arm, `:15068`-`:15077`) — an open-but-unconnected
  datagram port really sends, since the destination rides the command — the port **staying alive**;
  `udp_inet`'s operand grammar answers `Error(EINVAL)` the same way for its own `return_einval`
  refusals (a truncated ancillary-length word);
  (c) an **empty** output queue (`init_caller` would read `size < 2`) is `Ok(0)` and latches
  nothing, so a re-driven `OUTPUT` task after a finished write cannot kill the port;
  (d) a tag that is **empty** or not ETF-led is consumed but stages **no** reply
  (`echoable_caller_tag` — the C's `erl_drv_send_term` cannot decode it).
  Residuals, each a typed error rather than a wrong result: `udp_inet`'s **ancillary data**
  (`anc_len != 0`, the C's `sendmsg`-with-cmsgs arm) is the typed `unsupported` with the command
  left queued, and a tag of **≥ 256 bytes** would be read as a plain command (impossible for
  `prim_inet:send/3`, whose `term_to_binary/1` reference is tens of bytes).
- **A driver closes its own port through the failure channel, never by panicking or returning an
  error.** `DriverContext::signal_failure(DriverFailure)` latches the C's `driver_failure*`/`driver_exit`
  request from inside a callback (exactly one latch; later deliveries from the same callback are
  refused, the C's `-1`); `PortRegistry::reap_failures` then tears the port down through the ordinary
  close path after the round, and the runtime renders the owner's reason from
  `PortRegistry::take_failures()` (`FailureReason::atom_name`/`code`, gated on
  `DriverFailure::signals_owner`). `src/port_failure.rs` carries the whole mapping; the loaded-C-driver
  half (the `driver_failure*`/`driver_exit` hooks) is `../abi/`'s. Adopted in this node: `tcp_inet`
  and `udp_inet` latch `posix(EINVAL)` for a malformed `port_command/2` (the bullet above), so that
  path never answers a typed `BadArg` where the oracle kills or keeps the port.
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
  shape makes those two cases flaky under load — a starved host needs just over 5 s to schedule, run
  and reap the child. Keep the `EINTR` retry in the poll loop (`SIGCHLD` deliveries make it a normal
  outcome).
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
**Per-module in-module counts (`-p beam_io --lib`): `drivers` 727** — `inet` 603 (per-suite breakdown in `src/drivers/inet/CONTEXT.md`), `ram_file` 34 (23 + the 6 in `ram_file_split_tests.rs` + the 5 in `ram_file_close_tests.rs`), `spawn` 27 (23 + the 4 in `spawn_teardown_tests.rs`), `line_buffer` 24, `fd` 17 (14 + the 3 in `fd_close_tests.rs`), `efile` 13, `binary_io` 9; the crate-wide gate is **1328 passed / 0 failed / 2 ignored over 28 binaries**.
Each driver's **close-path half is a sibling test module of its own source half** (`super` is the driver module) so the source file stays near its size target:
- `spawn_teardown_tests.rs` (4) pins the **teardown ordering** over real children and a payload larger than a pipe buffer (512 KiB): `stop` gives the child end of file before anything is killed, a closed port's whole queued command really reaches the child, a child that never reads stdin does not hang the close (the bound, the documented divergence), and the same row cross-checked against the system `erl` (`erl` absent ⇒ skips **loudly**); every wait polls a condition under the 60 s hang guard.
- `fd_close_tests.rs` (3) pins an `FdDriver` close over a real `UnixStream::pair`: a short write parks the close **before** `stop` (the parked port still owns its write fd), the parked remainder is delivered in full by the rounds, and `a_command_queued_before_a_close_is_written_by_the_close` pins the writing that the inherited default `flush` leaves alone. Hermetic (no `cc`, no `erl`, no child).
- `ram_file_close_tests.rs` (5) pins that a queued command **executes** in the close drain: the command's reply, a `write`'s `NUMBER` reply, a `DATA` reply keeping its header/body split, the port finishing inside the `close` call (nothing left to park — `is_closing` is the `badarg` for the gone port), and several commands queued without a round arriving as **one** payload. Hermetic.
The `tcp_inet`/`udp_inet` close half is the **inet node's**, not this one's: `drivers/inet/flush_tests.rs` (6 — the `discard_output` matrix over real loopback sockets, incl. `udp_inet`'s surviving queue) and `drivers/inet/write_then_close_tests.rs` (5 — a write-then-close burst delivering its data before `Closed`).
`ram_file.rs`'s other suites (the sibling `ram_file_tests.rs`, 23 tests, plus the reply-split pin
`ram_file_split_tests.rs`, 6) need no OS resource: the reply
encodings, the mode/whence bit tables, the zero-filled-gap/truncate semantics, the argument faults
and the two transports' consume-what-you-report accounting are pure in-memory assertions driven
through a real `DriverContext`, plus a hard-coded matrix of the exact reply bytes the real
`ram_file_drv` produces. Two tests spawn the system `erl` (through the same `mpsc` + timeout helper
`inet/tests.rs` uses; a hung/absent `erl` skips **loudly**): one drives every command of the matrix
through a raw `open_port({spawn,"ram_file_drv"},[binary])` and compares **77 replies byte-for-byte**,
the other pins the high-level `ram_file:*` API shapes (`get_size`/`get_file`/`position`/`read`/
`pread`/`pwrite`/`truncate`/`sync`/`datasync`/`advise`/`allocate`/`ipread_s32bu_p32bu`/`close`).
The crate-level integration suites for this node are the sibling `../../tests/` ones: `inet.rs` (10), `udp_inet.rs` (6) and `ram_file.rs` (12).

## Routing Table
- `fd.rs` → `FdDriver` + `set_nonblocking` (the shared non-blocking wrapper); **inherits the default
  `Driver::flush`** (the port's `Closing` drain is the C's `ready_output` loop) — its close half is the sibling
  `fd_close_tests.rs` (`#[cfg(test)] #[path = "fd_close_tests.rs"] mod close_tests;`, 3 tests)
- `efile.rs` → `EfileDriver`/`make_efile`/`FileDataMode`, the `open` grammar + `Access`/`OpenFlags`
- `spawn.rs` → `SpawnDriver`/`SpawnSpec`/`SpawnOptions`/`StreamOptions`/`build_command`,
  `CHILD_EXIT_WAIT_MS`/`CHILD_EXIT_POLL_MS`; one of the two `flush` overriders here — it latches the
  C's `fd_flush` `terminating` and keeps the queue — feeding `stop`'s
  end-of-file-then-bounded-wait ordering; its teardown half is the sibling `spawn_teardown_tests.rs`
  (`mod teardown_tests;`, 4 tests)
- `ram_file.rs` → `RamFileDriver`/`make_ram_file`/`DRIVER_NAME` + the `RAM_FILE_*` command/mode/whence/response
  constants (the in-memory command-driven file driver; **inherits the default `flush`** — the C entry
  is `flush = NULL`, so the close drain runs the command through `output`); its suites are the siblings
  `ram_file_tests.rs` (`#[cfg(test)] #[path = "ram_file_tests.rs"] mod tests;`, the `inet/` split
  pattern), `ram_file_split_tests.rs` (`mod split_tests;`, the `header`/`body` pin per reply shape) and
  `ram_file_close_tests.rs` (`mod close_tests;`, 5 tests — a queued command executing in the close drain)
- `line_buffer.rs` → `LineBuffer`; `binary_io.rs` → `Phase`/`PhaseChange`
- `src/drivers/inet/` → the `tcp_inet` + `udp_inet` socket drivers; the option vocabulary
  (`opt.rs` — the five-byte/one-byte record walk over `Step`, the `SocketOptions` store, the
  `OptScope { stype, family, protocol }` gates — split into `opt_state.rs`'s driver-state codes and
  `opt_sys.rs`'s syscall-backed codes, incl. `linger`/`bind_to_device`/`raw`/`pktoptions` and the
  `propagate = 1` nine), `sys.rs`'s `get_socket_linger`/`set_socket_linger`/`get_socket_bytes`/
  `set_socket_bytes`, `adopt_probe.rs`'s `INET_REQ_FDOPEN` errno rendering, the numeric
  `erlang:port_control/3` opcode bridge (`opcode.rs` — the `0x03f1a300` magic, wrapping unsigned
  subtraction, the C's three switches), `{packet, N}` framing (read + send path), and `tcp_inet`'s
  `flush`/`stop`/`desc_close` split (`mod.rs` — the only `flush` override that discards, and a
  `stop` that is `release_socket`, never `desc_close`; `flush_tests.rs` 6,
  `write_then_close_tests.rs` 5); the option surface is oracle-pinned byte-for-byte (child node
  `src/drivers/inet/CONTEXT.md`)
- `../abi/` → the driver C-ABI host layer (sibling child node, not a built-in driver)

## See Also
- `../CONTEXT.md` → the `beam_io` crate root: the frozen `Driver`/`DriverContext` seam (incl. the
  additive `Driver::flush`), `PortRegistry`, reducer constants, *The close path* (flush → drain → `stop`,
  the park, `CLOSING_DRAIN_STALL_LIMIT`), the per-driver API surface and the driver handoffs.
- `../../CONTEXT.md` → crate-layer contracts, the A.4 driver C-ABI contract.
