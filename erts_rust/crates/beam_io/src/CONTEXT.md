# `beam_io/src` — crate source root (module wiring)

## Intent
Source root of `beam_io` (the I/O subsystem of the Rust ERTS rewrite) plus the three sub-nodes that
carry the bulk of the code. The parent `../CONTEXT.md` carries the crate's frozen API surface,
constraints, design decisions and cross-crate handoffs — read it first. This file adds the
**port-level mechanism** (line framing, the write path and the busy decision, the completion and
`{active,…}` channels, the flush-then-close path), the per-module in-module test counts and the
oracle-pinned shape tables, and the durable gotchas; the parent routes here for that detail.

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
  are what the out-of-scope `drivers/binary_io.rs` still uses; both views are the same byte sequence,
  with the frame marker as the only difference.
- `port_task.rs` → `LineNotification { port, header, bytes, eol }` — a **separate** struct, deliberately
  **not** a third `PortNotification` variant (a line-mode port's input never rides the raw
  `{Port, {data, Data}}` channel, and `PortNotification` keeps exactly its two variants so an
  exhaustive consumer stays exhaustive). Unlike `Data`, an **empty payload is a real frame**
  here — a blank line is `{data, {eol, []}}`. `header` is the `driver_output2` header of the read
  that **completed** the line (a per-frame cons prefix, empty for a plain read or an EOF flush).
- `port.rs` → `Port::set_line_mode(Option<usize>)` / `line_mode()` (the `SFLG` bit + `linebuf->max_len`;
  `Some(0)` = no length limit), `Port::flush_linebuf()` (the C `flush_linebuf_messages`), and
  `take_input_notifications()` (raw mode: **one `Data` per completed read message**; line mode:
  **no** notification, the frames are staged and drained by `take_line_frames()`).
  `Port::take_eof()` is the crate's **`driver_eof` observation point** and runs the flush.
- `registry.rs` → `PortRegistry::set_line_mode(id, Option<usize>)` / `flush_linebuf(id)` (the runtime's
  adoption seams) plus `take_line_notifications()` (drains every port's staged frames in port-number
  order — the accessor style of `take_eof`/`take_closed_notifications`); `execute_port_tasks`
  harvests every pending read of every port and its `notifications` carry only
  `PortNotification`s, so one round can yield several `Data` (one per `driver_output*` call,
  `io.c:3363-3448`) and a single read can stage many frames (597 bytes at `{line,4}` ⇒ 150, on the
  line channel).

### A driver's write path — the reply round trip, the busy decision and a refused command
`Port::command` owns the **busy-port decision table** (`PortCommandOutcome::{Done, Busy}`, or the typed
`notsup` for `[force]` on a driver without `DriverFlags::SOFT_BUSY`) and `PortRegistry::port_command_with`
is its registry surface (`port_command` keeps the `Result<()>` shape and is
`port_command_with(..).map(|_| ())`): only a `Done` queues the bytes, and a `Busy` answer leaves the
queue and the task list untouched. The queued write belongs to the round
(`execute_port_tasks` → `run_scheduled_tasks` → `Port::run_task(Output)` →
`Port::ready_output`, whose `Driver` default forwards to `output`). Whatever a driver pushes onto
`ctx.input()` — a built-in driver's `port_command` reply, or a `driver_output` made from
`start`/`control`/`timeout`/`output` — is harvested for **every** live port after the round and
reported as **one `PortNotification::Data` per completed read message**, drained once per byte (no
double report on a later round; a port in `{line, N}` mode reports through the separate
`LineNotification` channel instead).
`DriverContext::deliver_read(header, body)` is the same path for a **split** read (the C's
`driver_output2`/`driver_output_binary`/`driver_outputv`, whose `(hbuf, hlen, buf, len)` argument
group is what the split preserves): one `ReadMessage { header, body }`, the
header kept apart from the body because a `[binary]` port renders `[H1,…,Hn | <<Body>>]` while a
`[list]` port flattens both into ints. `body: None` is the C's `buf == NULL` (no binary tail at
all), `Some(vec![])` a present empty body (`<<>>` tail). A `Data` payload is therefore legitimately
empty: an explicit empty read IS the oracle's `{Port,{data,[]}}`. A `ram_file_drv` port needs no descriptor, so the whole chain is testable with no
child process and no poller wait.
The **outbound** half of a write is the driver's: a `tcp_inet` port prepends the `{packet, 1|2|4}`
length header on **both** write routes (the named `send` verb and the `port_command/2` one, i.e.
`Port::run_task(Output)` → `Driver::output`/`ready_output` → `write_command`) and charges header +
payload to `send_oct`; `udp_inet` writes its datagrams verbatim. The rules and their oracle-measured
pins are `drivers/inet/CONTEXT.md`'s `write_frame_tests.rs` (12).
A driver that **refuses** a write (`Err(BeamError::BadArg)` from `output`/`ready_output`, e.g. an
`inet` port whose socket is not open) is surfaced as that error: it short-circuits
`Port::check_write_accounting` (`result?` runs first, so the driver's reported byte count is never
inspected) and propagates out of `PortRegistry::execute_port_tasks` (`Err` — **not** a
`PortTaskOutcome`). Because the command was queued before it was driven, a refused command's bytes
stay in the output queue and the failing task is already popped, so a later round neither retries
nor re-reports it. A loaded C driver cannot refuse at all (`output`/`outputv` are `void`,
`prt->bytes_out += size`), so this shape is Rust-side: the runtime must read the error as "the
command was refused" and render the oracle's `badarg`.

### The driver→owner completion channel (`completion.rs` — the C's `driver_send_term` path)
A socket driver owes its owner **messages** that cannot be a return value: the
`{inet_reply, Port, Reply[, Tag]}` answer to `erlang:port_command/2` (the reply to a blocked
`gen_tcp:send/2` — `inet_reply_am`/`inet_reply_error_am`/`inet_reply_finish`,
`../../../erts/emulator/drivers/common/inet_drv.c:2617-2740`) and the
`{inet_async, Port, Id, Result}` completion of a queued asynchronous operation
(`send_async_ok`/`send_async_ok_port`/`send_async_error`, `:2493-2562`; teardown flushes every
queued op with `{error,closed}` via `async_error_am_all`, `:2597-2608`). This is a **third side
channel**, built exactly like the two precedents above and deliberately **not** a
`PortNotification` variant (that enum keeps its two variants so an exhaustive consumer stays
exhaustive):
- `completion.rs` → `CompletionEvent` (`InetReply { caller_ref: Option<Vec<u8>>, reply: InetReply }`
  / `InetAsync { id: u16, result: InetAsyncResult }`), `InetReply { Ok | Error(InetReason) }`,
  `InetAsyncResult { Ok | Data(ReadMessage) | Accepted { fd } | Port(PortId) | Error(InetReason) }`
  (`Data` keeps the crate's header/body read split, `Accepted` hands an accepted descriptor over
  **owned** — the driver never stores it, so no teardown path can close it),
  `InetReason { Closed | Timeout | Errno(i32) }` (+ `from_errno`, `atom_name` — the lowercase POSIX
  name via `error::errno_atom`, with the C's `errno_<N>` fallback for an errno the table does not
  name), and the port-stamped `PortCompletion { port, event }`. Term-free: `caller_ref` is the
  caller's `term_to_binary(Mref, [local])` image carried **verbatim** (a 131-led ETF binary the
  runtime decodes with `beam_codec`), never a decoded term.
- `driver.rs` → `DriverContext::deliver_completion(event)` (+ the chainable
  `with_completions(&mut Vec<PortCompletion>)`, the completion counterpart of `with_reads`). It
  **stamps `ctx.port()` itself**, so a driver cannot attribute a completion to another port, and a
  context with no sink answers the typed `unsupported` naming the missing sink (never a silent drop:
  the owner may be blocked waiting).
- `port.rs` → the `completions: Vec<PortCompletion>` stage + `Port::take_completions()` (drains once).
  The sink is installed at **every** driver interaction point — `open`(`start`), `output`,
  `ready_input`, `ready_output`, `control`, `timeout`, `drive_async`, `notify_exit`(`port_exit`) and
  close (`flush` **and** `stop`, see *The close path* below); the `timeout` one matters most, because
  `Port::run_task`'s `Timeout` arm discards `Driver::timeout`'s `Result`, so a timer-driven
  `{error,timeout}` reply exists only as a completion.
- `registry.rs` → `PortRegistry::take_completions()` drains every live port in **port-number order**
  (each port's own completions in delivery order), then the completions harvested from ports closed
  since the last drain. When a port **leaves the live map** (`remove_finished_port`, not necessarily
  the `close` call itself — a parked closing port stays live and keeps reporting, see *The close
  path*) its staged completions are **harvested** into a registry-owned queue, so a teardown's
  `async_error_am_all` replies (and any staged before it) still reach the owner exactly once; the
  port **number** keeps its existing rule — retired only by the `Closed` report, never by a
  completion.
Contract: **drain after every round** (line-frames style), a control op's return value is never a
completion, and each completion is reported exactly once.

### The driver→owner `{active, …}` input channel (`port_task.rs`'s `ActiveInput`)
An `{active, true|N|once}` socket owes its owner **incoming-message** notifications that are
neither `{Port, {data, …}}` (the passive read path) nor the answer to a call (a completion): the
`{tcp, S, Data}` of `tcp_message` (`inet_drv.c:3987`), its `{packet,N}` shape
`packet_binary_message` (`:4106`), `{tcp_passive, S}`/`{udp_passive, S}` from
`packet_passive_message` (`:4215`), and the `{tcp_error, S, Reason}`/`{udp_error, S, Reason}` of
`tcp_error_message` (`:4079`)/`packet_error_message` (`:4245`). Built exactly like the two precedents (separate struct, separate
drain — `PortNotification` keeps its two variants and `CompletionEvent` gains no variant):
- `port_task.rs` → `ActiveInput { port, event }` with `ActiveInputEvent { Data(ReadMessage),
  Passive, Error(InetReason) }`. `Data` is one delivered input with the header/body split kept
  (`header` = the `{packet,N}` framing header of a stream port, empty for `{packet,0}`, or the
  7-byte packed source address of a datagram port), never merged with another and never also
  reported as a `Data` notification; an empty body is still a real delivery. `Passive` is atom-free
  — the runtime picks `{tcp_passive,_}` vs `{udp_passive,_}` from the port's driver name. `Error`
  carries an `InetReason` and is likewise name-free — the runtime picks `{tcp_error,_}` vs
  `{udp_error,_}` the same way and renders the atom with `InetReason::atom_name()`; it is
  delivered **before** the close it precedes, and a datagram port emits it without any close.
- `driver.rs` → `DriverContext::deliver_active_input(event)` (+ the chainable
  `with_active(&mut VecDeque<ActiveInput>)`). It **stamps `ctx.port()` itself**, answers the typed
  `unsupported` with no sink, and refuses after a failure latched in the same callback — the same
  rules `deliver_read`/`deliver_completion` follow.
- `port.rs` → the `active_inputs: VecDeque<ActiveInput>` stage + `Port::take_active_inputs()`. The
  sink is installed at **every** driver interaction point (`open`/`output`/`ready_input`/
  `ready_output`/`control`/`timeout`/`drive_async`/`notify_exit`/`close`), like the other three.
- `registry.rs` → `PortRegistry::take_active_inputs()` drains every live port in **port-number
  order** (each port's own deliveries in delivery order), then the deliveries harvested from ports
  closed since the last drain (`close` copies them into `closed_active` before dropping the `Port`).
  Contract: **drain after every round and after every `close`**; draining is what removes them.
  Closed-port numbers keep the existing rule (retired only by the `Closed` report).
- **Who produces on it:** the `tcp_inet`/`udp_inet` drivers' `{active,…}` arms (their node), and
  `beam_rt` renders the messages (`{tcp,S,Data}`/`{udp,S,IP,Port,Data}`/`{tcp_passive,S}`/
  `{udp_passive,S}`). This node owns only the channel.
### The `{active, …}` state seam (`port_task.rs`'s `ActiveMode`/`ActiveSpend` — driver-agnostic)
The **read** half and the **notification** half of the same state machine the channel above reports
deliveries on: a driver owns the `{active, false|true|once|N}` mode (the C's `inet.active` +
`inet.active_count`) and spends one grant per delivered packet inside its own option store
(`INET_CHECK_ACTIVE_TO_PASSIVE`, `inet_drv.c:1552-1560`, whose deselect half is `:13106-:13114`).
Without this seam a runtime can only mirror the `setopts` walk, and can never learn the running
`{active, N}` counter nor that an `{active, once}` grant was spent (the C clears it **silently**,
sending no `{tcp_passive,_}`).
The **value types** are driver-agnostic (they live in `port_task.rs` and the two `Driver` members
carry defaults), so a driver that knows nothing about `{active,…}` needs no code; the two socket
drivers publish their own state through them (see *Who overrides the seam* below).
- `port_task.rs` → `ActiveMode { False, True, Once, Multi { remaining: i16 } }`
  (`Copy + Clone + PartialEq + Eq + Debug`, `Default = False`) with
  `ActiveMode::from_parts(active: i32, active_count: i16) -> ActiveMode` — the **single** place the
  driver store's numeric encoding lives (`1` ⇒ `True`, `2` ⇒ `Once`, `3` ⇒ `Multi` carrying the
  counter; anything else, including `INET_PASSIVE` `0`, ⇒ `False`) — plus `is_active()` (the C's
  `active > 0` test on the **mode** field, so even a hand-built `Multi { remaining: 0 }` is active)
  and `remaining() -> Option<i16>` (`Some` only for `Multi`).
- `port_task.rs` → `Multi { remaining }` is the **running** counter (`active_count`, what is left),
  never the granted `N`; a zero counter together with an active mode is a store state the driver
  never holds, because the decrement and the fall back to `False` happen in one step.
- `port_task.rs` → `ActiveSpend { Once, Multi }` (`Copy + Clone + PartialEq + Eq + Debug`): `Once`
  is a spent `{active, once}` grant (the silent flip), `Multi` an `{active, N}` counter that reached
  zero (whose `{tcp_passive,S}` already rides the channel above as `ActiveInputEvent::Passive`);
  `True`/`False` have no variant, because they are never spent.
- `driver.rs` → two **additive, defaulted** members: `Driver::active_mode(&self) -> ActiveMode`
  (default `ActiveMode::False`) and `Driver::take_active_spends(&mut self) -> Vec<ActiveSpend>`
  (default `Vec::new()`, which allocates nothing, so the "nothing was spent" path is free).
  Taking consumes, exactly like the crate's other drains.
- `port.rs` → `Port::active_mode(&self) -> ActiveMode` and
  `Port::take_active_spends(&mut self) -> Vec<ActiveSpend>` — plain forwards to the driver, unstamped.
- `registry.rs` → `PortRegistry::active_mode(&self, port: PortId) -> Result<ActiveMode>` (the
  `badarg` every other registry seam answers for an unknown id) and
  `PortRegistry::take_active_spends(&mut self) -> Vec<(PortId, ActiveSpend)>`.
  The drain reports every live port in **port-number order** (each port's own spends in the order its
  driver spent them), then the spends harvested from ports closed since the last drain (`close` copies
  them into `closed_active_spends` after the driver's `stop`, so a spend recorded before **or during**
  the teardown is reported).
- `registry.rs` → contract: **drain after every round and after every `close`**, every spend reported
  **exactly once**.
  The closed half exists because a delivery staged for a port that then closed still reaches the owner
  (`take_active_inputs`), so the spend that produced it must be reportable too; the number keeps the
  existing rule (retired only by the `Closed` report).
- **Who overrides the seam:** both socket drivers do — `TcpInetDriver` and `UdpInetDriver` read
  `ActiveMode::from_parts(self.options.active, self.options.active_count)` and record a spend at
  **one** site each, the per-delivery spend site (TCP `count_delivery`, the datagram twin), never on
  the `setopts`-walk-to-zero path (which still emits its `ActiveInputEvent::Passive` but spends no
  grant). Every other built-in inherits the trait defaults (`ActiveMode::False`, no spends), which is
  the honest answer for a driver with no active mode. The driver-level pins are
  `drivers/inet/active_seam_tests.rs` (9) and the child node's `src/drivers/inet/CONTEXT.md`.

### The close path — flush, drain, then `stop` (the C's `ERTS_PORT_SFLG_CLOSING`)
A close does not drop queued output: `Port::close` has the C's **flush-then-close** shape, so
`erlang:port_command(P, Data)` followed immediately by `erlang:port_close(P)` still delivers `Data`
(measured on the oracle: a `tee` child received 500 000/500 000 bytes while `port_close/1` returned
`true` with 454 656 bytes still queued — the drain is asynchronous, **not** part of the close's
return). The C: `erts_deliver_port_exit` clears `EXITING`, sets `ERTS_PORT_SFLG_CLOSING` and calls
`flush_port` instead of `terminate_port` when the ioq is non-empty and the reason is not `kill`
(`io.c:3946-3953`, `flush_port(prt)` at `:3952`); `flush_port` calls the driver's `flush` **only if
the entry has one** (`io.c:3664` the slot test, `:3687` the call) and
terminates **only if the ioq became empty** (`io.c:3696-3699`); `terminate_port` (`io.c:3704-3795`)
is the sole caller
of `drv->stop` (`io.c:3752`).
- `driver.rs` → `Driver::flush(&mut self, ctx: &mut DriverContext<'_>) -> Result<()>`, the
  **additive, defaulted** seam: the driver's one chance, *before* `stop`, to take bytes off
  `ctx.output()` and discard them, or (returning `Ok(())` without consuming anything) to say "keep
  them, try to write them". The default `Ok(())` discards nothing — the C's `flush == NULL` case
  (`io.c:3664`) — so a driver that consumes its output synchronously needs no override.
  No byte count is verified (the C's `flush` returns `void`); consuming what is discarded *is* the
  accounting rule. Overriders: `drivers/spawn.rs`'s `SpawnDriver` latches the C's `fd_flush`
  `terminating` flag (`sys_drivers.c:1029-1034`) and its `stop` then gives the child end-of-file
  and waits for it instead of killing it; `abi::DriverPort` forwards the loaded C driver's own
  entry through `DriverPort::call` (all four sinks installed, an absent slot the C's silent
  no-op) and deliberately does none of `stop`'s teardown work — a flush is not a teardown;
  `drivers/inet/mod.rs`'s `TcpInetDriver` reproduces `tcp_inet_flush` (`inet_drv.c:12708`-`:12730`,
  the entry's slot at `:1368`): it discards the queued output (`ctx.output().clear()`) exactly when
  the port is not write-armed-and-tokenized or the socket carries `SocketOptions::linger_zero`
  (`discard_output = !(desc->inet.event_mask & FD_WRITE) | TCP_ADDF_LINGER_ZERO`), and keeps the
  bytes for the port layer's `Closing` drain otherwise — the `token.is_some()` term of that test
  models `erl_inet_close` (`inet_drv.c:5101`-`:5117`) zeroing the C event mask, where this floor
  deregisters instead.
  Three built-ins deliberately do **not** override it, each the C's own `flush` slot: `udp_inet`
  (a `NULL` slot, `inet_drv.c:1406`-`:1443`), `ram_file_drv` (a `NULL` slot,
  `ram_file_drv.c:120`) and `fd`, whose `fd_flush` (`sys/unix/sys_drivers.c:1029`-`:1034`) only
  latches `terminating` and never clears the ioq — so their queued output is always the port
  layer's drain to write.
- `drivers/inet/mod.rs` → the **`tcp_inet` stop/close split**: `TcpInetDriver::stop` calls
  `release_socket`, **not** `desc_close`, because the C's `tcp_inet_stop` (`inet_drv.c:11895`-`:11924`)
  never touches the ioq — the unconditional `tcp_clear_output` of `tcp_desc_close`
  (`:11932`-`:11939`, `tcp_clear_input` then `tcp_clear_output` then `erl_inet_close`) belongs only
  to the close **verb** (`INET_REQ_CLOSE`, `TcpInetDriver::verb_close` → `desc_close`). Queued
  output is therefore the flush's business and the port layer's, never the stop's, pinned by
  `drivers/inet/flush_tests.rs` (`the_teardown_never_clears_the_queue_but_the_close_verb_does`).
- `drivers/inet/mod.rs` → the **read-path ordering rule** (owned by the inet node — see
  `drivers/inet/CONTEXT.md`): a complete buffered unit wins over a pending peer close, so
  `{Port,{data,…}}` is delivered before `{tcp_closed,…}`. The observation is kept in
  `TcpInetDriver::pending_close` and applied at the single latch site
  `TcpInetDriver::latch_peer_close` once nothing complete is left to deliver
  (`TcpInetDriver::latch_pending`), and only by a read the C would have made:
  `ReadReport::Answered` applies it, the reactor's eager passive read
  (`ReadReport::Unselected`) merely records it.
- `port.rs` → `PortState::Closing` is a **real observable state** (`Port::is_closing()`/
  `is_closed()`/`state()`), not a transient: `Port::close` is two halves.
  **Begin** (always): `state = Closing`, record `closing_watermark`, `driver_flush` (the four
  delivery sinks — reads, completions, `{active,…}`, failure — installed exactly as `notify_exit`
  installs them, because a `tcp_inet_flush`-shaped callback may hand a final completion up), then
  `drain_output_during_close`: it drops the pending `INPUT`/`TIMEOUT` tasks (a closing port has no
  owner left to notify) and runs the `OUTPUT` task(s) under one `DEFAULT_READINESS_RED_BUDGET`
  through the ordinary `run_task` → `ready_output` → `check_write_accounting` path, so there is
  exactly **one** write loop and a short write is re-armed rather than lost.
  **Finish** (`finish_close`, only once the queue **is** empty): `driver.stop` (same four sinks),
  then `state = Closed` — even when `stop` fails, so a teardown error can never leave the port
  looking open; whatever is still queued is dropped there (the caller has decided the remainder is
  obsolete). A close whose drain empties the queue **inside** the call finishes synchronously; a
  `flush` that discards the queue closes the port without touching the device at all.
  A **refused** write (`Err` from `output`/`ready_output`) does not fail the close: the drain stops
  and the port parks, and the refusal surfaces where it already did, from the round that re-drives
  the queued `OUTPUT` task.
- `port.rs` → the **parked** case: a queue still non-empty afterwards leaves the port in
  `PortState::Closing` with an `OUTPUT` task pending, `stop` **not** run, its poller registrations
  still armed and its number still allocated — the C's Limbo, "a state where the port is in Limbo,
  waiting to pass on. All links are removed, and the port receives in/out-put events so as soon as
  the port queue gets empty `terminate_port()` is called" (`io.c:3968-3971`; the deferred half is
  `erts_port_driver_callback_epilogue`, `erl_port.h:798-812`). Write readiness, or a later round,
  re-drives it. `PortRegistry::is_closing` reports it (`BadArg` for an unknown id, like every other
  registry seam); `Port`/`PortRegistry::close` answer `Ok(())`; a second `close` is an idempotent
  `Ok(())` — no second `flush`, no second `stop`, no second `Closed` (the C returns immediately on
  an `EXITING`/`CLOSING`/`DEAD` port); `Port::command` answers `BadArg` (the
  `ERTS_PORT_SFLGS_INVALID_LOOKUP` set includes `CLOSING`, `erl_port.h:351-354`; `io.c:1440-1441`
  answers `badarg`), so a late `port_command/2` is the oracle's `badarg`.
- `registry.rs` → the round-side finisher `finish_closing_ports`, called by **both**
  `execute_port_tasks` and `handle_events` **after `reap_failures` and before the closes are
  drained**: every live closing port is re-drained (the same `drain_output_during_close`, the same
  budget) and then either finished — queue empty, or the stall bound reached, through the same
  `remove_finished_port`/`finish_close` the synchronous path uses — or left parked for the next
  round. A port finished **this** round therefore reports its `Closed` this round. Every port is
  visited before the first error is returned, and an error leaves `self.closed` untouched, so the
  closes already queued stay queued for the next round.
- **`CLOSING_DRAIN_STALL_LIMIT` (`16`) is this crate's own valve, not the C's.** The C waits in
  Limbo for as long as the driver keeps the port alive; a port that can never write a byte would
  then hold its number, its descriptors and its poller registrations for ever **and** never tell the
  owner it closed. Progress is measured against `Port::closing_watermark` (the queue shrinks from
  the round's `OUTPUT` task as well as from the drain, so either route counts), so **any** shrinkage
  resets the count — a 500 KB write to a peer reading 1 KB per round is never cut off — and only
  `CLOSING_DRAIN_STALL_LIMIT` consecutive no-progress rounds (`Port::note_closing_drain`) make it
  give up: the remainder is dropped, `stop` runs and the close is reported exactly once.
- The **forced** close: a driver that latched a `driver_failure*`/`driver_exit` must not park
  (`PortRegistry::reap_failures` → `close_now`), because the driver whose queue would be drained has
  just declared itself broken — the C zeroes the ioq on that path (`prt->ioq.size = 0`,
  `io.c:7220`), so `close_now` runs `Port::close` and then `finish_close` regardless of the queue.
  `PortRegistry::open` does the same for a driver whose `start` failed.
- **Invariants.** A `Closed` is never minted while `output_len() > 0` (only the finish half or the
  stall bound mints it); the port number retires with that report, never with the park. What the
  port still owed when it leaves the live map is dropped — its undelivered **input** and its staged
  line frames (the C's `stopq` in `terminate_port`, `io.c:3768`) — while the three owner side
  channels (completions, `{active,…}` deliveries, spent grants) are harvested into the registry
  first and reported exactly once.

## Known Issues (durable gotchas — not fixed by changing this file)
- **A `PortNotification::Closed` names a number that is retired *with the report*.** A port that
  leaves the live map — a synchronous close, or the round that finishes a parked one — is queued in
  `self.closed` by `remove_finished_port`, and its number stays **allocated** in `ids` until the
  reporting round (`execute_port_tasks` and `handle_events`, both via `take_closed_notifications`)
  mints the `Closed` and releases the number there. So a `close` + `open` before the next round
  cannot make the deferred `Closed` name a reused, live port. A consumer must act on a `Closed` in
  the round that delivered it — the number may be handed out again immediately afterwards.
- **The partial-line `input` flush (`Port::flush_linebuf`) rides `take_eof`, so it needs the `eof`
  *stream option* in this crate.** It is triggered when `Driver::take_eof` reports `true`, but
  `drivers/spawn.rs` (`SpawnDriver::take_eof`) reports only when `StreamOptions.eof` was set — the
  driver latches the end of file either way. The oracle flushes at `driver_eof` regardless of the
  option, so a `[line]` port opened without `eof` would hold its final partial line here. Closing
  that needs a driver-side input-EOF accessor independent of the notification option (an additive
  `Driver` seam), and the runtime's adoption of `set_line_mode`/`flush_linebuf` on the `open_port`
  path. (`Driver::flush` and the close path's output drain are a different, unrelated flush — see
  *The close path* above.)
- **A close never flushes the line buffer** (`io.c:7210-7211` runs the flush from `driver_eof` only),
  and the partial line is neither reported nor discarded by it — it is still held afterwards, so an
  explicit `flush_linebuf` still releases it. Pinned by
  `port::tests::closing_a_line_port_never_flushes_the_pending_partial_line`. This says nothing about
  **output**: a close does write out (or, at the stall bound, drop) the port's queued output, while
  the **input** line buffer is untouched and undelivered input is dropped with the port.
- **A completion names a number that can be reused once its `Closed` report is minted.** The
  harvest copies a port's completions *out* when the port leaves the live map (a parked closing port
  is still live and keeps reporting), and the port number is still retired only by the `Closed` report — so a caller **must** drain
  `PortRegistry::take_completions` in the round it ran. A drain deferred past a later
  `PortRegistry::open` that reused the number would report a stale completion under a live port's
  id (the `Closed` report's own rule is unchanged and documented on `PortRegistry::close`).
- **One errno table: `error::ERRNO_NAMES` (61 rows).** `InetReason::atom_name`,
  `FailureReason::atom_name` and `drivers/inet/wire.rs::errno_name` all read it —
  `error::errno_atom` is the `Option` form (unrecognised ⇒ `None`, never an invented atom),
  `error::errno_name` the `[0] ++ Name` form with the C's `errno_<N>` fallback, and
  `wire.rs::errno_name` is a `pub(super) use crate::error::errno_name;` re-export, not a second
  table.

## Test Strategy
In-module `#[cfg(test)]` unit tests only — the sibling `tests/` directory belongs to the
integration-test node, and the crate-level policy (real fds, loud skips, oracle cross-checks) is in
`../CONTEXT.md`.
**Gate: `-p beam_io` = 1074 lib + 251 integration over 26 binaries + 3 doctests ⇒ 1328 passed, 0
failed, 2 `#[ignore]`d, over 28 test binaries.**
**Per-module in-module counts (`-p beam_io --lib` = 1074 passed):**
`drivers` **727** — of which `inet` **603** (per-suite breakdown in `drivers/inet/CONTEXT.md`),
`line_buffer` 24, `spawn` 27 (23 + the 4 in `spawn_teardown_tests.rs`), `ram_file` 34
(`ram_file_tests` 23 + `ram_file_split_tests` 6 + `ram_file_close_tests` 5), `fd` 17 (14 + the 3 in
`fd_close_tests.rs`), `efile` 13, `binary_io` 9 — then
`registry` 71 (36 + the 4 in `registry_reply_tests.rs` + the 4 in `registry_busy_tests.rs` + the 7 in
`registry_downcast_tests.rs` + the 9 in `active_mode_tests.rs` + the 4 in `awaits_callers_tests.rs`
+ the 7 in `closing_tests.rs`), `io_device` 41, `abi` 48 (`host` 25 = 18 + the 5 in
`host_output_tests.rs` + the 2 in `host_aliasing_tests.rs`, `port` 13 = 8 + the 5 in `flush_tests.rs`, `table` 7, `job` 3), `port` 36 (12 + the 12 in
`active_input_tests.rs` + the 8 in `port_command_tests.rs` + the 4 in `port_control_reply_tests.rs`),
`port_task` 22, `signal` 20, `poller` 17, `driver` 13, `timer` 12, `ddll` 11, `completion` 11,
`fs_access` 10, `queue` 8, `error` 8, `port_failure` 7, `cpu_time` 6, `close_fd` 6.
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
  `port.rs` unit-tests the port-level rule that a **close never flushes the input line buffer** (the
  held partial survives the close and an explicit flush still releases it — impossible had the close
  reported it) and that the
  `take_eof` flush releases a blank-line frame. `registry.rs` unit-tests the round-level shapes over a
  real pipe port (three `eol` lines, the blank line, the `{line,4}` split, one read ⇒ 150 frames, raw
  mode off by default, a line-mode port reporting **nothing** on `PortNotification`, `BadArg` for an
  unknown id, and the `take_eof`-driven flush) — all through the separate
  `take_line_notifications` channel. `tests/spawn.rs` (the integration node) cross-checks the whole
  pinned oracle table byte-for-byte against the system `erl` through a real `cat` child behind a real
  `[line]` / `[binary, {line, N}]` port, plus the `[binary]` no-wrapper row and the
  close-does-not-flush rule, skipping **loudly** when `erl` is absent.
- **The write path and the busy decision** are pinned hermetically (no `erl`, no child, no sleep) in
  two sibling modules of the source halves: `registry_reply_tests.rs` (declared from `registry.rs` as
  `#[cfg(test)] #[path = "registry_reply_tests.rs"] mod reply_tests;`) and `port_command_tests.rs`
  (declared from `port.rs` the same way); `registry_busy_tests.rs` and `registry_downcast_tests.rs`
  are `registry.rs`'s next two, and `port_control_reply_tests.rs` is `port.rs`'s second sibling. The
  reply round trip: one `ram_file_drv` port, `port_command [1,0,0,0,3,97,98,99]` ⇒ **one**
  `execute_port_tasks` round reports exactly one `Data { read: ReadMessage::body_only([3,0,0,0,0]) }` with `output_len()`
  and `input_len()` back to `0` and no task left pending, a second round reports nothing; plus a
  `RefusingWriteDriver` whose `output`/`ready_output` answer `Err(BadArg)` — `Port::output`,
  `Port::ready_output` and `execute_port_tasks` each surface `BadArg` with the bytes still queued.
- **The `{active, …}` state seam** is pinned hermetically in `active_mode_tests.rs` (9 tests, no
  socket, no `cc`, no `erl`), a `#[path]` sibling of `registry.rs`, over a plain-Rust `ActiveDriver`
  whose raw store (`active`/`active_count`) and staged spends are observable: `from_parts` for all four
  modes plus the out-of-range values (⇒ `False`), `is_active` on the mode field (including
  `Multi { remaining: 0 }` and a negative counter), `remaining()` only for `Multi` (including a counter
  that was never set ⇒ `Some(0)`) and `Default == False`; the **trait defaults** over a driver that
  overrides nothing (⇒ `ActiveMode::False` and an empty `Vec` from both the registry and the port, so
  no existing driver changes behaviour); a driver that *does* override both, read back through a real
  `PortRegistry` (`active_mode` per mode, the `BadArg` for an unknown id); the drain's exactly-once rule
  (a second call is empty), its within-port spending order and its port-number order across two ports;
  and the **closed half** — a spend staged before a `close`, and one staged from the driver's own `stop`,
  each reported once afterwards while the port is already gone from the live map.
  The **driver-level** half is `drivers/inet/active_seam_tests.rs` (9): the real `tcp_inet`/`udp_inet`
  drivers, one spend per exhausted `{active,once}`/`{active,N}` grant, the remaining counter readable
  through `PortRegistry::active_mode`, `{active,true}` never spending, the `setopts`-to-zero path
  spending nothing, and the registry route over a factory-built `"tcp_inet"` port.
- **The close path** is pinned hermetically in `closing_tests.rs` (7 tests, no socket, no `cc`, no
  `erl`, no sleeping), a `#[path]` sibling of `registry.rs` over a plain-Rust `ClosingDriver` whose
  ordered call log (`flush`/`output`/`stop`), accepted bytes and drain policy are all observable:
  `flush` before the drain and `stop` last; a fully written command closing **inside** the `close`
  call; a one-byte-per-call driver parking the port (`is_closing`, still `contains`, the remainder
  queued, an `OUTPUT` task pending, `stop` not run, a late command `BadArg`, a second `close` a
  no-op, then progressive rounds to a `Closed` with every byte delivered, and no `Closed` while
  `output_len() > 0`); the `CLOSING_DRAIN_STALL_LIMIT` give-up (exactly one `Closed`, remainder
  dropped); a discarding `flush` closing inside the call with nothing written; a `close` + `open`
  before the reporting round never shadowing the reported number; and exactly-once `Closed` across a
  fast (synchronous) and a slow (parked) port, plus the unknown-id `BadArg` on `close`/`is_closing`.
  The driver-level halves are `drivers/spawn_teardown_tests.rs` (4: `stop` gives the child its EOF
  before anything is killed, a closed port's whole queued command really reaches the child, a child
  that never reads stdin does not hang the close, plus the same row cross-checked against the system
  `erl`) and `abi/flush_tests.rs` (5 over `cc`-built fixtures: the loaded `flush` runs once before
  `stop`, a `NULL` slot closes cleanly and still stops, `flush` → the drain's `output` → `stop` in
  that order, a loaded driver never parks a closing port, and a marker build whose `driver_output`
  from inside the callback reaches the owner).
- **The wait-side async predicate** is pinned hermetically in `awaits_callers_tests.rs` (4 tests, no
  socket, no `cc`, no `erl`), a `#[path]` sibling of `registry.rs`, over two plain-Rust fake drivers:
  one that overrides **only** `has_pending_async` (the default `awaits_callers_async_op` follows it
  through both the port and the registry-wide probe, so an undecidable driver never claims nobody
  waits), and one that overrides **both** — read back through `Port::has_pending_async`/
  `Port::awaits_callers_async_op` and `PortRegistry::awaits_callers_async_op` over two live ports
  (listener-shaped: both `true`; loaded-driver-shaped: drive-only, and the registry-wide probe answers
  `false` once only that port is left), plus the live-port-only rule and that both probes are pure
  `&self` questions.
  The `abi::DriverPort` leg is unreachable from a source-module unit test (it needs a `cc`-built `.so`
  behind a loader handle), and its `false` wait answer carries no test today: the C-ABI integration
  suites pin the **drive** half only (`has_pending_async` in `tests/abi_async.rs`/`tests/abi_vector.rs`).
  The busy decision table is `port_command_tests.rs`'s (8 tests over a plain-Rust `BusyDriver` whose
  busy state, flag word and written bytes are all observable): idle ⇒ `Done` with the bytes really
  written and one `OUTPUT` task, busy + plain or `[nosuspend]` ⇒ `Busy` with **nothing** queued and no
  task, `[force]` ⇒ the `notsup` raise without `SOFT_BUSY` and `Done` with it, the state cleared ⇒
  `Done` again, a non-`Open` port ⇒ `BadArg`, and the `is_busy`/`port_closed` probes forwarding to the
  driver. `port_control_reply_tests.rs` (4) pins the control reply's second fact
  (`Port::control_reply_shape`, `PortRegistry::port_control_reply`/`control_reply_shape`, the reserved
  `close` op answering an empty `Buffer`, an unknown id's `badarg`, and `port_control`'s unchanged
  `Result<Vec<u8>>`). `registry_busy_tests.rs` (4) adds only what the registry contributes: the live answer, the
  legacy `port_command` folding `Busy` into `Ok(())` without queueing, and the unknown-id `BadArg`.
- **The completion channel** is pinned structurally in `completion_tests.rs` — a sibling `#[path]`
  module of `completion.rs` (11 tests, no `erl` needed): each of the nine driver interaction points
  (`start`/`output`/`ready_input`/`ready_output`/`control`/`timeout`/`drive_async`/`port_exit`/
  `stop`) stages a completion through a hook driver and it reaches `Port::take_completions` exactly
  once with the port's own id stamped; a timer-driven completion escapes through `pop_ready_task` +
  `run_task` (the only route, since `run_task`'s `Timeout` arm drops `Driver::timeout`'s `Result`);
  `execute_port_tasks` and `handle_events` rounds report what they staged; two ports drain in
  port-number order; a completion staged before `close` is reported once afterwards while the number
  waits for the `Closed` report; a second drain is empty; and `deliver_completion` with **no** sink
  is the typed `unsupported` naming the missing sink. `Port` here is the bare port (no registry) in
  the no-sink case, which is exactly the driver-driven-outside-a-port shape.
- **The `{active, …}` input channel** is pinned structurally in `active_input_tests.rs` — a sibling
  `#[path]` module of `port.rs` (12 tests, no `erl` needed): a hook driver stages a delivery from each
  of the nine interaction points and all nine reach `Port::take_active_inputs` exactly once, in call
  order, each stamped with the port's own id; an `Error(InetReason)` delivery crosses unchanged (one
  per call, two calls never merged, the reason's atom read back through `InetReason::atom_name`); a
  `Passive` event crosses unchanged; an empty payload is
  still a delivery; `deliver_active_input` stamps its own port, never merges two calls, answers the
  typed `unsupported` with no sink and refuses after a latched failure; `execute_port_tasks` reports
  what a round staged; two ports drain in port-number order and a second drain is empty; a delivery
  staged before/during `close` is reported once afterwards while the number waits for the `Closed`
  report; and one test is a **coherence** pin — `PortNotification` still has exactly two variants (a
  two-arm `match` with no wildcard).
- **The static-driver set** is pinned in `driver.rs` both ways (each of the three constructs through
  `create_static_spawn` and reports its own `Driver::name()`; `fd`/`efile` are `BadArg` there while
  still constructing through `with_builtins()`; `STATIC_SPAWN_DRIVERS` is sorted and agrees with
  `is_static_spawn_driver`; `with_static_spawn_drivers().names()` is exactly the three) and
  cross-checked against `erl_ddll:loaded_drivers/0` on the `erl` oracle, skipping **loudly** when
  `erl` is absent (the `fs_access.rs` pattern).

## Routing Table
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `poller.rs` → `Poller` (`EINTR`-tolerant `poll`, `libc::poll(2)` regular-file fallback), `Token`, `Interest` (`READABLE`/`WRITABLE`/`BOTH`/`ERROR`), `Ready`, `Event`, `Waker`; `poller_tests.rs` → its `#[path]` suite
- `port.rs` → `Port`, `PortState` (**`Closing` is a real state**, not a transient: see *The close path*), `PortControl`, `PortCommandOptions`/`PortCommandOutcome`, `Port::command`/`is_closing`/`is_closed`/`is_busy`/`port_closed`/`active_mode`/`take_active_spends`/`has_pending_async`/`awaits_callers_async_op` (the drive / wait async twins), `Port::close` + the finish half `finish_close` + `driver_flush`/`drain_output_during_close`/`note_closing_drain`, `CLOSING_DRAIN_STALL_LIMIT`, `DEFAULT_READINESS_RED_BUDGET`, `refresh_timeout`; the completed-read queue (`pending_reads`, `close_input_frame`), the completion stage (`completions` + `take_completions`), the `{active,…}` stage (`active_inputs` + `take_active_inputs`), and the line-framing state + `set_line_mode`/`line_mode`/`flush_linebuf`/`take_input_notifications`/`take_line_frames`; suites `port_command_tests.rs`, plus `port_control_reply_tests.rs` (the close-path suites live on the registry side — `closing_tests.rs`)
- `registry.rs` → `PortRegistry` (id allocation over `beam_process::PortTable`, reactor glue, `close` (parking or finishing) + `is_closing` + the round-side finisher `finish_closing_ports`, the shared removal/harvest helper `remove_finished_port`, the **forced** `close_now` a driver failure takes, `port_command`/`port_command_with`/`is_busy`/`port_closed`/`port_control`, the exit/eof/async seams, the registry-wide `awaits_callers_async_op` probe, `port_table`/`port_table_mut`, `set_line_mode`/`flush_linebuf`/`take_line_notifications`, `take_completions` + the `closed_completions` harvest, `take_active_inputs` + the `closed_active` harvest, `active_mode`, `take_active_spends` + the `closed_active_spends` harvest); suites `registry_reply_tests.rs`/`registry_busy_tests.rs`/`registry_downcast_tests.rs`/`active_mode_tests.rs`/`awaits_callers_tests.rs`/`closing_tests.rs`
- `completion.rs` → the driver→owner completion side channel: `CompletionEvent` (`InetReply { caller_ref: Option<Vec<u8>>, reply }` / `InetAsync { id, result }`), `InetReply` (`Ok`/`Error(InetReason)`), `InetAsyncResult` (`Ok`/`Data`/`Port`/`Error`), `InetReason` (`Closed`/`Timeout`/`Errno` + `from_errno`/`atom_name`) and the port-stamped `PortCompletion { port, event }`
- `active_input_tests.rs` → the `{active,…}` channel's structural suite (12 tests), a `#[path]` test module of `port.rs`
- `active_mode_tests.rs` → the driver-agnostic `{active,…}` state seam's suite (9 tests), a `#[path]` test module of `registry.rs`: the `ActiveMode::from_parts` mapping and its predicates, the two trait **defaults** over a driver that overrides nothing, an overriding fake driver read back through `PortRegistry::active_mode` (+ the unknown-id `BadArg`), and `take_active_spends`'s exactly-once / spending-order / port-number-order rules plus its **closed half** (a spend staged before a `close`, and one staged from `stop`)
- `closing_tests.rs` → the **flush-then-close** suite (7 tests), a `#[path]` test module of `registry.rs` over a plain-Rust `ClosingDriver` whose ordered call log (`flush`/`output`/`stop`), accepted bytes and drain policy are observable — what each test pins is in `## Test Strategy` above
- `awaits_callers_tests.rs` → the **wait-side** async predicate's suite (4 tests), a `#[path]` test module of `registry.rs`: the trait default following `has_pending_async` over a driver that overrides only the drive half, a driver that overrides **both** read back through `Port::has_pending_async`/`Port::awaits_callers_async_op` and the registry-wide probe over two ports (a listener-shaped one where both are `true`, a loaded-driver-shaped one that is drive-only), the live-port-only rule, and that both probes are pure `&self` questions
- `completion_tests.rs` → the channel's structural suite (11 tests), a `#[path]` test module of `completion.rs`: all nine interaction points, the timer-only route, both round drivers, port-number ordering, the close harvest, exactly-once, and the no-sink `unsupported`
- `driver_output2.rs` (`../tests/`) → the split-read pin over a real `PortRegistry`/`Poller`: one `Data` per call, header/body exact, `Some(vec![])` vs `None`, `input_len` = `hlen+len`, no re-report on a second round
- `registry_reply_tests.rs` → the write-path pin: the reply round trip (`port_command` ⇒ one `Data`) and the refused-command propagation (`Err(BadArg)` out of `execute_port_tasks`), a `#[path]` test module of `registry.rs`
- `registry_busy_tests.rs` → the registry's busy/peer-close seams (4 tests), a `#[path]` test module of `registry.rs`: a live idle port taking the command, a busy port reported busy with the legacy `port_command` folding `Busy` into `Ok(())` **without** queueing, `port_closed` forwarding the driver's latch, and the unknown-id `BadArg` all three seams answer
- `port_control_reply_tests.rs` → the control-reply **shape** suite (4 tests), a `#[path]` test module of
  `port.rs`: the driver's shape forwarded (`Buffer` default), the registry answering bytes **and** shape,
  the reserved `close` op's empty `Buffer`, and an unknown id's `badarg` on both new seams
- `registry_downcast_tests.rs` → the driver-downcast seam's suite (7 tests): `Port::driver`/`driver_as`
  and `PortRegistry::driver_as`, a mismatched type answering `None`, and an unknown id
- `port_command_tests.rs` → the busy-port decision table's suite (8 tests), a `#[path]` test module of `port.rs`: idle ⇒ `Done` + one `OUTPUT` task, busy + plain/`[nosuspend]` ⇒ `Busy` with nothing queued, `[force]` ⇒ `notsup` without `SOFT_BUSY` and `Done` with it, the state cleared ⇒ `Done` again, `BadArg` on a non-`Open` port, and the `is_busy`/`port_closed` probes
- `port_task.rs` → `PortTaskKind`/`PortTask`/`PortTaskQueue`, reduction constants, `Readiness`, `ReadMessage` (`{header, body}` — the `driver_output2` split), `PortNotification` (`Data { port, read }` + `Closed`), `LineNotification` (the separate `{line,N}` channel, with its per-frame `header`), `ActiveInput`/`ActiveInputEvent` (the separate `{active,…}` channel: `Data`/`Passive`/`Error(InetReason)`), `ActiveMode` (`{active, Mode}` with `from_parts`/`is_active`/`remaining`) and `ActiveSpend` (`Once`/`Multi` — the grants a delivery spent to exhaustion), `PortTaskOutcome`
- `queue.rs` → `ByteQueue`; `timer.rs` → `Timeout`, `Timer`
- `driver.rs` → `Driver` (its frozen method set: required `name`/`start`/`output`/`ready_input`/`stop`; defaulted `flags`/`ready_output`/`control`/`has_control` (default `true`; `false` iff the C counterpart has no `control` — `RamFileDriver` and a NULL-`control` `DriverPort`)/`is_busy`/`port_closed` (both defaulting `false`; the first is the `set_busy_port` state `Port::command` decides on, the second a latched peer/OS close)/`active_mode` (default `ActiveMode::False`) and `take_active_spends` (default an empty, allocation-free `Vec`)/`timeout`/`next_timeout`/`port_exit`/`drive_async`/`has_pending_async` (the **drive** predicate: work a port round must drive, accept/job inclusive)/`awaits_callers_async_op` (the **wait** predicate beside it: an async op some caller is parked on, defaulting to `has_pending_async` so a driver that cannot tell the two apart never claims nobody waits; the socket drivers answer accept-inclusively, `abi::DriverPort` answers `false`)/`flush` (the close path's one chance to discard queued output **before** `stop`; the default discards nothing — the C's `flush == NULL` — and only `SpawnDriver`, `drivers::TcpInetDriver` and `abi::DriverPort` override it)/`take_exit_status`/`take_eof`), `DriverContext` (incl. the four sinks: `with_reads` + `deliver_read(header, body)`, `with_completions` + `deliver_completion(event)`, `with_active` + `deliver_active_input(event)`, `with_failure` + `signal_failure(failure)` — the first three stamp `ctx.port()` itself, and each answers a typed `unsupported` with no sink), `DriverFlags`, `DriverRegistry` (+ `STATIC_SPAWN_DRIVERS`/`is_static_spawn_driver`/`create_static_spawn`/`with_static_spawn_drivers` — the `driver_tab` seam), `PortExitReason`, `PortArms`
- `error.rs` → `pub(crate)` typed error constructors
- `signal.rs` → `Signal`/`Disposition`/`SignalFacility`; `cpu_time.rs` → `CpuTimes`/`RuntimeClock`; `ddll.rs` → `DynLib`/`DllError`
- `fs_access.rs` → the safe `access(2)`/`geteuid(2)`/`getegid(2)` floor: `AccessMode` (`Exists`/`Read`/`Write`/`Execute`, `ALL`, `libc_mode()`), `try_access` (errno-preserving `Result<()>`, the one `unsafe` `access(2)` call site, `EINTR` retried, an interior-NUL path ⇒ `BadArg`), `access` (`Result<bool>`, refusal ⇒ `false`, only a genuine syscall failure is `BeamError::Io`), `path_access` (`Result<(read, write)>` — the `#file_info{}` `access` slot, two independent probes) and `process_credentials` (`Result<(euid, egid)>` — the **effective** ids `check_access` compares a `stat` against)
- `drivers/` → the built-in drivers (`fd`, `efile`, `spawn`, `tcp_inet`, `udp_inet`, `ram_file_drv`) + framing, incl. `line_buffer.rs`'s `LineFrame`/`LineBuffer` (the port-level `{eol | noeol, Data}` splitter) — child node `drivers/CONTEXT.md`
- `drivers/inet/` → the `tcp_inet`/`udp_inet` socket drivers (option/syscall/stat layer, the async-op queue, the `{active,…}` deliveries, the opcode bridge, `{packet,N}` framing) — child node `drivers/inet/CONTEXT.md`; its suite halves are `write_frame_tests.rs` (12 — the `{packet,1|2|4}` header on both write routes, `send_oct`, the short write and the refusals, over a real loopback port), `flush_tests.rs` (6 — the close-path flush: the unarmed-write discard, the `{linger,{true,0}}` flag, the teardown-vs-close-verb split, and `udp_inet`'s absent slot), `close_tests.rs` (5 — what a teardown keeps and what the close verb drops), `active_seam_tests.rs` (9 — `Driver::active_mode`/`take_active_spends` over the real drivers), `passive_recv_tests.rs` (9 — the passive `gen_tcp:recv` park), `accept_opts_family_tests.rs` (7 — `prim_inet:accept_opts/3`'s whole eleven-code list, incl. `FamilyOpts`) and `shutdown_contract_tests.rs` (12 — the `gen_tcp:shutdown/2` aftermath)
- `drivers/spawn_teardown_tests.rs` → the `spawn` driver's **teardown ordering** suite (4 tests), a `#[path]` test module of `drivers/spawn.rs` (so `super` is `spawn`): `stop` gives the child end-of-file before anything is killed, closing the port delivers every queued byte of a payload larger than a pipe buffer, a child that never reads stdin does not hang the close, and that same row cross-checked against the system `erl` — every wait polls a condition under a 60 s hang guard (never a wall-clock budget), skipping **loudly** without `erl`
- `abi/flush_tests.rs` → `abi::DriverPort`'s `flush` forwarding suite (5 tests), a `#[path]` test module of `abi/port.rs` over `cc`-built C fixtures (`flush_driver.c`, `flush_driver_no_flush.c`, `flush_driver_marker.c`): the loaded driver's `flush` runs exactly once and before its `stop`; an entry with no `flush` slot closes cleanly and still stops; the order is `flush` → the drain's `output` → `stop`; a loaded driver never parks a closing port (its `output`/`outputv` are `void`, so the queue always counts as written); and the marker build's `driver_output` from inside `flush` reaches the owner (gated on `host_symbols_exported`, since the lib unit-test binary lacks `-Wl,--export-dynamic`)
- `abi/` → the driver C-ABI host layer — child node `abi/CONTEXT.md`
- `io_device/` → the `{get_until,…}` io-device request layer — child node `io_device/` (files listed in `../CONTEXT.md`)

## Handoffs (beam_rt — what a runtime now observes on a close)
- **A close can return before the port is gone.** `PortRegistry::close` answers `Ok(())` with the port **parked** in `PortState::Closing` when its queued output could not all be written synchronously: the driver's `flush` has run, `stop` has **not**, the port is still in the live map (`contains` and `is_closing` both `true`), its poller registrations are still armed, and every further command is the oracle's `badarg`.
- **`Closed` arrives from a later round, and the number retires with that report.** A runtime must keep running rounds (`execute_port_tasks`/`handle_events`) after such a close: the round that empties the queue — or the one that hits `CLOSING_DRAIN_STALL_LIMIT` and drops the remainder — reports the `PortNotification::Closed`, and only then is the number released. A runtime that stops running rounds after a parked close therefore holds the number, its descriptors and its poller registrations; the same applies to a parked port the runtime never re-drives.
- **`erlang:port_close/1`'s `true` still covers the buffered bytes**, so rendering `true` on a parked close is correct rather than an early answer; the bytes are delivered by the rounds that follow (the oracle row this was measured against: a `tee` child received 500 000/500 000 bytes while `port_close/1` returned `true` with 454 656 bytes still queued).
- **The drained output goes to the device, so it owes the owner no extra message** — but whatever a `flush`, a `stop` or a drained `ready_output` stages on the side channels still reaches the runtime through the existing drains: completions, `{active,…}` deliveries and spent grants are harvested into the registry queues when the port leaves the live map, and a parked port keeps reporting through them until it does.
- **What is *not* reported:** the input and line frames a port still owed when it leaves the live map are dropped with it (the C's `stopq` in `terminate_port`), so input staged by the close-time drain of a port that finishes **inside** the `close` call is dropped too. A parked port is still live, so its input is harvested normally — which is why a runtime must drain `take_input_notifications`/`take_line_notifications` in the round it runs.

## See Also
- `../CONTEXT.md` → the authoritative crate doc (frozen API surface, constraints, test strategy, handoffs).
- `../../../erts/emulator/beam/{io.c,erl_port_task.c,erl_port_task.h,erl_bif_port.c}` and
  `../../../erts/emulator/sys/common/{erl_poll.c,erl_check_io.c}` → the legacy C ground truth
  (`io.c:3140-3307` the `LineBuf` state machine, `:3454` `deliver_linebuf_message`, `:7210-7211` the
  `driver_eof` flush; `erl_bif_port.c:862-869,986` the `line`/`{line,N}` option decoding).
- The close path's C ground truth: `io.c:3946-3953` (`erts_deliver_port_exit`'s CLOSING arm),
  `:3659-3700` (`flush_port`), `:3704-3795` (`terminate_port`, whose sole `drv->stop` call is
  `:3752`),
  `:3968-3971` (the Limbo comment), `:7220` (the forced `prt->ioq.size = 0` on a driver failure),
  `:3768` (`stopq`), `erl_port.h:798-812` (`erts_port_driver_callback_epilogue`), `:351-354`
  (`ERTS_PORT_SFLGS_INVALID_LOOKUP`, which includes `CLOSING`), plus
  `../../../erts/emulator/sys/unix/sys_drivers.c:1029-1034` (`fd_flush`'s latch) and
  `drivers/common/ram_file_drv.c:120` (`flush = NULL`).

## Oracle-pinned shapes of the port layer (measured with the system `erl`)

A C driver fixture (`driver_output(port,"",0)`, `driver_output(port,NULL,0)`,
`driver_output2(port,"hdr",3,{NULL,""},"body",4)`, two calls per callback) run against
OTP 29.1.1 pins, per `open_port` option set:

| call | `[]` | `[binary]` | `[{line,4}]` |
|---|---|---|---|
| `driver_output(p,"",0)` | `{data,[]}` | `{data,<<>>}` | **no message** |
| `driver_output(p,NULL,0)` | `{data,[]}` | `{data,[]}` (no tail, so no binary) | no message |
| `driver_output2(p,"hdr",3,NULL,0)` | `{data,"hdr"}` | `{data,"hdr"}` | no message |
| `driver_output2(p,"hdr",3,"",0)` | `{data,"hdr"}` | `{data,[104,100,114\|<<>>]}` | no message |
| `driver_output2(p,"hdr",3,"body",4)` | `{data,"hdrbody"}` | `{data,[104,100,114\|<<"body">>]}` | no message |
| two `driver_output` in one callback | **two** `{data,`_`}` messages | two | one `noeol` frame once the buffer passes its limit |

So an empty `driver_output` **is** delivered in raw mode (both port modes) and never in line
mode, and two calls are two messages. Both halves of that rule live in the port layer now:
`Port::take_input_notifications` closes the open frame first and then gates on `pending_reads`
alone (so a staged zero-byte read is reported, and `Port::has_input` is a test-only observation
helper nothing may gate on), and a loaded C driver's `driver_output*` hooks stage through
`DriverContext::deliver_read` — `output` is `deliver_read(&[], Some(data))` (one body-only read per
call, never banked on the accumulator, so two calls in one callback are two `{data,_}` messages) and
`output2` forwards the header/body split with the C's `buf == NULL` kept as `body: None`. Pinned
in-module by `src/abi/host_tests.rs` (`driver_output_stages_one_body_only_read_per_call`,
`driver_output2_keeps_the_header_apart_and_the_null_body_distinct`,
`the_host_output_path_reaches_the_port_owner_one_message_per_call`).

Driver failures, measured the same way after `driver_failure*`/`driver_exit` from `output`:

| C call | owner mailbox | `port_info(P,name)` | later `port_command`/`port_control`/`port_close` |
|---|---|---|---|
| `driver_failure_posix(p,EINVAL)` | `{'EXIT',P,einval}` | `undefined` | `badarg` |
| `driver_failure_atom(p,"enospc")` | `{'EXIT',P,enospc}` | `undefined` | `badarg` |
| `driver_failure(p,42)` | `{'EXIT',P,42}` | `undefined` | `badarg` |
| `driver_failure_eof(p)` | `{'EXIT',P,normal}` | `undefined` | `badarg` |
| `driver_exit(p,0)` / `driver_exit(p,ENOSPC)` | **nothing** (the C unlinks first, `io.c:7244-7245`) | `undefined` | `badarg` |
| `driver_output` then `driver_failure_posix` | `{data,…}` **then** `{'EXIT',P,einval}` | `undefined` | `badarg` |
