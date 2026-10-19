# `beam_io/src` — crate source root (module wiring)

## Intent
Source root of `beam_io` (the I/O subsystem of the Rust ERTS rewrite) plus the three sub-nodes that
carry the bulk of the code. The parent `../CONTEXT.md` carries the crate's frozen API surface,
constraints, design decisions and cross-crate handoffs — read it first. This file adds the
**port-level channel mechanism** (line framing, the write path and the busy decision, the completion
and `{active,…}` channels), the per-module in-module test counts and the oracle-pinned shape tables,
and the durable gotchas; the parent routes here for that detail.

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
  `io.c:3363-3442`) and a single read can stage many frames (597 bytes at `{line,4}` ⇒ 150, on the
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
`../../../erts/emulator/drivers/common/inet_drv.c:2606-2680`) and the
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
  `close`(`stop`); the `timeout` one matters most, because `Port::run_task`'s `Timeout` arm discards
  `Driver::timeout`'s `Result`, so a timer-driven `{error,timeout}` reply exists only as a completion.
- `registry.rs` → `PortRegistry::take_completions()` drains every live port in **port-number order**
  (each port's own completions in delivery order), then the completions harvested from ports closed
  since the last drain. `close` **harvests** the closing port's staged completions into a
  registry-owned queue before dropping the `Port`, so a teardown's `async_error_am_all` replies (and
  any staged before it) still reach the owner exactly once; the port **number** keeps its existing
  rule — retired only by the `Closed` report, never by a completion.
Contract: **drain after every round** (line-frames style), a control op's return value is never a
completion, and each completion is reported exactly once.

### The driver→owner `{active, …}` input channel (`port_task.rs`'s `ActiveInput`)
An `{active, true|N|once}` socket owes its owner **incoming-message** notifications that are
neither `{Port, {data, …}}` (the passive read path) nor the answer to a call (a completion): the
`{tcp, S, Data}` of `tcp_message` (`inet_drv.c:3979`), its `{packet,N}` shape
`packet_binary_message` (`:4110`), `{tcp_passive, S}`/`{udp_passive, S}` from
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
- **A completion names a number that can be reused once its `Closed` report is minted.** The
  harvest copies a closing port's completions *out* (the port itself is dropped), and the port
  number is still retired only by the `Closed` report — so a caller **must** drain
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
**Per-module in-module counts (`-p beam_io --lib` = 989):** `drivers` **660** — of which `inet`
**548** (per-suite breakdown in `drivers/inet/CONTEXT.md`), `line_buffer` 24, `spawn` 23, `ram_file`
29 (`ram_file_tests` 23 + `ram_file_split_tests` 6), `fd` 14, `efile` 13, `binary_io` 9 — then
`registry` 60 (36 + the 4 in `registry_reply_tests.rs` + the 4 in `registry_busy_tests.rs` + the 7 in
`registry_downcast_tests.rs` + the 9 in `active_mode_tests.rs`), `io_device` 41, `abi` 41 (`host` 23 = 18 + the 5 in
`host_output_tests.rs`, `port` 8, `table` 7, `job` 3), `port` 36 (12 + the 12 in
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
- `port.rs` → `Port`, `PortState`, `PortControl`, `PortCommandOptions`/`PortCommandOutcome`, `Port::command`/`is_busy`/`port_closed`/`active_mode`/`take_active_spends`, `DEFAULT_READINESS_RED_BUDGET`, `refresh_timeout`; the completed-read queue (`pending_reads`, `close_input_frame`), the completion stage (`completions` + `take_completions`), the `{active,…}` stage (`active_inputs` + `take_active_inputs`), and the line-framing state + `set_line_mode`/`line_mode`/`flush_linebuf`/`take_input_notifications`/`take_line_frames`; suite `port_command_tests.rs`, plus `port_control_reply_tests.rs`
- `registry.rs` → `PortRegistry` (id allocation over `beam_process::PortTable`, reactor glue, `port_command`/`port_command_with`/`is_busy`/`port_closed`/`port_control`, the exit/eof/async seams, `port_table`/`port_table_mut`, `set_line_mode`/`flush_linebuf`/`take_line_notifications`, `take_completions` + the `closed_completions` harvest, `take_active_inputs` + the `closed_active` harvest, `active_mode`, `take_active_spends` + the `closed_active_spends` harvest); suites `registry_reply_tests.rs`/`registry_busy_tests.rs`/`registry_downcast_tests.rs`/`active_mode_tests.rs`
- `completion.rs` → the driver→owner completion side channel: `CompletionEvent` (`InetReply { caller_ref: Option<Vec<u8>>, reply }` / `InetAsync { id, result }`), `InetReply` (`Ok`/`Error(InetReason)`), `InetAsyncResult` (`Ok`/`Data`/`Port`/`Error`), `InetReason` (`Closed`/`Timeout`/`Errno` + `from_errno`/`atom_name`) and the port-stamped `PortCompletion { port, event }`
- `active_input_tests.rs` → the `{active,…}` channel's structural suite (12 tests), a `#[path]` test module of `port.rs`
- `active_mode_tests.rs` → the driver-agnostic `{active,…}` state seam's suite (10 tests), a `#[path]` test module of `registry.rs`: the `ActiveMode::from_parts` mapping and its predicates, the two trait **defaults** over a driver that overrides nothing, an overriding fake driver read back through `PortRegistry::active_mode` (+ the unknown-id `BadArg`), and `take_active_spends`'s exactly-once / spending-order / port-number-order rules plus its **closed half** (a spend staged before a `close`, and one staged from `stop`)
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
- `driver.rs` → `Driver` (its frozen method set: required `name`/`start`/`output`/`ready_input`/`stop`; defaulted `flags`/`ready_output`/`control`/`has_control` (default `true`; `false` iff the C counterpart has no `control` — `RamFileDriver` and a NULL-`control` `DriverPort`)/`is_busy`/`port_closed` (both defaulting `false`; the first is the `set_busy_port` state `Port::command` decides on, the second a latched peer/OS close)/`active_mode` (default `ActiveMode::False`) and `take_active_spends` (default an empty, allocation-free `Vec`)/`timeout`/`next_timeout`/`port_exit`/`drive_async`/`has_pending_async`/`take_exit_status`/`take_eof`), `DriverContext` (incl. the four sinks: `with_reads` + `deliver_read(header, body)`, `with_completions` + `deliver_completion(event)`, `with_active` + `deliver_active_input(event)`, `with_failure` + `signal_failure(failure)` — the first three stamp `ctx.port()` itself, and each answers a typed `unsupported` with no sink), `DriverFlags`, `DriverRegistry` (+ `STATIC_SPAWN_DRIVERS`/`is_static_spawn_driver`/`create_static_spawn`/`with_static_spawn_drivers` — the `driver_tab` seam), `PortExitReason`, `PortArms`
- `error.rs` → `pub(crate)` typed error constructors
- `signal.rs` → `Signal`/`Disposition`/`SignalFacility`; `cpu_time.rs` → `CpuTimes`/`RuntimeClock`; `ddll.rs` → `DynLib`/`DllError`
- `fs_access.rs` → the safe `access(2)`/`geteuid(2)`/`getegid(2)` floor: `AccessMode` (`Exists`/`Read`/`Write`/`Execute`, `ALL`, `libc_mode()`), `try_access` (errno-preserving `Result<()>`, the one `unsafe` `access(2)` call site, `EINTR` retried, an interior-NUL path ⇒ `BadArg`), `access` (`Result<bool>`, refusal ⇒ `false`, only a genuine syscall failure is `BeamError::Io`), `path_access` (`Result<(read, write)>` — the `#file_info{}` `access` slot, two independent probes) and `process_credentials` (`Result<(euid, egid)>` — the **effective** ids `check_access` compares a `stat` against)
- `drivers/` → the built-in drivers (`fd`, `efile`, `spawn`, `tcp_inet`, `udp_inet`, `ram_file_drv`) + framing, incl. `line_buffer.rs`'s `LineFrame`/`LineBuffer` (the port-level `{eol | noeol, Data}` splitter) — child node `drivers/CONTEXT.md`
- `drivers/inet/` → the `tcp_inet`/`udp_inet` socket drivers (option/syscall/stat layer, the async-op queue, the `{active,…}` deliveries, the opcode bridge, `{packet,N}` framing) — child node `drivers/inet/CONTEXT.md`; its two newest suite halves are `write_frame_tests.rs` (12 — the `{packet,1|2|4}` header on both write routes, `send_oct`, the short write and the refusals, over a real loopback port) and `active_seam_tests.rs` (9 — `Driver::active_mode`/`take_active_spends` over the real drivers)
- `abi/` → the driver C-ABI host layer — child node `abi/CONTEXT.md`
- `io_device/` → the `{get_until,…}` io-device request layer — child node `io_device/` (files listed in `../CONTEXT.md`)

## See Also
- `../CONTEXT.md` → the authoritative crate doc (frozen API surface, constraints, test strategy, handoffs).
- `../../../erts/emulator/beam/{io.c,erl_port_task.c,erl_port_task.h,erl_bif_port.c}` and
  `../../../erts/emulator/sys/common/{erl_poll.c,erl_check_io.c}` → the legacy C ground truth
  (`io.c:3140-3307` the `LineBuf` state machine, `:3454` `deliver_linebuf_message`, `:7211` the
  `driver_eof` flush; `erl_bif_port.c:862-869,986` the `line`/`{line,N}` option decoding).

## Oracle-pinned shapes still owed by the port layer (measured with the system `erl`)

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
| `driver_exit(p,0)` / `driver_exit(p,ENOSPC)` | **nothing** (the C unlinks first, `io.c:7244`) | `undefined` | `badarg` |
| `driver_output` then `driver_failure_posix` | `{data,…}` **then** `{'EXIT',P,einval}` | `undefined` | `badarg` |
