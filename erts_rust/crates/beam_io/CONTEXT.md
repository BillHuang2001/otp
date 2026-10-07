# `beam_io` — Poller/Reactor, Ports, Port-Task Scheduling, Built-in Drivers

## Intent
Owns the **I/O subsystem** of the Rust ERTS rewrite (Stage 5, Part II §H) and the **OS/driver seam** — one
of the two clean, already-existing rewrite seams (Part II §A): it is the layer that must be reproduced with
the *frozen* driver C ABI on top of it (workspace `../CONTEXT.md` §1 **A.4**), so a driver `.so` compiled
against the unmodified `erl_driver.h` links and behaves identically.

Five things live here: the `mio`-backed **poller/reactor** (`Token`→registration map); the **`Port`** (a
process-like I/O endpoint with an input + output byte queue, a lifecycle and driver dispatch); **port-task
scheduling** (`INPUT`/`OUTPUT`/`TIMEOUT` tasks + reduction accounting, `port_task.rs`, mirroring
`erts_port_task_execute`); the **`Driver` trait + built-in drivers** (`fd`, `efile`, the OS-child `spawn`
driver, the TCP `tcp_inet` driver, `{line, N}` buffering, the text⇄binary phase-change protocol); and the
**driver C-ABI host layer** (`src/abi/`) — a driver `.so` loaded by `beam_abi::driver::loader` is presented
as a `Box<dyn Driver>` by [`DriverPort`], the driver's own `driver_*`/`erl_drv_*` calls are served by
[`AbiHost`] (a `beam_abi::driver::host::DriverHost` impl over this crate's poller/queues/timer), the one
runtime hook this crate cannot fill — atom interning — is the [`PortHost`] trait, and [`DriverTable`]
(`src/abi/table.rs`) is the **process-global driver registry** (the C's `driver_list`) plus its
port-creation service.

Consumed by `beam_rt` (boot/`system_info`/driver registration) and, through port ids, observed by
`beam_process` (ports are the peer of pids in `ptab`). `beam_rt` **drives its ports through this crate**: one
long-lived `Poller` + `PortRegistry`, a [`SpawnDriver`] built from its decoded `open_port/2` terms, the
port-task round (`execute_port_tasks`) run from its main loop, and the resulting [`PortNotification`]s turned
into `{Port,{data,…}}`/`{Port,{exit_status,…}}` delivered to the owning process (waking a blocked receiver
through the `beam_process` scheduler). A C driver `.so` reaches the same `Driver` trait through
[`make_loaded`] → [`DriverPort`], which `beam_rt` opens with `PortRegistry::open` exactly like a built-in
driver. This crate is the **library**; the scheduler *thread* wiring (the poll loop, port run queues) lives in
`beam_rt` (single-threaded).

Legacy C ground truth (read-only): `../../erts/emulator/sys/common/erl_poll.c` + `erl_check_io.c` (the poll
loop, one-shot re-armed readiness, self-pipe wakeup), `../../erts/emulator/beam/io.c`
(`erts_port_command`/`erts_port_control`, the `ioq` output queue), `erl_port.h`, `erl_io_queue.{c,h}`
(`SysIOVec` queue), `erl_port_task.{c,h}` (the asynchronous work unit + reductions),
`../../erts/emulator/drivers/common/inet_drv.c`, and `../../erts/emulator/sys/unix/erl_child_setup.c` (the
`{line, N}` / binary phase-change behavior).

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_io::prelude::*`. Only the **frozen contracts and non-obvious
behaviour** are listed; rustdoc carries the mechanical signatures.

### `poller.rs`
- `Token(usize)` — opaque registration handle; **token `0` is reserved for the waker**, user tokens start at `1`.
- `Interest` — the readiness mask (`READABLE`/`WRITABLE`/`BOTH`). · `Ready` — observed readiness, **no public
  constructor** (only `Ready::default()` and `From<Ready> for Readiness`). · `Event` — a readiness event
  (`token`/`key`/`ready`); `Event::new` lets tests forge one.
- `Poller` — owns `mio::Poll` **and the single `mio::Waker`** created in `new()`; registration is by raw `RawFd`
  + `Interest` + a caller `key`; `poll(timeout)` returns an **owned `Vec<Event>`** (`None` blocks); `waker()`
  returns an infallible cheap clone.
- `Waker` — an `Arc`-backed, cloneable, thread-safe handle to the poller's single wakeup handle; the woken event
  carries `key() == WAKER_KEY` (`u64::MAX`).
- **Regular-file fallback (internal, no public signature change).** `mio`/epoll refuses regular files
  (`EPOLL_CTL_ADD` ⇒ `EPERM`), so `Poller::register` keeps a `libc::poll(2)`-backed fallback set: an unsupported
  descriptor is watched with `poll(2)` and its readiness **merged** into `Poller::poll` in the same `Event`/`Ready`
  shape; the other ops treat both sources uniformly, any other `mio` error is still `BeamError::Io`, and `poll`
  never blocks indefinitely while the fallback set is non-empty (`FALLBACK_POLL_CAP`, 1 ms; zero wait when the
  fallback probe already fired).

### `timer.rs` / `queue.rs`
- `Timeout` (`Copy`) — a single deadline; `Timer` — a set of deadlines keyed by `u64`. · `ByteQueue` — a byte
  buffer whose `front()` is always contiguous.

### `port_task.rs` (port-task scheduling — `erts_port_task_execute`)
- Reduction constants: `CONTEXT_REDS = 4000`; `PORT_REDS_{INPUT,OUTPUT,TIMEOUT} = CONTEXT_REDS / 100 (40)`;
  `PORT_REDS_EXECUTE = CONTEXT_REDS / 4 (1000)`.
- `PortTaskKind { Input, Output, Timeout }` (`priority()`: Input < Output < Timeout; `reductions()`); `PortTask`
  (`Copy + Eq + Debug`).
- `PortTaskQueue` — one pending task per kind (dedupe), ordered by kind priority then FIFO `seq`, timeouts
  deadline-gated. `arm_timeout(deadline)` re-points/replaces a pending `TIMEOUT`, distinct from the dedupe-by-kind
  `enqueue_timeout`, which cannot re-point.
- `Readiness { readable, writable, error, read_closed, write_closed }` — `read_ready()` (readable ‖ read_closed ‖
  error), `write_ready()` (writable ‖ write_closed), `is_idle()`, `From<Ready>`.
- `PortNotification::Data { port, bytes } | Closed { port }` — the seam the runtime turns into
  `{Port, {data, Data}}`/port-exit messages. · `PortTaskOutcome { pub ran, pub reds_used, pub notifications }` —
  one round's result.

### `port.rs`
- `PortState { Opening, Open, Closing, Closed }`. · `DEFAULT_READINESS_RED_BUDGET = PORT_REDS_EXECUTE` — the
  budget `handle_events` runs.
- `PortControl::CLOSE == "close"` — the one **reserved** control-op name (a namespace marker, not an enum: any
  other name is a driver verb).
- `PortCommandOptions { nosuspend, force }` — carried/validated but no-ops (M1 ports are never busy; the BIF layer
  owns their effect once a busy-port command queue exists).
- `Port` — owns a `Box<dyn Driver>` + the two byte queues; the endpoint, scheduling
  (`queue_output`/`schedule_task`/`schedule_timeout`/`arm_timeout`/`cancel_timeout`/`next_timeout`/`pending_tasks`/
  `next_task`/`has_pending_task`/`timeout`) and exit/async (`take_exit_status`/`drive_async`/`notify_exit`)
  surfaces. **Every driver interaction** (`start`/`output`/`ready_input`/`ready_output`/`control`/`timeout`)
  refreshes the port timer from `Driver::next_timeout` via the internal `refresh_timeout`: `Some(d)`
  arms/**re-points** the single `TIMEOUT` task at `d`, `None` clears it (a driver's declaration is authoritative
  until its next interaction). Re-exports `PortRegistry` so `crate::port::PortRegistry` keeps resolving.

### `registry.rs`
- `PortRegistry` — id allocation (inner `beam_process::PortTable`) + `HashMap<u64, Port>` keyed by port number;
  basic ops (`new`/`open`/`open_named`/`get`/`get_mut`/`output`/`contains`/`len`/`is_empty`/`close`). Frozen seams
  (all `BadArg` for an unknown id): `handle_events` (**schedules *and* runs** one round under
  `DEFAULT_READINESS_RED_BUDGET`); `execute_port_tasks` (run queued tasks in port-number order under a reduction
  budget, returning a `PortTaskOutcome`); `schedule_events`/`schedule_readiness` (enqueue only — readiness ⇒ an
  `INPUT`/`OUTPUT` task per `read_ready`/`write_ready`, **only for directions the driver actually armed**,
  recorded by `DriverContext` in `driver::PortArms`); `port_command` (queue output + schedule an `OUTPUT` task —
  the write happens in the task, not here); `port_control` (the reserved name `"close"` closes the port with an
  empty reply; **every other `op`** is forwarded verbatim to the driver's `control(op, arg)` and its reply
  returned); `take_exit_status` (the reaped child status **once** — `Ok(None)` = no child / not yet exited /
  already reported); `schedule_timeout`/`cancel_timeout`/`next_deadline` (explicit port timers; `next_deadline` =
  the minimum over live drivers' `Driver::next_timeout`, for clamping a poll timeout); `notify_port_exit`
  (`Driver::port_exit` **at most once**, the port left live, independent of `close`/`stop`); `drive_async` (the
  async drive point — calls the driver's `Driver::drive_async`, i.e. `poll_async` for a loaded driver; call once
  per port-task round, there is no `PortTaskKind::Async` in M1).

### `driver.rs`
- `DriverFlags(u32)` — `Copy + Eq`; `SOFT_BUSY`. · `PortExitReason { Closed, OwnerExited, Normal }` — the port-exit
  notification vocabulary.
- `DriverContext<'a>` — the driver's view of its port; **the only way a driver touches the poller or the queues**
  (`port`/`register`/`reregister`/`deregister`/`input`/`output`); registration keys are the port number.
  (`pub(crate) PortArms` records which directions the driver armed; not public.)
- `pub trait Driver` — **frozen**. Required: `name()` (+ `flags()` default `empty()`), `start`, `output`,
  `ready_input`, `stop`. Defaulted: `ready_output` (= `output`), `control`, `timeout`,
  `next_timeout() -> Option<Instant>` (= `None`; the driver's **declaration** of its next deadline, mirrored into
  the port's `TIMEOUT` task after each interaction, deadline-gated so it never fires early),
  `port_exit(PortExitReason)` (= `Ok(())`; the once-only, advisory port-exit notification, distinct from the
  teardown `stop`), `drive_async` (= `Ok(0)`; the `driver_async`-completion **drive point**, overridden by
  `DriverPort` with `poll_async`), `take_exit_status() -> Result<Option<i32>>` (= `Ok(None)`; overridden only by
  `SpawnDriver`). New capabilities must be additive with defaults that keep the built-in drivers compiling.
- `DriverFactory = fn() -> Result<Box<dyn Driver>>`; `DriverRegistry` — name→factory (the `driver_tab.c` seam);
  `with_builtins()` registers **`"fd"`, `"efile"` and `"tcp_inet"`**.

### `drivers/` (each module's role is in the Routing Table)
- `fd.rs` — `FdDriver`, the built-in `"fd"` byte-stream driver; **owns** its fds (closed on `stop`/`Drop`).
  Constructors `pipe`/`connected(read_fd, write_fd)`/`read_only`/`write_only`; line/phase modes
  (`with_line_mode`/`set_line_mode` (0 = unlimited)/`set_binary_mode`/`is_line_buffered`/`phase`/`set_phase`/
  `max_line_len`). `Driver`: registers the read fd `READABLE` on `start`; `output`/`ready_output` drain
  `ctx.output()` to the write fd (`EAGAIN` ⇒ keep queued + arm `WRITABLE`); `ready_input` reads into `ctx.input()`
  (loops to `EAGAIN`, latches `read_eof` on `read(2)==0`, flushes a pending partial line at EOF, then
  **de-registers the read fd** so an exhausted — level-triggered fallback — regular file stops producing events;
  the fd stays attached/owned, so a post-EOF `ready_input` is still `Ok`). `control` verbs: `line [decimal(N) |
  "line,<N>"]`, `noline`, `binary`, `text`, `getopts` (reply `b"line,<N>"`/`b"binary"`); unknown ⇒ `BadArg`.
- `line_buffer.rs` — `LineBuffer` + `DEFAULT_MAX_LINE_LEN = 1024`: frames on `\n` (a preceding `\r` is kept as
  part of the terminator), splits a line longer than `max_len`, holds a partial line until `flush_all`/EOF.
- `binary_io.rs` — `Phase { Text, Binary }` + `PhaseChange`, the `erl_child_setup` text⇄binary phase machine,
  default `Phase::Binary` (byte-identical to raw passthrough); `handle` frames Binary as one raw frame and Text as
  line frames; `flush` returns the trailing partial line (Text only).
- `efile.rs` — `EfileDriver` (`name() == "efile"`) delegates byte movement to a composed `FdDriver`; `control`
  verbs `open <path>` / `close` / `path` / `eof` plus the delegated `line`/`noline`/`binary`/`text`/`getopts`.
  `open` works on **regular files** (served by the poller's `poll(2)` fallback) as well as on epoll-servable
  descriptors (pipes/FIFOs/char devices/sockets); `write_only`/`connected` write regular files too. `make_efile()`
  is its `DriverFactory` (the built-in `"efile"`).
- `spawn.rs` — `SpawnStream { Binary, List }` (`Default = List`) is the `binary`/`list` option, applied by the
  **runtime** when it builds `{Port, {data, Data}}` (the driver hands bytes through unchanged). `SpawnSpec
  { Spawn { command, argv }, SpawnExecutable { path }, Fd { fd } }` is the term-free `open_port/2` `Name`
  (`/bin/sh -c Cmd` or `argv` run directly, `Path` run directly with `SpawnOptions::args`, or an adopted
  descriptor with no child); `command()` is the `port_info(Port, name)` value (`fd:<N>` for the descriptor form).
  `SpawnOptions { stream, cd, env, args, exit_status }` (`Default`: `list`, nothing else) are the honored options.
  `build_command(spec, options)` is the single place the argv vector, shell-vs-direct, `{cd, …}`/`{env, …}` and the
  standard streams (`stdin`/`stdout` piped, `stderr` discarded to `/dev/null`) are set up — `None` for
  `SpawnSpec::Fd`, `BadArg` for an empty argv; it starts nothing, so it is unit-testable alone. `SpawnDriver`
  (`name() == "spawn"`) is constructed **directly** from a spec/options pair (a `DriverFactory` is a no-arg `fn`
  and cannot carry one, so it is *not* in `DriverRegistry`); `new(...)` **really spawns** (or adopts the fd) so a
  failure registers nothing and the runtime can read `os_pid()` before `PortRegistry::open`; accessors `os_pid`/
  `command`/`stream`/`eof`/`exited()` (a plain liveness probe)/`take_exit_status()` (the `exit_status` option,
  reported once). `start` only registers the already-created descriptors; the child's stdout is read into the port
  input queue (drained to `EAGAIN`, EOF latched + the descriptor deregistered/closed), its stdin drains the port
  output queue (`O_NONBLOCK`; `EAGAIN` leaves the remainder queued and arms write readiness; a write failure is a
  typed `BeamError::Io`); `stop`/`Drop` kill + reap the child (an already-exited child is reaped via `try_wait`, no
  kill) and deregister/close every descriptor. A spawned child's descriptors are owned/closed; an adopted `{fd, N}`
  descriptor is **duplex** (one `Interest::BOTH` registration serving both read and the stream `port_command`
  writes to), deregistered but **never** closed (the Erlang side owns it). The child's **stderr is discarded**
  (`Stdio::null()`): it is not port data and there is no pipe for it to fill. A failed **direct-exec** spawn (a
  missing program, `EACCES`, `ENOTDIR`, …) is a `BeamError::Raise` carrying the errno atom
  (`enoent`/`eacces`/`enotdir`/…), matching the oracle; an unmapped errno falls back to `BeamError::Io` (see
  `src/error.rs`).
- `inet/` (`mod.rs` + `wire.rs`) — `DRIVER_NAME = "tcp_inet"` is the name `port_info(Port, name)` reports and
  `prim_inet` opens; `make_tcp_inet()` is its `DriverFactory`. `TcpInetDriver` (`name() == "tcp_inet"`) is a
  non-blocking TCP socket behind a port; `from_socket(fd)` adopts a descriptor and probes its kernel state
  (`SO_ACCEPTCONN` ⇒ listening, `getpeername` ⇒ connected, else open), like `inet_ctl_fdopen`; `connect_error()`
  exposes the late `SO_ERROR` outcome of a non-blocking `connect`; `InetState` is the socket state. Control verbs
  (the C `INET_REQ_*`/`TCP_REQ_*` codes) and reply bytes: `open` ⇒ `[1]`; `bind` ⇒ `[1] ++ port_be16`; `listen` ⇒
  `[1]`; `accept` ⇒ `[1] ++ fd_be32` (bare `[1]` when nothing is pending, `[0]einval` if not listening);
  `connect` ⇒ `[1]` (or `[0]exbadport`/`[0]eisconn`/`[0]einval`/`[0]++errno`); `send`/`recv`/`shutdown` ⇒ `[1]`
  (or `[0]enotconn`); `close` ⇒ `[1]`. Operational failures are `[0] ++ lowercase-errno-name` (`erl_errno_id`);
  malformed args and unknown verbs ⇒ `BadArg`; packed addresses use `INET_AF_INET`/`ANY`/`LOOPBACK`. Flow control
  mirrors `FdDriver` (`MSG_NOSIGNAL`; `EAGAIN` keeps the remainder queued and arms write readiness; a drained
  queue drops writability and applies a deferred `shutdown`; reads drain to `EAGAIN` and latch EOF; `EINPROGRESS`
  arms write readiness). **Deferred** (in the module docs): the second-port `accept`, the async-op queue,
  `driver_set_timer` timeouts, `fdopen`/`sendfile`/`unrecv`/`getopts`/`setopts`/`getstats`/`getfd`/`ignorefd`,
  `recv`'s timeout/length, IPv6/UDP/SCTP/Unix-domain sockets, `{active,…}` modes, and DNS (literal addresses).

### `src/abi/` (`mod.rs` + `host.rs` + `job.rs` + `port.rs` + `table.rs`) — the driver C-ABI host layer (A.4)
The bridge that presents a **loaded C driver** (`beam_abi::driver::loader::load`) as a [`Driver`](src/driver.rs) —
the only place in this crate that holds raw `ErlDrv*` pointers and calls foreign code; the four modules are a
`pub` sub-tree (`pub use crate::abi::*`).
- `mod.rs` — module docs (the derivation, the stable `ErlDrvPort` handle, the built-in/`.so` unification, the
  `unsafe`-module marking, where each of the four process-global `driver_*` slots reaches or refuses) + `pub mod
  host; job; port; table;` + re-exports.
- `host.rs`:
  - `pub trait PortHost` — the runtime hook `beam_io` cannot fill. Required `intern_atom(name) -> Option<Eterm>`;
    defaulted `connected_pid()`/`caller_pid()` (both `None`) and `send_term(to, term) -> bool` (default `false`;
    the term sink for `erl_drv_output_term`/`erl_drv_send_term`, `to == None` = the port owner). **A
    locally-interned atom does NOT match the runtime's atom table** — `beam_rt` MUST inject its own `PortHost`
    over the shared table.
  - `pub struct LocalAtomPortHost { atoms: AtomTable }` — a **test/standalone** host only (seeds names in order;
    inherits the optional defaults; documented as *not* matching the runtime's table).
  - `SelectedFd { token, interest }` (`pub(crate)`) and `DriverState` — the port's persistent C-ABI state:
    `selected: BTreeMap<RawFd, SelectedFd>` (a `BTreeMap` so `ready_input`/`ready_output` pick a **stable** fd),
    `deadline: Option<Instant>` (the `driver_set_timer` deadline) and `jobs: AsyncJobs`.
  - `AbiHost` — the `beam_abi::driver::host::DriverHost` impl, **owning** everything (`port_host: Box<dyn
    PortHost>`, `heap: Heap`, `state: DriverState`, `driver: Option<DriverIdentity>`, `ctx: Cell<*mut c_void>`)
    so it can be `Box`ed behind a stable
    `ErlDrvPort` (a `DRIVER_HEAP_WORDS = 1024`-word term heap). When **no context is installed** every
    port-touching method fails gracefully (`-1`/`None`/no-op) — never derefs null. `DriverHost` behaviors:
    `intern_atom`/`connected_pid`/`caller_pid` → `PortHost`; `output` → pushes onto the port's **input** queue
    (`0`/`-1`); `send_term` → `PortHost::send_term` (`0`/`-1`); `select(_, event, mode, on)` → `fd = event as
    usize as RawFd` (the C's fd-cast), `on != 0` registers (or **re-registers in place**, updating the interest)
    and `on == 0` de-registers (unknown fd ⇒ `0`), with READ→READABLE, WRITE→WRITABLE, both→BOTH and
    **neither→READABLE** (M1 does not distinguish `ERL_DRV_USE`; the fd must still be watched for close/error);
    `set_timer` arms `deadline` (returns `0` — M1 always succeeds, no `timeout == NULL` case), `cancel_timer`
    clears it (`0`), `read_timer` reports the remaining ms (`Some(0)` when expired, `None` when none); `r#async` →
    `AsyncJobs::schedule`, `id as c_long` or (on thread failure) runs the driver's `free(data)` and returns `-1`;
    the four **driver-registry** hooks → `add_driver_entry` records `de` in the process-global `DriverTable` (`0`
    recorded, `1` the entry's `init` refused it — the C's `void` wrapper discards that code, so it is only
    assertable, and a positive value stays distinguishable from the trait default's `-1` = "no current host"),
    `remove_driver_entry` forwards the table's `1`/`0`/`-1` unchanged, `lock_driver` answers `0` for a port whose
    identity is a `dlopen`ed driver (the C consults the **port's own** driver, never a name lookup — so the global
    table is consulted only to honour its `locked` flag) and `-1` for a bare/statically linked host, and
    `create_port` answers `ERTS_INVALID_ERL_DRV_PORT` (see *Design Decisions*).
  - `DriverIdentity { name, loaded }` (`pub(crate)`) — the port's registered driver name and whether it came from
    a `dlopen`ed object; `AbiHost::new` yields none, `AbiHost::for_driver(host, name, loaded)` / `with_driver(...)`
    record one (`DriverPort::from_library` sets it from `library.name()` with `loaded = true`). It is a field on
    `AbiHost`, **not** a `DriverHost` method — a driver never asks its own identity.
- `job.rs` — `AsyncJobs`: one `std::thread` per job, an `mpsc` completion channel and the `driver_async` queue
  position; `schedule(invoke, data, free)` (`None` `invoke` = fire-and-forget: a valid position, no work, no
  completion) and `drain() -> Vec<AsyncCompletion>`; `AsyncData(*mut c_void)` is the one `unsafe impl Send`.
  **Documented deviation:** `async_free` runs on the **port** thread (the C runs it on the worker).
- `port.rs`:
  - `DriverPort` — a loaded `ErlDrvEntry` as `impl Driver` (fields `lib: Arc<DriverLibrary>`/`name`/`command`/
    `abi: Box<AbiHost>`/`env: Box<DriverEnv>`/`data: ErlDrvData`). The library is an **`Arc`** so the process-global
    driver table can keep one loaded object registered while ports are created from it (and several ports of one
    driver share a single `dlopen` handle; `DriverLibrary::drop` — `finish` + `dlclose` — runs when the last owner
    goes). `DriverPort::load(...)` calls `beam_abi::driver::loader::load` (validating `command` **first**, so an
    interior NUL is `BadArg`, not a load failure), mapping `DriverLoadError` to
    `BeamError::Raise(Raise::error(ExitReason::atom(err.reason_atom())))`
    (`unspecified_error` for an unopenable object; `no_driver_init`/`bad_driver_name`/`driver_incorrect_version`/
    `driver_init_failed`); `DriverPort::from_library(Arc<DriverLibrary>, CString, host)` wraps an **already
    loaded** object without a second `dlopen` (the registry's port-creation path; it does **not** re-run `init`).
    `env` + `abi` are **both boxed** so the
    `ErlDrvPort` a driver sees is the **same pointer for the port's whole life** (the C guarantees
    `&prt->drv_port` never moves).
  - `poll_async` — drains the async jobs, sorts by queue position (deterministic, matching the C's queue order),
    runs `ready_async` when the entry has one else the driver's `async_free`; `beam_rt` calls this from the
    port-task round. `make_loaded(path, name, command, host) -> Result<Box<dyn Driver>>` is the built-in/`.so`
    unification helper (see *Design Decisions*).
  - `Driver` mapping: `name` = the entry's name; `flags` = `SOFT_BUSY` iff `ERL_DRV_FLAG_SOFT_BUSY` (other
    `ERL_DRV_FLAG_*` bits have no counterpart); `start` (absent slot or any of
    `ERL_DRV_ERROR_GENERAL`/`ERRNO`/`BADARG` ⇒ `BadArg`, else keeps the `ErlDrvData`); `output`/`ready_output`
    (drain the port output queue; an absent `ready_output` falls back to `output`; drained bytes are treated as
    written — the C callbacks are `void`); `ready_input` (the fd armed for reading, else null; absent ⇒ no-op);
    `timeout` (call, then clear `deadline`); `control` (the `&str` command is parsed as a **decimal `c_uint`** — a
    C driver's verbs are integer codes like `INET_REQ_*`; absent slot or `cres < 0` ⇒ `BadArg`; reply copied from
    `*rbuf`, `n` bytes, into a `CONTROL_REPLY_CAPACITY = 4096` buffer); `next_timeout` = `state.deadline`;
    `port_exit` → `process_exit` (null `*mut ErlDrvMonitor`) if present, else `emergency_close` if present, else a
    no-op; `take_exit_status` = the default `Ok(None)`; `stop` (absent ⇒ no-op).
- `table.rs` — the **process-global driver registry and its port-creation service** (the `beam_io` half of the four
  `ErlDrvApi` slots, reached from a driver through [`AbiHost`](src/abi/host.rs)). `unsafe`-marked (it stores/derefs
  `*mut ErlDrvEntry` and calls the entry's `init`); the one `unsafe impl Send for Inner` (mutex-serialized) carries
  a rationale.
  - `DriverTable` — `global()` (the C's one `driver_list`; `OnceLock`) and `new()` (an isolated table, for tests).
    `add_entry(*mut ErlDrvEntry)` (**`unsafe`**; the C's `add_driver_entry`) records a **statically linked** entry
    after running its `init` (`Option<fn() -> c_int>`; non-zero ⇒ the `driver_init_failed` raise and **no**
    registration, matching `erts_add_driver_entry`'s "remove it again"); a null entry/name ⇒ `BadArg`.
    `add_loaded(DriverLibrary)` records a **dynamically loaded** driver (`entry()` + `.name()` + `Arc<DriverLibrary>`
    as the handle) and does **not** re-run `init` (the load path already did). `remove_entry(*mut ErlDrvEntry)`
    (safe — address comparison only) answers the C's `-1` (dynamic: *"Not allowed for dynamic drivers"*) / `1`
    (removed) / `0` (not found), newest match first. `lock_driver(name)` answers `0` (locked; has a handle) or `-1`
    (no handle = statically linked, or unknown name). `lookup(name) -> Option<RegisteredDriver>` (newest wins, the
    C's head-inserted list) / `names()` / `len()` / `is_empty()`. `create_port(&mut PortRegistry, &mut Poller,
    name, command, host) -> Result<PortId>` resolves the registered driver, builds a `DriverPort::from_library`
    and opens it through `PortRegistry::open` (so `start` runs and supplies the instance's `ErlDrvPort`); the table
    lock is **released before** `start`, so a driver may re-enter the table.
  - `RegisteredDriver { name, entry: *mut ErlDrvEntry, dynamic, locked }` — the read-only snapshot.
  - **Documented deviation:** the C's `driver_create_port` re-uses the *creating port's* driver instance and stores
    the caller's `drv_data` verbatim; here a **fresh** instance is created by running `start` (this crate has no
    shared driver instance across ports), so no `drv_data` operand is accepted — it is `start`'s return.
  - **M1 gap:** `create_port` needs a [`Driver`] adapter, which only exists over a loaded `DriverLibrary`; a
    **statically linked** registration therefore answers the typed `notsup` raise (never a wrong port).

## Constraints
- Errors: **`beam_common::{Result, BeamError}` only**; I/O/syscall failures ⇒ `BeamError::Io`, argument faults ⇒
  `BeamError::BadArg`, id/registration exhaustion ⇒ `BeamError::SystemLimit`. **Never panic** on runtime data,
  never `unwrap()`, never `todo!`/`unreachable!` in shipped code.
- **Safe Rust by default.** `unsafe` is confined to the raw-fd syscall wrappers (`libc::read`/`write`/`pipe`/
  `fcntl`/`close`) and `mio::unix::SourceFd` construction. Every `unsafe` block carries a preceding `// SAFETY:`
  rationale; `unsafe_op_in_unsafe_fn` stays clean (`--workspace` lint).
- **Port ids are allocated by `beam_process::PortTable`** — do not add a second number allocator. `PortId`
  encoding stays `beam_term`'s internal-port immediate (via `beam_process::PortId`).
- **Single-thread invariant (enforced).** `Port`/`PortRegistry` are **not** thread-safe and this crate has no
  locking: they must be created and driven from **one OS thread**, and concurrent registration/dispatch is
  unsupported. `PortRegistry` carries a `PhantomData<core::cell::Cell<()>>` marker so it is neither `Send` nor
  `Sync`; a `Port` holds a `Box<dyn Driver>` with no `Send`/`Sync` bound, so it is single-threaded too. (Not yet
  enforced on `Poller`/`Waker` — the `Waker` is explicitly meant to cross threads.)
- **The `Driver` trait and `DriverContext` public methods are the frozen driver seam** — do not change their
  signatures. New driver capabilities must be additive with defaults that keep the built-in drivers compiling.
- Files target ~1000 lines, split by responsibility: poller / port / registry / port_task / driver / timer / queue
  / drivers.
- Deps: `beam_common`, `beam_term`, `beam_process`, `beam_abi`, `beam_alloc`, `libc`, `mio` only. No new deps
  (see *Design Decisions* for the `beam_io → beam_abi`/`beam_alloc` edge the C-ABI host layer requires).

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` **GREEN**, and
`... build --workspace` **GREEN, warning-free**. Current: **231 tests, 0 failures** (1 ignored doc-test) — `170`
lib (`#[cfg(test)]` in-module: poller, timer, queue, driver, port, port_task,
`drivers/{fd,line_buffer,binary_io,efile,spawn,inet}`, `abi/{host,port,table}`) + `tests/abi_driver.rs` `8` +
`tests/poller.rs` `8` + `tests/port.rs` `4` + `tests/port_task.rs` `4` + `tests/spawn.rs` `18` + `tests/inet.rs`
`3` + `tests/driver_seams.rs` `5` + `tests/fixture_builds.rs` `4` + `tests/driver_table.rs` `5` + `2` doctests (the
`!Send`/`!Sync` compile-fail pins on `PortRegistry`).
- `tests/fixture_builds.rs` — builds the `cc`-compiled `tests/fixtures/host_driver.c` and asserts a real
  `beam_abi::driver::loader::load`; a missing object is a typed load error; missing `cc`/`erl_nif.h` SKIPs loudly.
- `tests/driver_table.rs` — **the process-global registry, end-to-end over the real fixture**: `add_loaded` (the
  load path's `erts_add_driver_entry`) → `DriverTable::create_port` (the C's `driver_create_port`) → the driver's
  `start`/`output`/`stop` observed **in order** through the fixture's `log=<path>` callback log, `driver_output`
  reaching the port owner as `PortNotification::Data`, `lock_driver` = `0` + `remove_entry` = `-1` for a dynamic
  driver, a second port from one registration (the shared `Arc<DriverLibrary>`), the distinct `badarg`
  (unknown name) / `notsup` (static entry) / NUL-command failures, and the ordinary reactor drive path
  (`port_command` → a `ready_output` task) on a registry-created port. SKIPs loudly without `cc`/`erl_nif.h`.
- `tests/abi_driver.rs` — **the end-to-end C-driver path**: fixture `.so` → `make_loaded` → a real port over a
  real `Poller` + `PortRegistry` → the port-task round (start-token command parsing, the `output` echo, a
  write-readiness task reaching `ready_output`, `driver_output` → exactly one `send_term`, timer arm/fire/cancel,
  `driver_select`/deselect read → a `Data` notification, a `driver_async` completion driven by
  `PortRegistry::drive_async`, `close` → `stop` + `PortNotification::Closed`). SKIPs loudly without
  `cc`/`erl_nif.h`; a `dlopen` failure PANICS with the `undefined symbol` hint. The binary references
  `beam_abi::driver::drv::driver_api_table()` so `-Wl,--export-dynamic` exports the `#[no_mangle]` `driver_*`
  symbols the fixture's `dlopen(RTLD_NOW)` resolves.
- `src/abi/` in-module suites (18: `host.rs` 10 + `port.rs` 8) — `AbiHost::output` → the port **input** queue and
  the no-context `-1`; `driver_select` register/**re-register in place**/de-select on a real socketpair; timer
  arm/report/cancel + the immediate `Some(0)`; `driver_async` invoke + one drained completion + null-invoke
  fire-and-forget; the `PortHost` defaults and `LocalAtomPortHost` interning; `interest_from_mode`;
  `DriverPort::load` failures (a typed `Raise`; interior NUL ⇒ `BadArg`); `driver_flags`' SOFT_BUSY-only mapping +
  the `start` sentinels; built-in vs loaded both a `Box<dyn Driver>`.
- `tests/poller.rs` — registration + readiness on a real OS pipe/socketpair (`key`, poll-with-no-events,
  `reregister` direction flip, `deregister`, double-register errors) + the waker (blocking interrupt, same/other
  thread, shareable clones).
- `tests/port.rs` — `open`→`output`→`poll`→`handle_events`→`read_input` round-trip; `close` releases the id and
  empties the poller; `open_named` registry lookup (unknown name ⇒ `BadArg`); unknown keys (incl. `WAKER_KEY`).
- `tests/port_task.rs` — scheduled OUTPUT+INPUT end-to-end, a **pure EOF** event still reaching the driver, the
  reduction budget bounding a round, and the **wake path** (`PortNotification::Data` → `deliver_simple`
  (`DeliverOutcome::Woke`) → `Scheduler::resume_woken`).
- `tests/spawn.rs` — **real child processes**: `echo` stdout → input queue, the argv / `{spawn_executable, Path}`
  + `{args, …}` forms, `cat` echoing through the output queue and the `PortNotification` seam, EOF deregistration,
  a stalled write staying queued and arming write readiness, the reaped `exit_status` taken once (never for a port
  without the option), an unknown id ⇒ `BadArg`, `stop` killing **and** reaping a long-lived child, the duplex
  `{fd, N}` cases (adopted read/write ends; one `UnixStream::pair()` end both directions), a 200 KiB **stderr**
  flood still delivering stdout, a failed spawn registering nothing, an `erl`-oracle cross-check of the
  failed-spawn reason atom, and `{cd, …}`/`{env, …}` reaching the child.
- `tests/inet.rs` — real loopback TCP: a byte round-trip both ways, a stalled write arming write readiness, unknown
  verb ⇒ `BadArg`, and an `erl` **oracle cross-check** of the `INET_REQ_OPEN`/`BIND`/`LISTEN`/`TCP_REQ_SHUTDOWN`/
  `RECV`/`CLOSE` reply bytes (OTP 29.1.1 / erts-17.1; skips loudly without `erl`).
- `tests/driver_seams.rs` — a driver-declared deadline armed with its `Driver::timeout` firing exactly once after
  it, never before; a `control` re-arming/clearing the port timer;
  `PortRegistry::{schedule_timeout,cancel_timeout}` incl. re-pointing; `notify_port_exit` reaching the driver once
  with the right `PortExitReason` while a later `close` still runs `stop`.
- In-module unit tests cover: `ByteQueue`; `Interest`/`Ready`/`Event`; `Poller` register/deregister + `WAKER_KEY` +
  wakers + the **regular-file fallback** (readiness + `poll(None)` promptness, the mio+fallback merge, fallback
  `reregister`, independent entries); `Timeout`/`Timer`; `DriverRegistry` (all three built-ins, dup rejection,
  missing-name `BadArg`); `PortState`; `PortRegistry` alloc/close/reuse + unknown-id `BadArg`; `FdDriver` (pipe
  round-trip, EOF + the EOF de-registration, error paths, `stop` deregistration, fd closing on `Drop`+`stop`);
  `LineBuffer`/`PhaseChange`/`EfileDriver` framing and control + a regular-file open→read-to-EOF→close and write;
  port-task ordering/reductions/dedupe + timer re-pointing; the driver-seam defaults and the `PortExitReason`
  vocabulary; the errno→atom mapping; the `tcp_inet` verb/reply-shape helpers (packed-address decoding,
  `errno_name`, `from_socket` state, the loopback round trip); the failed-`start` teardown; `SpawnDriver`/
  `build_command` (the four name forms, `{cd, …}`/`{env, …}`, empty argv/negative fd `BadArg`, the unknown-program
  constructor error, the hand-built-`DriverContext` read path, the once-only `exit_status`, `128 + signal`,
  idempotent kill/reap).

## Design Decisions
- **Backend: `mio` (lib) vs raw epoll.** `mio` gives a portable, safe `Poll`/`Registry`/`Waker` and a one-shot
  re-arm model matching the C's `EPOLLONESHOT`/`EV_ONESHOT`. The public API is **mio-independent** (registration by
  raw `RawFd` via `Interest`/`Token`/`Ready`), so a future io_uring/`/dev/poll` backend swaps in without touching
  callers; `io_uring` is a documented alternative, not M1.
- **Async notification = one `mio::Waker` per `Poller`, shared via `Arc`.** `mio` permits exactly one `Waker` per
  `Poll`, so the `Poller` creates it once in `new()`; `Poller::waker` returns a cheap `Arc` clone
  (`Waker(Arc<mio::Waker>)`), making `waker()` idempotent and safe to drop.
- **Who owns the output queue.** The `Port` (not the driver) owns both `ByteQueue`s, mirroring the C `ioq`.
  `Port::output` appends and calls `Driver::output`, which **pulls** from `ctx.output()`; `EAGAIN` leaves the
  remainder queued and the driver arms `WRITABLE` (no double buffering).
- **Port-task execution lives in `beam_io`; the scheduler thread lives above it.**
  `PortTask`/`PortTaskKind`/`PortTaskQueue`/reductions model `erl_port_task.h`; `PortRegistry::execute_port_tasks`
  runs a round. The poll loop, port run queues and scheduler-charged reductions belong to `beam_process`/`beam_rt`.
  `PortNotification` is the seam so the runtime (not this crate) turns port output into messages and wakes the
  owner via `beam_process` — keeping the DAG clean and the engine/runtime split intact.
- **Readiness ⇒ task, gated by `PortArms`.** The poller keys every registration by port number, so an `Event` names
  the port but not which of its fds fired; `DriverContext` records the armed directions in `PortArms` so a full
  close (`EPOLLHUP` ⇒ `read_closed` **and** `write_closed`) does not schedule a bogus OUTPUT task on a read-only
  driver — restoring what the C gets from per-event task handles (`ErlDrvEvent`). **Documented divergence:** the C
  keeps three FIFO queues with no cross-kind priority; this crate uses one queue ordered by kind priority
  (INPUT → OUTPUT → TIMEOUT) with enqueue sequence as FIFO tie-break — observable only in the order two *different*
  kinds run within one round.
- **`handle_events` schedules *and* runs the default round** (preserving "driving the reactor makes ports
  progress"); callers wanting full budget control use `schedule_events` + `execute_port_tasks`. Unlike
  `execute_port_tasks` it does **not** harvest `PortNotification`s (input bytes stay in the port's queue, reachable
  via `Port::drain_input`; closes are not reported) — only `execute_port_tasks` yields the `Data`/`Closed`
  notifications a runtime turns into `{Port, {data, Data}}`/port-exit messages.
- **Driver = Rust trait; the C-ABI bridge is a `Driver` over a loaded `ErlDrvEntry`.** A built-in driver
  implements `Driver` directly; the frozen `ErlDrvEntry`/`ErlDrvApi` (`beam_abi::driver`) is an *ABI*, not a Rust
  interface, so `src/abi/` adapts it: `DriverPort` implements `Driver` over a `DriverLibrary` (`start→init/start`,
  `output→output/outputv`, `ready_input→ready_input`, `ready_output→ready_output`, `control→control`,
  `timeout→timeout`, `stop→stop`, `driver_select` ⇒ `DriverContext::register`), changing no
  `Driver`/`Port`/`PortRegistry` signature.
- **The host layer is one boxed `AbiHost` behind a stable pointer; the live port is reached through an installed
  `DriverContext`.** A `DriverHost` method runs **inside** a driver callback, where the port exists only as that
  borrowed context; with no context installed every port-touching method fails gracefully (`-1`/`None`/no-op)
  rather than dereferencing null.
- **The `ErlDrvPort` is `DriverEnv::as_port()`, stable for the port's whole life.** Both the `AbiHost` and the
  `DriverEnv` in a `DriverPort` are **boxed**, so the pointer a driver receives never moves (the C's
  `&prt->drv_port` is likewise stable); `DriverEnv` is lifetime-erased (one `*mut dyn DriverHost`) and the owned
  `AbiHost` keeps it valid as long as the `DriverPort` lives.
- **`driver_output` is harvested by a non-empty input queue, not by an `INPUT` task.** `AbiHost::output` pushes
  onto the port's *input* queue; `Port::take_input_notification` reports `Some(PortNotification::Data)` whenever
  that queue is non-empty (and drains it). A driver may call `driver_output` from `start`/`control`/`timeout`/
  `output`/`ready_input` — *outside* any `INPUT` task — so gating on `INPUT`-task completion would silently drop
  those bytes; reporting on the queue itself delivers every `driver_output` to the owner exactly once per
  port-task round.
- **The port thread owns `async_free` in M1; jobs drain in queue-position order.** The C runs a completed job's
  `async_free` on the **worker** thread; this floor records the completion and lets the port thread decide (it is
  where the driver entry is reachable) — a documented deviation, never a leak (the callback runs exactly once).
  `poll_async` sorts by the `driver_async` queue position, reproducing the C's queue order under concurrency.
- **Built-in and loaded drivers unify at `Box<dyn Driver>`; the `.so` path is a helper, not a registry method.**
  `make_loaded(...)` presents a loaded `.so` as the same `Box<dyn Driver>` `DriverRegistry::create` hands back for
  a built-in, so the poller, the byte queues, the port tasks and the notifications are identical either way. It
  cannot be a `DriverFactory` (a `fn()` pointer) because a loaded driver needs a path/name/command/`PortHost` — the
  same reason `SpawnDriver` is constructed directly. `DriverRegistry`/`open_named` signatures stay frozen.
- **`control` is numeric for a loaded driver.** The built-ins use string verbs (`"close"`, `"line"`, …), but a C
  driver's verbs are the integer `INET_REQ_*`-style codes, so `DriverPort::control` parses the `&str` command as a
  decimal `c_uint`. An absent `control` slot or `cres < 0` is `BadArg` (`io.c:4035`/`io.c:4074`); the reply is
  `cres` bytes at `*rbuf` (our `CONTROL_REPLY_CAPACITY = 4096` buffer, or the driver's own memory valid until the
  next `control` call) — a **trust boundary**, since a driver is native code the runtime loaded.
- **`set_timer` always succeeds; `read_timer` reports `None` only when disarmed.** The C refuses
  `driver_set_timer` when the port has no timer (`if (prt->timeout == NULL) return -1;`), but this port always has
  one (an expired deadline reports `Some(0)`, as the C writes `0`). `beam_abi`'s `driver_read_timer` maps our
  `None` to `-1` — a documented deviation from `io.c:6936`, which answers `0` with `0` for "no timer".
- **`ready_input`/`ready_output` name the fd from the driver's own `driver_select` bookkeeping.** The poller keys a
  registration by **port number**, so a readiness event names the port but not which of its fds fired; the C
  carries that per-`ErlDrvEvent`. `DriverState::selected` (a `BTreeMap`) restores it: M1 passes the stable first fd
  in the wanted direction (fd order), one callback per port-task round — a documented floor for a port with several
  fds selected in the same direction.
- **`PortHost` is the only runtime service this crate cannot supply.** Deliberately minimal (`intern_atom`
  required; `connected_pid`/`caller_pid`/`send_term` defaulted), so a test host only has to intern atoms.
  `beam_rt` injects a real one over its shared atom table; `LocalAtomPortHost` is for tests/standalone use and does
  *not* match the runtime's table.
- **The `beam_io → beam_abi`/`beam_alloc` dependency edge (directionally legal).** Wave 5 (`beam_io`) depends
  downward on wave 0 (`beam_abi`, `beam_alloc`); `beam_abi` is the frozen ABI to adapt and `beam_alloc`'s `Heap` is
  the term arena a driver builds terms on. No cycle, no `*_dev` edge.
- **Port ids reuse `beam_process::PortTable`.** `PortRegistry` wraps it for allocation and adds the
  `HashMap<u64, Port>` keyed by port *number*; a failed driver `start` runs the driver's teardown (so any fd it
  registered is deregistered) and releases the number — the id space and the poller never leak.
- **`Poll` returns owned `Vec<Event>`** (the reactor glue needs `&mut Poller` while iterating events), reusing the
  internal `mio::Events` buffer.
- **Line/phase framing is driver-side.** `{line, N}` and the text⇄binary phase change live in the `FdDriver` (via
  `LineBuffer`/`PhaseChange`), matching `erl_child_setup`: the line flag affects how a port *delivers input to its
  owner*, not how it accepts output. Default phase = Binary, so nothing changes unless toggled.
- **Exit status travels through a defaulted trait method, not a notification variant.** The runtime must be able
  to obtain a child's reaped status **once** for a port opened with `exit_status`, without reaching into driver
  internals: a defaulted `Driver::take_exit_status() -> Result<Option<i32>>` (= `Ok(None)`, so
  `FdDriver`/`EfileDriver` compile unchanged) plus the additive `PortRegistry::take_exit_status(id)` accessor —
  `Ok(None)` means "no child", "not reaped yet" or "already reported"; `Result` rather than a bare `Option` so a
  failing `wait` is reported instead of swallowed. A `PortNotification` variant is rejected: exit status is not a
  *task-round* outcome (it is neither readiness- nor reduction-driven, exactly why the reference runtime polls it
  between stream rounds), and a variant would force every round to carry a status the port may not owe.
- **The spawn driver is constructed, not registered — and reaps its child eagerly.** A `DriverFactory` is a
  no-arg `fn`, so it cannot carry the command/argv/options a spawn port needs; the driver is built directly from a
  `SpawnSpec` (its `Driver::name()` stays `"spawn"` for `port_info`). The child is started in the **constructor**
  so a failed spawn leaves nothing registered and the runtime can read `os_pid()` before `PortRegistry::open`.
  `{fd, N}` adopts one descriptor as a **duplex** stream (one `Interest::BOTH` registration; `arm_output`/
  `disarm_output`/`release_stdout` are no-ops for it) and never closes it (the Erlang side owns it, matching the
  reference runtime's `LivePort::close`). `stop`/`Drop` kill **and** reap (and `Drop` reaps because Rust's `Child`
  does not: dropping it otherwise leaks a zombie).
- **Regular files ride a `poll(2)` fallback inside the poller.** `epoll` cannot watch a regular file, so `Poller`
  keeps a second, `libc::poll(2)`-based registration set for descriptors `mio` rejects and merges its readiness
  into `Poller::poll` — reproducing the reference runtime's `poll(2)` readiness path ("a regular file is trivially
  always ready", `erts/emulator/sys/common/erl_poll.c`) without changing the public `Poller`/`Port` API or adding a
  dependency, and without giving up the edge-triggered `mio` model for the descriptors it serves. Because the
  fallback is level-triggered, `FdDriver` de-registers a read fd at EOF.
- **A failed spawn raises the errno atom.** The reference raises `erl_errno_id(errno)` from
  `erts_internal_open_port_2` (`erts/emulator/beam/erl_bif_port.c`), so a direct-exec spawn failure is a
  `BeamError::Raise` carrying the errno's lowercase name; `BeamError::Io` stays the unmapped-errno fallback.
- **The child's stderr is discarded, not piped** — a piped-but-undrained stderr deadlocks a child that writes past
  the pipe capacity, and a child's stderr is not port data (the oracle does not deliver it either), so
  `build_command` sends it to `/dev/null`. `SpawnSpec`/`SpawnOptions` carry no `stderr_to_stdout` option.
- **The timer seam is driver-declared and mirrored; the port-exit channel is advisory and once-only.**
  `Driver::next_timeout` is mirrored into the single `TIMEOUT` task after each interaction (deadline-gated, never
  early), with `PortRegistry::{schedule_timeout,cancel_timeout}` as the runtime's explicit override;
  `Driver::port_exit` fires at most once and is distinct from the teardown `stop`. Both are additive defaults, so
  every existing driver compiles unchanged.
- **The TCP driver is a floor, not a full `inet`** — see the module docs for the deferred verbs.

## Known Issues
- **The C-ABI host layer is a floor; a driver must not stash `ErlDrvPort` beyond the callback in M1.** The port
  context is installed per callback (`install`/`clear` around each `DriverPort` call), so a driver that keeps the
  `ErlDrvPort` handle and calls `driver_*` from outside a callback (e.g. its own worker thread) finds no context
  and gets the C's failure code (`-1`). The handle *pointer* is stable and valid for the port's whole life; it is
  the **installed context** that is callback-scoped. `ready_async` receives no port (matching `erl_driver.h`), so a
  driver that needs its port there uses the handle it stashed in `start` — which works because `poll_async` runs
  inside a `DriverPort` method with the context installed.
- **`driver_async` has no `PortTaskKind::Async`: `beam_rt` must call `PortRegistry::drive_async(poller, id)` once
  per port-task round for a port with pending jobs to progress** (the analog of the C's `call_async_ready`).
  Completed jobs reach `ready_async`/`async_free` only there — no readiness- or reduction-driven task does it for
  you (a `PortTaskKind::Async` is a documented follow-up).
- **`beam_rt` must inject a real `PortHost` (handoff).** A driver that builds a term with `erl_drv_output_term`/
  `erl_drv_send_term`/`driver_mk_atom` gets its atom from `PortHost::intern_atom`; a locally-interned atom
  (`LocalAtomPortHost`) does **not** match the runtime's atom table, so such a term would compare unequal to the
  same atom built by running code — a silently wrong result. `beam_rt` owns the shared `beam_term::atom::AtomTable`
  (`beam_engine`'s `ExecutionContext`/`beam_bifs`' `AtomInterner`), so it supplies `PortHost` (interning into it,
  and `connected_pid`/`caller_pid`/`send_term` over the process/message path).
- **`ErlDrvApi` slots that are served vs. still deferred (handoff to `beam_abi`).** The four
  **driver-registry** slots are served: `add_driver_entry`/`remove_driver_entry` delegate to this crate's
  process-global [`DriverTable`](src/abi/table.rs) through [`AbiHost`](src/abi/host.rs),
  `driver_lock_driver` answers `0` for a port whose identity is a `dlopen`ed driver (the C consults the port's own
  driver, not `driver_list`) and the C's `-1` for a bare/statically linked host, and `driver_create_port` answers
  `ERTS_INVALID_ERL_DRV_PORT` — the C's single failure value, never a fabricated port (a driver callback already
  holds a `&mut` into the port registry, and a new port would need a `Box<dyn PortHost>` only `beam_rt` can
  supply). The remaining slots below are `None` in `beam_abi`'s
  `DRIVER_API` because `beam_abi`'s `DriverHost` seam has no method for them; each group lists the `DriverHost`
  methods `beam_abi` must add before the slot can be implemented:
  - **port queues** — `driver_vec_to_buf` + the `driver_enq`/`driver_pushq`/`driver_deq`/`driver_sizeq`/
    `driver_peekq*`/`driver_enqv`/`driver_pushqv` family. Needs a port-queue group: `enq`, `enq_bin`, `pushq`,
    `pushq_bin`, `deq`, `sizeq`, `peekq`, `peekqv`, `enqv`, `pushqv`, `vec_to_buf`.
  - **port lifecycle & attributes** — `erl_drv_init_ack`, `erl_drv_set_os_pid`, `erl_drv_busy_msgq_limits`, the
    `driver_failure*`/`driver_exit` family, `set_busy_port`, `set_port_control_flags`, `get_port_flags`,
    `driver_async_port_key`, `driver_get_now` (deprecated), `driver_binary_get_refc`/`inc_refc`/`dec_refc`. Needs
    `init_ack`, `set_os_pid`, `busy_msgq_limits`, `failure_eof`/`failure_atom`/`failure_posix`/`failure`, `exit`,
    `set_busy_port`, `set_port_control_flags`, `get_port_flags`, `async_port_key`, `get_now`,
    `binary_get_refc`/`binary_inc_refc`/`binary_dec_refc`.
  - **monitors & port data lock** — the `driver_monitor_*` family needs `monitor_process`/`demonitor_process`/
    `get_monitored_process`/`compare_monitors`; the whole `driver_pdl_*` family needs a port-data-lock accessor
    group.
  - **async thread pool** — `driver_async_port_key` (the "fair" async key derives from the port's id term, which
    the current seam has no representation for).
  - **threads** — the whole `erl_drv_mutex_*`/`erl_drv_cond_*`/`erl_drv_rwlock_*`/`erl_drv_tsd_*`/
    `erl_drv_thread_*`/`erl_drv_*_name` family. Needs `mutex_*`/`cond_*`/`rwlock_*`/`tsd_*`/`thread_*` plus the
    `*_name` accessors.
  - **deliberately `None`** — the binary-refcount trio (`driver_binary_get_refc`/`inc_refc`/`dec_refc`): the
    frozen `ErlDrvBinary` is exactly `{ orig_size, orig_bytes }` (16 bytes) with no refcount word, so there is
    nothing to report or adjust.
- **Linking the C driver fixture requires BOTH `build.rs`'s `-Wl,--export-dynamic` link-arg and a
  `driver_api_table()` reference from the test binary** (either alone leaves the fixture's `dlopen(RTLD_NOW)`
  failing with `undefined symbol: driver_mk_port`). The end-to-end run (fixture → `load` → `make_loaded` →
  `PortRegistry::open` → `execute_port_tasks` + `drive_async`) is `tests/abi_driver.rs`; `tests/fixture_builds.rs`
  asserts the real load. `src/abi/` is otherwise covered by in-module tests over a real poller/socketpair.
- **A driver's own worker thread has no installed `DriverContext` in M1.** The context is installed only around a
  `DriverPort` callback, so a `driver_output` from a plain `std::thread` returns `-1` and its bytes are dropped;
  only the completion hand-off (`ready_async`/`async_free`) is driven, via `Driver::drive_async`.
- **Single-OS-thread only.** No dedicated poll threads, no `ThreadProgress`/generation handoff, no
  `ERTS_POLL_USE_WAKEUP` beyond `Waker`; the single-thread invariant is enforced on `Port`/`PortRegistry` (see
  Constraints) but **not** guarded on `Poller` itself — use it from one thread.
- **Readiness is edge-triggered** (`mio` registers with `EPOLLET`), so a driver *must* drain to `EAGAIN` on every
  callback — `FdDriver` reads/writes in an `EAGAIN`-terminated loop for exactly this reason. Readiness is only
  scheduled for directions the driver armed (`PortArms`), so a driver that never registers a direction never sees
  its events.
- **This crate has no poll thread of its own.** `execute_port_tasks` runs a round on demand; the per-iteration
  drive (poll → `schedule_events`/`handle_events` → `execute_port_tasks`), the reduction budget and the single
  long-lived `Poller` live in `beam_rt`'s single-threaded main loop. No port run queue, no dedicated poll thread.
- **`FdDriver` is deliberately minimal.** Byte-stream only; line mode frames input but does **not** line-frame
  output (matching the emulator's line flag). No `inet`/`forker` drivers; no packet framing.
- **`SpawnDriver` is the OS-child floor, not the whole `spawn` driver.** Decoding the `open_port/2` terms into a
  [`SpawnSpec`](src/drivers/spawn.rs)/`SpawnOptions` (and synthesizing `{Port, {data, …}}`,
  `{Port, {exit_status, …}}` and a `{Port, eof}` message from them) stays the runtime's job — this crate only moves
  the bytes and kills/reaps the child. The child's stderr is discarded (`Stdio::null()`); the driver signals EOF
  only by latching its own state (`eof()`), it does not report an event. Writing to a child that closed its stdin
  fails with a typed `BeamError::Io` (the queued remainder is kept, never silently discarded), and `{fd, N}` is a
  **duplex** adoption — one descriptor behind one `Interest::BOTH` registration, read and written by the port
  (end-to-end `port_command` to a `{fd, N}` port is still intercepted as `unsupported` in `beam_rt`, a handoff).
  Adopting a descriptor also requires it to be switchable to non-blocking (`F_SETFL` + `O_NONBLOCK`); a descriptor
  that refuses that (some inherited descriptors return `EPERM`) fails the open with `BeamError::Io`, because the
  reactor is edge-triggered and cannot serve a blocking descriptor. Input is not line-framed (there is no `line`
  control verb): the `binary`/`list` choice rides on `SpawnStream` for the runtime to apply.
- **`port_control`/control atoms are not decoded here.** `port_control(op, &[u8])` takes a `&str` op + raw bytes;
  the C decodes Erlang tuples in `io.c` — atom/tuple decoding of `{command, …}`/`{connect, …}` is the runtime's
  job.
- **`mio` fd registration is `os-ext`/Unix-only** (`mio::unix::SourceFd`); Windows/iocp is out of scope (as in
  `beam_abi`).
- **Residual `open_port` divergences.** `open_port({spawn_executable, "/etc"}, [])` (a **directory**) opens a port
  in the oracle (`ok`) but this crate's synchronous `Command::spawn` reports `eacces`; and `open_port({spawn,
  ""}, [])` / `{spawn_executable, ""}` is `badarg` in the oracle (the runtime's name validation) while the driver
  would map an empty name to `enoent`. Both are open handoffs (an `access(X_OK)` precheck / `beam_rt` name
  decoding).
- **The `tcp_inet` driver is a floor.** The async-op queue, per-op `driver_set_timer` timeouts, the second-port
  `accept`, IPv6/UDP/SCTP/Unix-domain sockets, `{active,…}` modes and DNS are absent; `recv` rejects a
  timeout/length argument with `BadArg`. The verbs it does cover match the C's reply shapes (oracle-cross-checked).
- **File sizes (source vs. in-module tests).** `src/port.rs` is 610 lines of source (the `Port` endpoint + its
  `PortRegistry` re-export; no in-module tests) and `src/registry.rs` is 1080 (≈490 source + ≈590 tests) — both
  sources are under the ~1000 baseline. `src/drivers/inet/mod.rs` is ≈991 source with its wire vocabulary in
  `src/drivers/inet/wire.rs` (182). `src/drivers/spawn.rs` is 1071 (≈793 source + ≈278 tests) — over 1000 only
  because of its in-module suite; if it grows, split the term-free spec
  (`SpawnSpec`/`SpawnOptions`/`SpawnStream`/`build_command`) into a sibling module and keep the driver in
  `spawn.rs`. The `tcp_inet` raw socket-syscall helpers
  (`socket_fd`/`accept_socket`/`has_peer`/`local_port`/`is_listening`/`socket_error`/`shutdown_socket`/`close_fd`)
  could move to a third inet sibling if that file grows past baseline.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/poller.rs` → `Poller`, `Token`, `Interest`, `Ready`, `Event`, `Waker` (the `mio`-backed reactor)
- `src/timer.rs` → `Timeout`, `Timer` (poll-deadline primitives)
- `src/queue.rs` → `ByteQueue` (the port input/output byte buffers)
- `src/port_task.rs` → `PortTaskKind`/`PortTask`/`PortTaskQueue` (incl. `arm_timeout`), reduction constants, `Readiness`, `PortNotification`, `PortTaskOutcome` (the `erts_port_task_execute` model)
- `src/port.rs` → `Port`, `PortState`, `PortControl`, `PortCommandOptions`, `DEFAULT_READINESS_RED_BUDGET` (the port endpoint, its input/output queues, driver dispatch and the driver-timer mirror surface `Port::{arm_timeout,cancel_timeout,next_timeout,notify_exit}`); re-exports `PortRegistry`
- `src/registry.rs` → `PortRegistry` (port-id allocation over `beam_process::PortTable` + the `HashMap<u64, Port>` of live ports + the reactor glue `handle_events`/`schedule_events`/`execute_port_tasks`/`port_command`/`port_control` + the timer/exit/async seams `schedule_timeout`/`cancel_timeout`/`next_deadline`/`notify_port_exit`/`drive_async`)
- `src/driver.rs` → `Driver` trait (incl. the defaulted `take_exit_status` exit-status seam, the `next_timeout` driver-declared timer seam and the `port_exit` port-exit notification seam), `PortExitReason`, `DriverContext`, `DriverFlags`, `DriverRegistry`, `pub(crate) PortArms` (the driver seam)
- `src/drivers/fd.rs` → `FdDriver` (the built-in fd/pipe byte-stream driver, incl. line/phase modes + `control` + EOF de-registration)
- `src/drivers/line_buffer.rs` → `LineBuffer`, `DEFAULT_MAX_LINE_LEN` (`{line, N}` framing)
- `src/drivers/binary_io.rs` → `Phase`, `PhaseChange` (the text⇄binary phase-change protocol)
- `src/drivers/efile.rs` → `EfileDriver`, `make_efile` (the built-in `efile` driver; serves regular files via the poller fallback)
- `src/drivers/spawn.rs` → `SpawnDriver` (the OS-child `spawn` driver, incl. the errno-atom failed-spawn raise and the duplex `{fd, N}` form), `SpawnSpec`/`SpawnOptions`/`SpawnStream` + `build_command` (the term-free `open_port/2` name/options model the runtime decodes into)
- `src/drivers/inet/mod.rs` + `src/drivers/inet/wire.rs` → `TcpInetDriver`/`make_tcp_inet`/`DRIVER_NAME` (`"tcp_inet"`), `InetState` (the driver state machine + socket syscalls) and the `INET_*`/`TCP_SHUT_*`/`INET_REP_*` codes + packed-address helpers + `errno_name` (the wire vocabulary)
- `src/drivers/mod.rs` → driver-module wiring + re-exports
- `src/abi/mod.rs` → the driver C-ABI host-layer module docs + wiring (`pub mod host/job/port/table` + re-exports) incl. the `beam_abi` wiring the four global `driver_*` slots still need
- `src/abi/host.rs` → `PortHost` (the runtime atom-interning/pid/term-sink hook) + `LocalAtomPortHost` (test host) + `DriverState`/`SelectedFd` (persistent `driver_select`/timer/async state) + `AbiHost` (the `beam_abi::driver::host::DriverHost` impl over the poller/queues/timer)
- `src/abi/job.rs` → `AsyncJobs` (the `driver_async` worker/bookkeeping) + `AsyncCompletion`/`AsyncData`
- `src/abi/port.rs` → `DriverPort` (a loaded `ErlDrvEntry` as a `Driver`) + `DriverPort::load`/`from_library`/`poll_async` + `make_loaded` (built-in/`.so` unification) + `driver_flags`/`is_start_failure`/`load_failure`
- `src/abi/table.rs` → `DriverTable` (the process-global driver registry: `add_entry`/`add_loaded`/`remove_entry`/`lock_driver`/`lookup`/`names` + the `create_port` service) + `RegisteredDriver`; `entry_identity` (the name/`init` reader)
- `src/error.rs` → `pub(crate)` error constructors + the errno→BEAM-atom mapping (`errno_atom`) and `spawn_failure` (no new error type)
- `tests/poller.rs` → registration + readiness on a real OS pipe/socketpair + timeout/waker
- `tests/port.rs` → `open`→`output`→`poll`→`handle_events`→`read_input` byte round-trip
- `tests/port_task.rs` → port-task scheduling end-to-end + the receive-wake path (`PortNotification` → `beam_process` scheduler)
- `tests/spawn.rs` → real child processes end-to-end (stdout→input queue, stdin→output queue, notifications, EOF, a stalled write arming write readiness, once-only exit status, kill+reap, the duplex `{fd, N}` adoption, a stderr flood, a failed spawn with the errno-atom + an `erl`-oracle reason cross-check)
- `tests/inet.rs` → the `tcp_inet` driver on real loopback TCP (round-trip, stalled write arming write readiness, unknown-verb `BadArg`, `erl`-oracle reply-byte cross-check)
- `tests/driver_seams.rs` → the driver-declared timer (`Driver::next_timeout` → `Driver::timeout`) + the once-only `PortRegistry::notify_port_exit` seam
- `tests/fixture_builds.rs` + `tests/fixtures/host_driver.c` → the real `cc`-built C driver fixture and its load assertion
- `tests/driver_table.rs` → the process-global registry end-to-end over the real fixture: `add_loaded` → `create_port` → the driver's `start`/`output`/`stop` in order (read back from the fixture's `log=<path>` callback log) + `driver_output` → port data, a second port from one registration, the `badarg`/`notsup`/NUL-command failures, and the ordinary reactor drive path (`port_command` → `ready_output`)
- `../beam_process/src/ptab.rs`, `scheduler.rs`, `process.rs` → `PortTable`/`PortId`, `Scheduler::resume_woken`, `ProcessTable::deliver`/`drain_woken`, `DeliverOutcome` (sibling — read-only)
- `../beam_rt/src/port.rs` → the reference `open_port/2` term decoding (`RuntimePorts::{name_spec, command_spec, port_options, env_list, arg_list, spawn}`) that `SpawnSpec`/`SpawnOptions`/`build_command` mirror term-free (sibling — read-only)
- `../beam_abi/CONTEXT.md` → the frozen `ErlDrvEntry`/`ErlDrvApi` driver C ABI that `src/abi/` bridges onto the `Driver` trait (sibling — read-only)
- `../../erts/emulator/sys/common/erl_poll.c`, `../../erts/emulator/beam/io.c`, `../../erts/emulator/beam/erl_port.h`, `../../erts/emulator/beam/erl_port_task.{c,h}`, `../../erts/emulator/drivers/common/inet_drv.c`, `../../erts/emulator/sys/unix/erl_child_setup.c` → legacy C ground truth (read-only reference)

## Notes for Agents
- Read `../CONTEXT.md` (crate-layer contracts, the poller/`Port`/driver-registry seam), `../beam_process/CONTEXT.md`
  (`PortTable`/`PortId`, `Scheduler`/`ProcessTable`) and `../../CONTEXT.md` §1 A.4/§3 before changing this crate.
- Build/test: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` (sandbox `$HOME`
  is read-only — always the wrapper). Never `cd`; never touch `../../tools/`.
- A driver never touches the queues or the poller directly — it goes through `DriverContext`. When a `Port`
  method calls the driver, **destructure `self`** (`let Port { driver, input, output, id, .. }`) so the driver and
  its context borrow disjoint fields; do not add a combined raw-pointer accessor.
- `Poller::poll` returns owned events; call `registry.handle_events(&mut poller, &events)` after it, or drive the
  queue explicitly with `schedule_events` + `execute_port_tasks`. Keys are port *numbers* (`PortId::number()`).
- `FdDriver` closes the fds it is given (on `stop` and on `Drop`); do not close them elsewhere. `EfileDriver`
  composes an `FdDriver`, so the same ownership rule applies through it.
- `mio::unix::SourceFd` borrows `&mut RawFd` only for the duration of the syscall — build it from a local
  `let mut fd = fd;` inside the wrapper; the raw fd number is what mio registers.
- `Ready` has **no public constructor**: tests that need a specific readiness forge it through
  `Event::new(token, key, ready)` with a real `Ready` or through the public `schedule_readiness(key, Readiness)`.

## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types, the Stage-0…6 mapping (§7) and wave-5 position.
- `../../CONTEXT.md` → workspace contracts: §1 A.4 (driver C ABI) / A.6 (CLI/config), §3 (error strategy).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A (the two clean seams) and §H (Stage 5).
- `../beam_process/CONTEXT.md` → `PortTable`/`PortId`, `Scheduler`/`ProcessTable` and the scheduler that drives ports.
- Owed upstream (repo-root owned, outside this node): `../../CHANGE.md` entries for the port-task model +
  `PortNotification` seam + `port_command`/`port_control` surface; the line/phase/`efile` driver additions and the
  `DriverRegistry` `"efile"` registration; the poller `libc::poll(2)` regular-file fallback + the `FdDriver` EOF
  de-registration; the errno→atom failed-spawn raise (`error::errno_atom`/`spawn_failure`); the child-stderr discard
  and the duplex `{fd, N}` adoption; the two defaulted driver seams (`Driver::next_timeout` /
  `Driver::port_exit` + `PortExitReason`); and the `"tcp_inet"` driver registration.
