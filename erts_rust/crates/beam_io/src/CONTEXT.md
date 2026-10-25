# `beam_io/src` — crate source root (module wiring)

## Intent
Source root of `beam_io` (the I/O subsystem of the Rust ERTS rewrite) plus the three sub-nodes that
carry the bulk of the code. The parent `../CONTEXT.md` carries the crate's frozen API surface,
constraints, design decisions and cross-crate handoffs — read it first. This file adds the
**port-level mechanism** (line framing, the write path and the busy decision, the completion and
`{active,…}` channels, the flush-then-close path), the per-suite test pins and the durable gotchas;
the parent routes here for that detail.

## API Surface
Module wiring + root re-exports + `prelude`: `lib.rs`. Every module below is re-exported at the
crate root and in `prelude`.

Four modules are **standalone raw-OS facilities** (safe wrappers over syscalls `beam_rt`, which
denies `unsafe_code`, cannot issue itself) rather than parts of the I/O pipeline: `signal.rs`
(`os:set_signal/2`), `cpu_time.rs` (`erlang:statistics(runtime)`), `ddll.rs` (`erl_ddll`) and
`fs_access.rs` (`access(2)` / `geteuid(2)` / `getegid(2)`, for the `file`/`filelib` floor). Their
frozen APIs are documented in the parent `../CONTEXT.md` and in each module's own rustdoc.

### The statically-linked driver set (`driver.rs` — the C `driver_tab.c` seam)
Only the drivers the emulator **statically links** are addressable through
`open_port({spawn_driver, Name}, …)`; `erts-*/lib/` ships zero `.so`, and the oracle's
`erl_ddll:loaded_drivers/0` answers exactly `{ok,["tcp_inet","udp_inet","ram_file_drv"]}`.
`driver.rs` exposes that set and nothing else there:
- `STATIC_SPAWN_DRIVERS: [&str; 3]` — `["ram_file_drv","tcp_inet","udp_inet"]`, sorted; the
  emulator's `driver_tab` entries, **not** `.so`-loadable.
- `is_static_spawn_driver(name) -> bool` — membership in that set.
- `DriverRegistry::with_static_spawn_drivers()` — a registry whose `names()` is exactly the three.
- `DriverRegistry::create_static_spawn(name)` — the `{spawn_driver, Name}` resolution path: a
  non-member is `BeamError::BadArg` (the oracle's `badarg` for `{spawn_driver,"fd"}` / `"efile"`)
  *even on a registry that registers it*, then the ordinary factory lookup (so a static name
  missing from *this* registry is also `BadArg`, never a wrong driver).
`DriverRegistry::with_builtins()` registers five names — `fd` and `efile` are this crate's
own drivers (its internal users and tests build ports from them) — so a runtime must gate
`{spawn_driver, Name}` on `create_static_spawn`, **not** on `with_builtins`/`create`.

### `{line, N}` line framing (`open_port(…, [line])` / `[binary, {line, N}]`)
The C splits lines **in the port**, not in the driver: `open_port`'s bare `line` option sets
`ERTS_PORT_SFLG_LINEBUF_IO` with a 512-byte line buffer and `{line, N}` sets it to `N`
(`../../../../erts/emulator/beam/erl_bif_port.c:862-869,986`), and the port wraps every frame it splits
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
`../../../../erts/emulator/drivers/common/inet_drv.c:2617-2740`) and the
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

**A new owner-facing message needs a NEW channel** — a struct + a `DriverContext::deliver_*` sink +
a `PortRegistry::take_*` drain, under the same "drain after every round and every `close`" rule —
never a third `PortNotification` variant and never a `CompletionEvent` arm.

### The `{active, …}` state seam (`port_task.rs`'s `ActiveMode`/`ActiveSpend` — driver-agnostic)
The **read** half of the state machine the channel above reports deliveries on: a driver owns the
`{active, false|true|once|N}` mode (the C's `inet.active`/`inet.active_count`) and spends one grant
per delivered packet inside its own option store (`INET_CHECK_ACTIVE_TO_PASSIVE`,
`inet_drv.c:1552-1560`). Without this seam a runtime could mirror the `setopts` walk but never learn
the running `{active, N}` counter, nor that an `{active, once}` grant was spent (the C clears that
**silently**, sending no `{tcp_passive,_}`). The value types are driver-agnostic and both `Driver`
members carry defaults, so a driver that knows nothing about `{active,…}` needs no code.
- `port_task.rs` → `ActiveMode { False, True, Once, Multi { remaining: i16 } }` (`Copy + Clone +
  PartialEq + Eq + Debug`, `Default = False`), whose `ActiveMode::from_parts(active: i32, active_count: i16)` is
  the **single** home of the driver store's numeric encoding (`1`/`2`/`3` ⇒ `True`/`Once`/`Multi`;
  anything else, `INET_PASSIVE` `0` included, ⇒ `False`) — plus `is_active()` (the C's `active > 0`
  test on the **mode** field, so even a hand-built `Multi { remaining: 0 }` is active) and
  `remaining()` (`Some` only for `Multi`, and the **running** counter, never the granted `N`).
- `port_task.rs` → `ActiveSpend { Once, Multi }`: `Once` a spent `{active, once}` grant (the silent
  flip), `Multi` a counter that reached zero (whose `{tcp_passive,S}` already rides the channel above
  as `ActiveInputEvent::Passive`); `True`/`False` have no variant because they are never spent.
- `driver.rs` → the two **additive, defaulted** members `Driver::active_mode() -> ActiveMode`
  (`False`) and `Driver::take_active_spends() -> Vec<ActiveSpend>` (empty and allocation-free when
  nothing was spent); taking consumes, like every other drain in this crate.
- `port.rs` → `Port::active_mode()` / `Port::take_active_spends()`, plain unstamped forwards.
- `registry.rs` → `PortRegistry::active_mode(port) -> Result<ActiveMode>` (the `badarg` every other
  registry seam answers for an unknown id) and
  `PortRegistry::take_active_spends() -> Vec<(PortId, ActiveSpend)>`. Contract: drain after **every
  round and every `close`**, every spend reported **exactly once** — live ports in **port-number
  order**, then the spends harvested from ports closed since the last drain (`closed_active_spends`,
  filled **after** the driver's `stop`, so a spend recorded before **or during** the teardown is
  reported: the delivery it produced still reaches the owner). The number keeps the existing rule —
  retired only by the `Closed` report.
- **Who overrides:** the two socket drivers only, each reading `ActiveMode::from_parts(...)` and
  recording a spend at **one** site, the per-delivery one (TCP `count_delivery`, the datagram twin) —
  never on the `setopts`-walk-to-zero path (which still emits its `ActiveInputEvent::Passive` but
  spends no grant). Every other built-in inherits the honest defaults. Detail:
  `drivers/inet/CONTEXT.md`.

### The close path — flush, drain, then `stop` (the C's `ERTS_PORT_SFLG_CLOSING`)
A close does not drop queued output: `Port::close` has the C's **flush-then-close** shape, so
`erlang:port_command(P, Data)` followed immediately by `erlang:port_close(P)` still delivers `Data` —
the drain is asynchronous, **not** part of the close's return. The C: `erts_deliver_port_exit` clears
`EXITING`, sets `ERTS_PORT_SFLG_CLOSING` and calls `flush_port` instead of `terminate_port` when the
ioq is non-empty and the reason is not `kill` (`io.c:3946-3953`, `flush_port(prt)` at `:3952`);
`flush_port` calls the driver's `flush` **only if the entry has one** (`io.c:3664` the slot test,
`:3687` the call) and terminates **only if the ioq became empty** (`io.c:3696-3699`);
`terminate_port` (`io.c:3704-3795`) is the sole caller of `drv->stop` (`io.c:3752`).
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
  and the port parks, and the refusal surfaces from the round that re-drives the queued `OUTPUT`
  task.
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
  id (the `Closed` report's own rule, documented on `PortRegistry::close`).
- **One errno table: `error::ERRNO_NAMES` (61 rows).** `InetReason::atom_name`,
  `FailureReason::atom_name` and `drivers/inet/wire.rs::errno_name` all read it —
  `error::errno_atom` is the `Option` form (unrecognised ⇒ `None`, never an invented atom),
  `error::errno_name` the `[0] ++ Name` form with the C's `errno_<N>` fallback, and
  `wire.rs::errno_name` is a `pub(super) use crate::error::errno_name;` re-export, not a second
  table.

## Test Strategy
In-module `#[cfg(test)]` unit tests only — the sibling `tests/` directory is the integration-test
node's (`../tests/CONTEXT.md`), and the crate-level policy is `../CONTEXT.md`'s.
- **Gate:** `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` GREEN,
  plus `... build --workspace --all-targets` GREEN and warning-free. The wrapper is **mandatory** — the
  sandbox `$HOME` (hence cargo's default `CARGO_HOME`) is read-only.
- **A skip is loud, never a false pass:** a suite needing `cc`/`erl_driver.h`/`erl` prints a `SKIP:`
  banner and returns early. A wait is a **condition under a hang guard** (60 s), never a fixed sleep,
  never a `waitpid(2)` from a test, never a wall-clock budget tuned for an idle host.
- **Byte-exact against the oracle** wherever an oracle row exists — the reply matrices, the line
  framing table, the `{packet,N}`/`send_oct` figures, the `access`/`uid`/`gid` slot: an `erl`-derived
  expectation is compared byte-for-byte, never approximately.
- **Test counts are deliberately not pinned here** (they go stale): the per-suite entries in the
  routing table below and in each child node name what a suite pins, and `git log` carries the latest
  aggregate.
- **`fs_access.rs`'s uid caveat:** `access(2)` answers differently for a privileged process, so the
  mode-`000` read/write refusal is asserted only when `euid != 0` and that sub-case skips loudly under
  a uid-0 container; the `ENOENT` and no-execute-bit cases hold for every uid (POSIX fixes the latter,
  root included). Its one oracle test compares two implementations of the *same* syscall
  (`file:read_file_info/1`'s `access`/`uid`/`gid`), so it needs no guard at all.
- **Line framing** is pinned at three levels: the `line_buffer.rs` marker unit tests (`eol` only for a
  newline terminator, the limit and CR_AFTER splits `noeol`, every flushed frame `noeol`,
  `flush_into_ready`'s arrival order), the port-level rule that a **close never flushes the input line
  buffer**, and a round-level suite over a real pipe port (the blank line, the `{line,4}` split, one
  read staging many frames, a line-mode port reporting **nothing** on `PortNotification`) — plus
  `../tests/spawn.rs`'s byte-exact oracle table through a real `cat` child.
- **The decision-table suites are all hermetic** (no `erl`, no child, no socket, no sleep), each a
  `#[path]` sibling of its source half over a plain-Rust fake driver whose call log, bytes and
  counters are observable: `port_command_tests.rs` (the busy table), `port_control_reply_tests.rs`
  (the reply **shape**), `active_mode_tests.rs` and `active_input_tests.rs` (the `{active,…}` state
  seam and its delivery channel), `closing_tests.rs` (flush-then-close, the park, the stall bound,
  exactly-once `Closed`), `completion_tests.rs`, `awaits_callers_tests.rs`, `registry_reply_tests.rs`,
  `registry_busy_tests.rs`, `registry_downcast_tests.rs`. What each asserts is its routing-table entry.
- **Driver-level halves live with their drivers:** `drivers/spawn_teardown_tests.rs` (teardown
  ordering over real children, a payload larger than a pipe buffer), `fd_close_tests.rs`,
  `ram_file_close_tests.rs`, the `drivers/inet/*_tests.rs` families and `abi/flush_tests.rs` over
  `cc`-built fixtures — detail in `drivers/CONTEXT.md`, `drivers/inet/CONTEXT.md`, `abi/CONTEXT.md`.
- **Test gap:** `abi::DriverPort`'s `false` wait answer (`awaits_callers_async_op`) is unreachable
  from a source-module unit test (it needs a `cc`-built `.so` behind a loader handle); the C-ABI
  integration suites pin the **drive** half only.

## Routing Table
- `lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `poller.rs` → `Poller` (`EINTR`-tolerant `poll`, `libc::poll(2)` regular-file fallback), `Token`, `Interest` (`READABLE`/`WRITABLE`/`BOTH`/`ERROR`), `Ready`, `Event`, `Waker`; `poller_tests.rs` → its `#[path]` suite
- `port.rs` → `Port`, `PortState` (**`Closing` is a real state**, not a transient: see *The close path*), `PortControl`, `PortCommandOptions`/`PortCommandOutcome`, `Port::command`/`is_closing`/`is_closed`/`is_busy`/`port_closed`/`active_mode`/`take_active_spends`/`has_pending_async`/`awaits_callers_async_op` (the drive / wait async twins), `Port::close` + the finish half `finish_close` + `driver_flush`/`drain_output_during_close`/`note_closing_drain`, `CLOSING_DRAIN_STALL_LIMIT`, `DEFAULT_READINESS_RED_BUDGET`, `refresh_timeout`; the completed-read queue (`pending_reads`, `close_input_frame`), the completion stage (`completions` + `take_completions`), the `{active,…}` stage (`active_inputs` + `take_active_inputs`), and the line-framing state + `set_line_mode`/`line_mode`/`flush_linebuf`/`take_input_notifications`/`take_line_frames`; suites `port_command_tests.rs`, plus `port_control_reply_tests.rs` (the close-path suites live on the registry side — `closing_tests.rs`)
- `registry.rs` → `PortRegistry` (id allocation over `beam_process::PortTable`, reactor glue, `close` (parking or finishing) + `is_closing` + the round-side finisher `finish_closing_ports`, the shared removal/harvest helper `remove_finished_port`, the **forced** `close_now` a driver failure takes, `port_command`/`port_command_with`/`is_busy`/`port_closed`/`port_control`, the exit/eof/async seams, the registry-wide `awaits_callers_async_op` probe, `port_table`/`port_table_mut`, `set_line_mode`/`flush_linebuf`/`take_line_notifications`, `take_completions` + the `closed_completions` harvest, `take_active_inputs` + the `closed_active` harvest, `active_mode`, `take_active_spends` + the `closed_active_spends` harvest); suites `registry_reply_tests.rs`/`registry_busy_tests.rs`/`registry_downcast_tests.rs`/`active_mode_tests.rs`/`awaits_callers_tests.rs`/`closing_tests.rs`
- `completion.rs` → the driver→owner completion side channel: `CompletionEvent` (`InetReply { caller_ref: Option<Vec<u8>>, reply }` / `InetAsync { id, result }`), `InetReply` (`Ok`/`Error(InetReason)`), `InetAsyncResult` (`Ok`/`Data`/`Port`/`Error`), `InetReason` (`Closed`/`Timeout`/`Errno` + `from_errno`/`atom_name`) and the port-stamped `PortCompletion { port, event }`
- `active_input_tests.rs` → the `{active,…}` channel's structural suite (12 tests), a `#[path]` test module of `port.rs`
- `active_mode_tests.rs` → the driver-agnostic `{active,…}` state seam's suite (9 tests), a `#[path]` test module of `registry.rs`: the `ActiveMode::from_parts` mapping and its predicates, the two trait **defaults** over a driver that overrides nothing, an overriding fake driver read back through `PortRegistry::active_mode` (+ the unknown-id `BadArg`), and `take_active_spends`'s exactly-once / spending-order / port-number-order rules plus its **closed half** (a spend staged before a `close`, and one staged from `stop`)
- `closing_tests.rs` → the **flush-then-close** suite (7 tests), a `#[path]` test module of `registry.rs` over a plain-Rust `ClosingDriver` whose ordered call log (`flush`/`output`/`stop`), accepted bytes and drain policy are observable: `flush` before the drain and `stop` last; a fully written command closing **inside** the `close` call; a one-byte-per-call driver parking the port (still `contains`, remainder queued, `stop` not run, a late command `BadArg`, a second `close` a no-op) then draining to `Closed` with no `Closed` while output is owed; the `CLOSING_DRAIN_STALL_LIMIT` give-up; a discarding `flush`; a `close` + `open` never shadowing the reported number; exactly-once `Closed` across a fast and a parked port; and the unknown-id `BadArg`
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
- `port_failure.rs` (+ `port_failure_tests.rs`) → the driver→owner **failure** channel: `FailureReason` (`Eof`/`Normal`/`Atom`/`Posix`/`Code` + `atom_name`/`code`/`is_eof`), `DriverFailure { reason, signals_owner }` (the flag `driver_exit` clears — it unlinks the owner **before** closing) and the three `deliver_*` seams' refusal once a failure latched
- `error.rs` → `pub(crate)` typed error constructors
- `signal.rs` → `Signal`/`Disposition`/`SignalFacility`; `cpu_time.rs` → `CpuTimes`/`RuntimeClock`; `ddll.rs` → `DynLib`/`DllError`; `close_fd.rs` → `close_fd(RawFd)` (an `EINTR`-retried `close(2)` reporting `EBADF`) and `OwnedFd` (closed exactly once; `into_raw` hands a descriptor to a driver) — the **safe, public** close primitive for a descriptor the runtime owns or declines
- `fs_access.rs` → the safe `access(2)`/`geteuid(2)`/`getegid(2)` floor: `AccessMode` (`Exists`/`Read`/`Write`/`Execute`, `ALL`, `libc_mode()`), `try_access` (errno-preserving `Result<()>`, the one `unsafe` `access(2)` call site, `EINTR` retried, an interior-NUL path ⇒ `BadArg`), `access` (`Result<bool>`, refusal ⇒ `false`, only a genuine syscall failure is `BeamError::Io`), `path_access` (`Result<(read, write)>` — the `#file_info{}` `access` slot, two independent probes) and `process_credentials` (`Result<(euid, egid)>` — the **effective** ids `check_access` compares a `stat` against)
- `drivers/` → the built-in drivers (`fd`, `efile`, `spawn`, `tcp_inet`, `udp_inet`, `ram_file_drv`) + framing, incl. `line_buffer.rs`'s `LineFrame`/`LineBuffer` (the port-level `{eol | noeol, Data}` splitter) — child node `drivers/CONTEXT.md`
- `drivers/inet/` → the `tcp_inet`/`udp_inet` socket drivers (option/syscall/stat layer, the async-op queue, the `{active,…}` deliveries, the opcode bridge, `{packet,N}` framing) — child node `drivers/inet/CONTEXT.md`; its suite halves are `write_frame_tests.rs` (12 — the `{packet,1|2|4}` header on both write routes, `send_oct`, the short write and the refusals, over a real loopback port), `flush_tests.rs` (6 — the close-path flush: the unarmed-write discard, the `{linger,{true,0}}` flag, the teardown-vs-close-verb split, and `udp_inet`'s absent slot), `close_tests.rs` (5 — what a teardown keeps and what the close verb drops), `active_seam_tests.rs` (9 — `Driver::active_mode`/`take_active_spends` over the real drivers), `passive_recv_tests.rs` (9 — the passive `gen_tcp:recv` park), `accept_opts_family_tests.rs` (7 — `prim_inet:accept_opts/3`'s whole eleven-code list, incl. `FamilyOpts`) and `shutdown_contract_tests.rs` (12 — the `gen_tcp:shutdown/2` aftermath)
- `drivers/spawn_teardown_tests.rs` → the `spawn` driver's **teardown ordering** suite (4 tests), a `#[path]` test module of `drivers/spawn.rs` (so `super` is `spawn`): `stop` gives the child end-of-file before anything is killed, closing the port delivers every queued byte of a payload larger than a pipe buffer, a child that never reads stdin does not hang the close, and that same row cross-checked against the system `erl` — every wait polls a condition under a 60 s hang guard (never a wall-clock budget), skipping **loudly** without `erl`
- `abi/flush_tests.rs` → `abi::DriverPort`'s `flush` forwarding suite (5 tests), a `#[path]` test module of `abi/port.rs` over `cc`-built C fixtures (`flush_driver.c`, `flush_driver_no_flush.c`, `flush_driver_marker.c`): the loaded driver's `flush` runs exactly once and before its `stop`; an entry with no `flush` slot closes cleanly and still stops; the order is `flush` → the drain's `output` → `stop`; a loaded driver never parks a closing port (its `output`/`outputv` are `void`, so the queue always counts as written); and the marker build's `driver_output` from inside `flush` reaches the owner (gated on `host_symbols_exported`, since the lib unit-test binary lacks `-Wl,--export-dynamic`)
- `abi/` → the driver C-ABI host layer — child node `abi/CONTEXT.md`
- `io_device/` → the `{get_until,…}` io-device request layer — child node `io_device/` (files listed in `../CONTEXT.md`)

## Handoffs (beam_rt — what a runtime observes on a close)
- **A close can return before the port is gone.** `PortRegistry::close` answers `Ok(())` with the port **parked** in `PortState::Closing` when its queued output could not all be written synchronously: the driver's `flush` has run, `stop` has **not**, the port is still in the live map (`contains` and `is_closing` both `true`), its poller registrations are still armed, and every further command is the oracle's `badarg`.
- **`Closed` arrives from a later round, and the number retires with that report.** A runtime must keep running rounds (`execute_port_tasks`/`handle_events`) after such a close: the round that empties the queue — or the one that hits `CLOSING_DRAIN_STALL_LIMIT` and drops the remainder — reports the `PortNotification::Closed`, and only then is the number released. A runtime that stops running rounds after a parked close therefore holds the number, its descriptors and its poller registrations; the same applies to a parked port the runtime never re-drives.
- **`erlang:port_close/1`'s `true` still covers the buffered bytes**, so rendering `true` on a parked close is correct rather than an early answer; the bytes are delivered by the rounds that follow (the oracle row this was measured against: a `tee` child received 500 000/500 000 bytes while `port_close/1` returned `true` with 454 656 bytes still queued).
- **The drained output goes to the device, so it owes the owner no extra message** — but whatever a `flush`, a `stop` or a drained `ready_output` stages on the side channels still reaches the runtime through the existing drains: completions, `{active,…}` deliveries and spent grants are harvested into the registry queues when the port leaves the live map, and a parked port keeps reporting through them until it does.
- **What is *not* reported:** the input and line frames a port still owed when it leaves the live map are dropped with it (the C's `stopq` in `terminate_port`), so input staged by the close-time drain of a port that finishes **inside** the `close` call is dropped too. A parked port is still live, so its input is harvested normally — which is why a runtime must drain `take_input_notifications`/`take_line_notifications` in the round it runs.

## See Also
- `../CONTEXT.md` → the authoritative crate doc (frozen API surface, constraints, test strategy, handoffs).
- `../../../../erts/emulator/beam/{io.c,erl_port_task.c,erl_port_task.h,erl_bif_port.c}` and
  `../../../../erts/emulator/sys/common/{erl_poll.c,erl_check_io.c}` → the legacy C ground truth
  (`io.c:3140-3307` the `LineBuf` state machine, `:3454` `deliver_linebuf_message`, `:7210-7211` the
  `driver_eof` flush; `erl_bif_port.c:862-869,986` the `line`/`{line,N}` option decoding).
- The close path's C ground truth: `io.c:3946-3953` (`erts_deliver_port_exit`'s CLOSING arm),
  `:3659-3700` (`flush_port`), `:3704-3795` (`terminate_port`, whose sole `drv->stop` call is
  `:3752`),
  `:3968-3971` (the Limbo comment), `:7220` (the forced `prt->ioq.size = 0` on a driver failure),
  `:3768` (`stopq`), `erl_port.h:798-812` (`erts_port_driver_callback_epilogue`), `:351-354`
  (`ERTS_PORT_SFLGS_INVALID_LOOKUP`, which includes `CLOSING`), plus
  `../../../../erts/emulator/sys/unix/sys_drivers.c:1029-1034` (`fd_flush`'s latch) and
  `../../../../erts/emulator/drivers/common/ram_file_drv.c:120` (`flush = NULL`).

## Oracle-pinned shapes of the port layer

A C driver fixture calling `driver_output*` from inside one callback pins, per `open_port` option set:
an empty `driver_output(p,"",0)` **is** delivered in raw mode (`{data,[]}` on `[]`, `{data,<<>>}` on
`[binary]`) and **never** in `{line,N}` mode; `driver_output(p,NULL,0)` has no binary tail, so
`[binary]` renders the plain list `{data,[]}` too; `driver_output2(p,"hdr",3,Buf,4)` keeps the header
apart, so `[binary]` renders `[104,100,114|<<"body">>]` (and `[104,100,114|<<>>]` for a present empty
body, the list `{data,"hdr"}` for `NULL`); and two `driver_output` calls in one callback are **two**
`{data,_}` messages (one `noeol` frame in line mode).
Both halves of that rule live in the port layer: `Port::take_input_notifications` closes the open frame
first and then gates on `pending_reads` alone (so a staged zero-byte read is reported, and
`Port::has_input` is a test-only observation helper nothing may gate on), and a loaded C driver's
`driver_output*` hooks stage through `DriverContext::deliver_read` — `output` is
`deliver_read(&[], Some(data))` (one body-only read per call, never banked on the accumulator, so two
calls in one callback are two `{data,_}` messages) and `output2` forwards the header/body split with
the C's `buf == NULL` kept as `body: None`. Pinned in-module by `abi/host_output_tests.rs`
(`driver_output2_keeps_the_header_apart_and_the_null_body_distinct` and the two sibling rows).

A driver that latches a failure answers the owner `{'EXIT',P,Reason}` (`driver_failure_posix(EINVAL)`
⇒ `einval`, `driver_failure_atom("enospc")` ⇒ `enospc`, `driver_failure(p,42)` ⇒ `42`,
`driver_failure_eof` ⇒ `normal`), while `driver_exit` answers **nothing** (the C unlinks the owner
first, `io.c:7244-7245`). After either, `port_info(P,name)` is `undefined` and every
`port_command`/`port_control`/`port_close` is `badarg`; an input staged before the failure is reported
**first**, then the `'EXIT'`.
