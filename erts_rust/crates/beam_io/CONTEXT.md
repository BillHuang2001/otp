# `beam_io` — Poller/Reactor, Ports, Port-Task Scheduling, Built-in Drivers

## Intent
Owns the **I/O subsystem** of the Rust ERTS rewrite (Stage 5, Part II §H) and the **OS/driver seam** —
one of the two clean, already-existing rewrite seams (Part II §A): it is the layer that must be
reproduced with the *frozen* driver C ABI on top of it (workspace `../CONTEXT.md` §1 **A.4**), so a
driver `.so` compiled against the unmodified `erl_driver.h` links and behaves identically.

Four things live here:
1. the **event poller/reactor** — a `mio`-backed readiness loop (epoll/kqueue/…) with a
   `Token`→registration map (stage 5's "I/O" row);
2. the **`Port`** — a process-like I/O endpoint: a port id (bit-exact with `beam_term`'s internal-port
   immediate), an **input** byte queue and an **output** byte queue, a lifecycle, and dispatch to a
   driver;
3. **port-task scheduling** — real per-port `INPUT`/`OUTPUT`/`TIMEOUT` tasks with reduction accounting
   (`port_task.rs`), mirroring `erts_port_task_execute`;
4. the **driver interface + built-in drivers** — a Rust `Driver` trait now, with the frozen C-ABI
   `ErlDrvEntry` bridge as documented future work, plus the built-in `fd` and `efile` byte-stream
   drivers, the OS-child `spawn` driver, the TCP `tcp_inet` socket driver, line buffering, and the
   text⇄binary phase-change protocol.

Consumed by `beam_rt` (boot/`system_info`/driver registration) and, through port ids, observed by
`beam_process` (ports are the peer of pids in `ptab`). `beam_rt` **drives its ports through this crate**:
it holds one long-lived `Poller` + `PortRegistry`, constructs a [`SpawnDriver`](src/drivers/spawn.rs) from
its decoded `open_port/2` terms, runs the port-task round (`execute_port_tasks`) from its main loop, and
consumes the [`PortNotification`]s — delivering `{Port,{data,…}}`/`{Port,{exit_status,…}}` to the owning
process and waking a receiver blocked on port data through the `beam_process` scheduler. This crate is the
**library**; the scheduler *thread* wiring (driving a port-task round from the poll loop, port run queues)
lives in `beam_rt` (single-threaded), not here.

Legacy C ground truth (read-only): `../../erts/emulator/sys/common/erl_poll.c` + `erl_check_io.c` (the
poll loop, one-shot re-armed read/write readiness, self-pipe wakeup), `../../erts/emulator/beam/io.c`
(`erts_port_command`/`erts_port_control`, the `ioq` output queue), `erl_port.h`
(`struct _erl_drv_port`, `ERTS_PORT_SFLG_*`), `erl_io_queue.{c,h}` (`SysIOVec` queue),
`erl_port_task.{c,h}` (the asynchronous work unit + reductions), `../../erts/emulator/drivers/common/inet_drv.c`
(the representative fd+poller driver: lazy `driver_select`, EAGAIN leaves data queued), and
`../../erts/emulator/sys/unix/erl_child_setup.c` (the `{line, N}` / binary phase-change behavior).

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_io::prelude::*`.
### `poller.rs`
- `pub struct Token(usize)` (`Copy + Eq + Hash + Ord + Debug`) — an opaque registration handle;
  `pub fn get(self) -> usize`. Token `0` is reserved for the waker; user tokens start at `1`.
- `pub struct Interest` (`Copy + Eq + Debug`) — readiness mask: consts `READABLE`, `WRITABLE`, `BOTH`;
  `fn readable() -> Interest`, `fn writable() -> Interest`, `fn is_readable(self) -> bool`,
  `fn is_writable(self) -> bool`.
- `pub struct Ready` (`Copy + Default + Eq + Debug`) — observed readiness:
  `readable()/writable()/is_error()/is_read_closed()/is_write_closed() -> bool`.
  **No public constructor** — only `Ready::default()` and (in `port_task.rs`) `From<Ready> for Readiness`.
- `pub struct Event` — `token() -> Token`, `key() -> u64` (the caller key it was registered with),
  `ready() -> Ready`; `Event::new(token, key, ready)` lets tests/forgers build one.
- `pub struct Poller` (owns `mio::Poll` **and the single `mio::Waker`** created in `new()`):
  - `pub fn new() -> Result<Poller>`
  - `pub fn register(&mut self, fd: RawFd, interest: Interest, key: u64) -> Result<Token>`
  - `pub fn reregister(&mut self, fd: RawFd, token: Token, interest: Interest) -> Result<()>`
  - `pub fn deregister(&mut self, fd: RawFd, token: Token) -> Result<()>`
  - `pub fn poll(&mut self, timeout: Option<Duration>) -> Result<Vec<Event>>` (owned — a caller can
    then take `&mut Poller` again to re-register; `None` blocks)
  - `pub fn waker(&self) -> Result<Waker>` (an infallible clone of the poller's wakeup handle — safe
    to call any number of times); `pub fn registration(&self, token: Token) -> Option<u64>`
  - `pub fn len(&self) -> usize`, `pub fn is_empty(&self) -> bool`
- `pub struct Waker` — an `Arc`-backed, cloneable handle to the poller's single wakeup handle:
  `pub fn wake(&self) -> Result<()>` (thread-safe, callable repeatedly and from any thread; the
  woken event carries `key() == WAKER_KEY`).
- `pub const WAKER_KEY: u64` — the key of a waker event (`u64::MAX`).
- **Regular-file fallback source (internal, no public signature change).** `mio`/epoll refuses regular
  files (`EPOLL_CTL_ADD` ⇒ `EPERM`), so `Poller::register` keeps a `libc::poll(2)`-backed fallback set:
  a descriptor `mio` rejects as unsupported (`EPERM`/`EINVAL`, i.e. a regular file) is watched with
  `poll(2)` and its readiness is **merged** into `Poller::poll` in the same `Event`/`Ready` shape (a
  regular file ⇒ readable on `POLLIN`, writable on `POLLOUT`). `reregister`/`deregister`/`registration`/
  `len`/`is_empty` treat both sources uniformly; any other `mio` error (`EEXIST`/`ENOENT`/`EBADF`/…) is
  still reported as `BeamError::Io`. `Poller::poll` never blocks indefinitely while the fallback set is
  non-empty (private `FALLBACK_POLL_CAP`, 1 ms; zero wait when the fallback probe already fired).
### `timer.rs`
- `pub struct Timeout` (`Copy`) — a single deadline: `infinite()`, `after(Duration)`, `at(Instant)`,
  `poll_timeout() -> Option<Duration>`, `is_expired() -> bool`.
- `pub struct Timer` — a set of deadlines keyed by `u64`: `new()`, `schedule(key, Instant)`,
  `next_deadline() -> Option<Instant>`, `poll_timeout() -> Option<Duration>`, `pop_expired(now) -> Vec<u64>`,
  `cancel(key) -> bool`, `len()`, `is_empty()`.
### `queue.rs`
- `pub struct ByteQueue` — a byte buffer (`front()` always contiguous): `new()`, `with_capacity(n)`,
  `len()`, `is_empty()`, `push(&[u8])`, `front() -> &[u8]`, `consume(n) -> usize`, `read_into(&mut [u8]) -> usize`, `clear()`.
### `port_task.rs` (port-task scheduling — `erts_port_task_execute`)
- Reduction constants: `CONTEXT_REDS = 4000`, `PORT_REDS_INPUT = PORT_REDS_OUTPUT = PORT_REDS_TIMEOUT =
  CONTEXT_REDS / 100 (40)`, `PORT_REDS_EXECUTE = CONTEXT_REDS / 4 (1000)` (C: `erl_port.h`/`erl_vm.h`).
- `pub enum PortTaskKind { Input, Output, Timeout }` — `priority() -> u8` (Input < Output < Timeout) and
  `reductions() -> usize`.
- `pub struct PortTask` (`Copy + Eq + Debug`) — `kind()`, `deadline() -> Option<Instant>`, `seq() -> u64`,
  `reductions()`, `is_ready_at(Instant) -> bool`.
- `pub struct PortTaskQueue` — one pending task per kind (dedupe), ordered by kind priority then FIFO
  `seq`, with deadline gating for timeouts: `new()`, `enqueue(kind) -> bool`, `enqueue_timeout(deadline) -> bool`,
  `arm_timeout(deadline)` (re-points/replaces a pending `TIMEOUT` — distinct from the dedupe-by-kind
  `enqueue_timeout`, which cannot re-point), `len()`, `is_empty()`, `contains(kind)`,
  `next() -> Option<PortTaskKind>`, `pop_next(now) -> Option<PortTask>`, `remove(kind) -> bool`, `clear()`.
- `pub struct Readiness { readable, writable, error, read_closed, write_closed }` (`Copy + Default + Eq + Debug`),
  with `read_ready()` (readable ‖ read_closed ‖ error), `write_ready()` (writable ‖ write_closed), `is_idle()`,
  and `impl From<Ready> for Readiness`.
- `pub enum PortNotification { Data { port: PortId, bytes: Vec<u8> }, Closed { port: PortId } }` — the seam the
  runtime turns into `{Port, {data, Data}}`/port-exit messages.
- `pub struct PortTaskOutcome { pub ran: usize, pub reds_used: usize, pub notifications: Vec<PortNotification> }`.
### `port.rs`
- `pub enum PortState { Opening, Open, Closing, Closed }`.
- `pub const DEFAULT_READINESS_RED_BUDGET: usize = PORT_REDS_EXECUTE` — the budget `handle_events` runs.
- `pub struct PortControl;` with `pub const PortControl::CLOSE: &'static str = "close"` — the one **reserved**
  control-op name (a namespace marker, not an enum: any other name is a driver verb, see `port_control`).
- `pub struct PortCommandOptions { nosuspend, force }` (`Copy + Default + Eq`) — `EMPTY`, `new()`,
  `with_nosuspend(bool)`, `with_force(bool)`, `is_nosuspend()`, `is_force()`. Milestone-1 ports are never
  busy, so both flags are carried/validated but change nothing (the BIF layer owns their effect once a
  busy-port command queue exists).
- `pub struct Port` — owns a `Box<dyn Driver>` + the two queues:
  `id()`, `state()`, `driver_name()`, `input_len()`, `read_input(&mut [u8]) -> usize`, `drain_input() -> Vec<u8>`,
  `output_len()`, `open(&mut Poller)`, `output(&mut Poller, &[u8]) -> Result<usize>`, `ready_input(&mut Poller)`,
  `ready_output(&mut Poller) -> Result<usize>`, `control(&mut Poller, command, arg) -> Result<Vec<u8>>`,
  `close(&mut Poller)`, `take_exit_status() -> Result<Option<i32>>` (forwards to the driver),
  `notify_exit(&mut Poller, PortExitReason) -> Result<()>` (the once-only port-exit notification), and the
  scheduling surface `queue_output(&[u8])`, `schedule_task(PortTaskKind) -> bool`,
  `schedule_timeout(Instant) -> bool`, `arm_timeout(Instant)`, `cancel_timeout() -> bool`,
  `next_timeout() -> Option<Instant>`, `pending_tasks() -> usize`, `next_task() -> Option<PortTaskKind>`,
  `has_pending_task(PortTaskKind) -> bool`, `timeout(&mut Poller) -> Result<()>`.
  Each driver interaction (`start`/`output`/`ready_input`/`ready_output`/`control`/`timeout`) refreshes the
  port timer from `Driver::next_timeout` through the internal `refresh_timeout`: `Some(d)` arms/**re-points** the
  single `TIMEOUT` task at `d`, `None` clears it (a driver's declaration is authoritative until its next interaction).
- `pub struct PortRegistry` — id allocation (inner `beam_process::PortTable`) + `HashMap<u64, Port>` keyed by
  port number: `new()`, `open(poller, Box<dyn Driver>) -> Result<PortId>`,
  `open_named(poller, &DriverRegistry, name) -> Result<PortId>`, `get(id)`, `get_mut(id)`,
  `output(poller, id, &[u8]) -> Result<usize>`, `contains(id)`, `len()`, `is_empty()`, `close(poller, id) -> Result<()>`, plus:
  - `handle_events(poller, &[Event]) -> Result<usize>` — schedules the implied tasks then runs one round
    under `DEFAULT_READINESS_RED_BUDGET`; returns how many events named a live port.
  - `schedule_events(&[Event]) -> usize` / `schedule_readiness(key: u64, Readiness) -> bool` — enqueue only
    (no execution) so a caller drives the queue itself; readiness ⇒ `INPUT`/`OUTPUT` task per
    `Readiness::read_ready`/`write_ready`, **but only for directions the driver actually armed**
    (`DriverContext` records this in `driver::PortArms`).
  - `execute_port_tasks(poller, reds_budget) -> Result<PortTaskOutcome>` — run queued tasks in port-number
    order under the budget, returning what ran + the notifications.
  - `port_command(poller, id, data, PortCommandOptions) -> Result<()>` — queue output + schedule an
    `OUTPUT` task (the `erlang:port_command` write path; the write happens in the task, not here).
  - `port_control(poller, id, op, arg) -> Result<Vec<u8>>` — the `erts_port_control` seam: the reserved name
    `"close"` closes the port (empty reply); **every other `op`** is forwarded verbatim to the driver's
    `control(op, arg)` and its reply returned (unknown verb ⇒ the driver's `BeamError::BadArg`).
  - `take_exit_status(id) -> Result<Option<i32>>` — the exit-status seam: takes the reaped status of the OS
    child behind `id` **once** (`Ok(None)` for an unknown id's driver with no child, a child that has not
    exited, or a status already reported; `BeamError::BadArg` for an unknown id).
  - `schedule_timeout(id, deadline) -> Result<()>` / `cancel_timeout(id) -> Result<bool>` /
    `next_deadline() -> Option<Instant>` — the runtime's explicit port-timer entry points (`next_deadline` is the
    minimum over live drivers' `Driver::next_timeout`, for clamping a poll timeout); `BadArg` for an unknown id.
  - `notify_port_exit(poller, id, reason: PortExitReason) -> Result<()>` — the port-exit notification channel:
    invokes the driver's `Driver::port_exit` **at most once** per port (a repeat is a no-op), leaves the port live
    with its registrations armed, and is independent of the later `close`/`stop`; `BadArg` for an unknown/released id.
### `driver.rs`
- `pub struct DriverFlags(u32)` — `Copy + Eq`; `empty()`; const `SOFT_BUSY`; `contains(DriverFlags)`.
- `pub enum PortExitReason { Closed, OwnerExited, Normal }` (`Copy + Eq + Debug`) — the vocabulary of the
  port-exit notification channel (see the defaulted `Driver::port_exit` and `PortRegistry::notify_port_exit`).
- `pub struct DriverContext<'a>` — the driver's view of its port; **the only way a driver touches the poller
  or the queues**: `port() -> PortId`, `register(fd, interest) -> Result<Token>`,
  `reregister(fd, token, interest) -> Result<()>`, `deregister(fd, token) -> Result<()>`,
  `input() -> &mut ByteQueue`, `output() -> &mut ByteQueue`. Registration keys are the port number.
  (`pub(crate) struct PortArms` inside the context records which directions the driver armed; not public.)
- `pub trait Driver` — `name() -> &str` (+ `flags()` default `empty()`); required `start(ctx)`,
  `output(ctx) -> Result<usize>`, `ready_input(ctx)`, `stop(ctx)`; defaulted `ready_output(ctx)` (= `output`),
  `control(ctx, command: &str, arg: &[u8]) -> Result<Vec<u8>>` (= `Ok(vec![])`), `timeout(ctx)` (= `Ok(())`),
  `next_timeout() -> Option<Instant>` (= `None`; the driver's **declaration** of the next deadline it wants
  `Driver::timeout` at — mirrored into the port's `TIMEOUT` task after each interaction, deadline-gated so it
  never fires early), `port_exit(ctx, PortExitReason) -> Result<()>` (= `Ok(())`; the once-only, advisory
  port-exit notification, distinct from the teardown `stop`), and
  `take_exit_status() -> Result<Option<i32>>` (= `Ok(None)`; the exit-status seam, overridden only by `SpawnDriver`).
- `pub type DriverFactory = fn() -> Result<Box<dyn Driver>>`.
- `pub struct DriverRegistry` — name→factory (the `driver_tab.c` seam): `new()`, `with_builtins()`
  (**registers `"fd"`, `"efile"` and `"tcp_inet"`**), `register(name, factory) -> bool`, `create(name) -> Result<Box<dyn Driver>>`,
  `contains(name)`, `names() -> Vec<&str>` (sorted).
### `drivers/fd.rs`
- `pub struct FdDriver` — the built-in `"fd"` byte-stream driver, **owns** its fds (closed on `stop`/`Drop`):
  constructors `pipe()`, `connected(read_fd, write_fd)`, `read_only(fd)`, `write_only(fd)` (unchanged);
  mode/framing surface `with_line_mode(max_len)`, `set_line_mode(max_len)` (0 = unlimited), `set_binary_mode()`,
  `is_line_buffered()`, `phase() -> Phase`, `set_phase(Phase)`, `max_line_len()`, `set_max_line_len(usize)`,
  `pending_input()`, `eof()`.
  Implements `Driver`: registers the read fd `READABLE` on `start`; `output`/`ready_output` drain `ctx.output()`
  to the write fd (`libc::write`, `EAGAIN` ⇒ keep queued + arm `WRITABLE`); `ready_input` reads into
  `ctx.input()` (`libc::read`, loops to `EAGAIN`, latches `read_eof` on `read(2)==0`, flushes a pending
  partial line at EOF, then **de-registers the read fd** (`disarm_input`) so an exhausted — level-triggered
  fallback — regular file stops producing events; the fd stays attached/owned, so a post-EOF `ready_input`
  is still `Ok`). `control` verbs: `line [decimal(N) | "line,<N>"]`, `noline`, `binary`, `text`,
  `getopts` (reply `b"line,<N>"` / `b"binary"`); unknown ⇒ `BeamError::BadArg`.
### `drivers/line_buffer.rs`
- `pub const DEFAULT_MAX_LINE_LEN: usize = 1024`. `pub struct LineBuffer`: `new(max_len)`, `max_len()`,
  `set_max_len(max_len)`, `pending()`, `push(&[u8])`, `take_lines() -> Vec<Vec<u8>>`,
  `drain(&mut impl FnMut(&[u8]))`, `flush_all() -> Vec<Vec<u8>>`. Frames on `\n` (a preceding `\r` is kept as
  part of the terminator), splits a line longer than `max_len`, holds a partial line until `flush_all`/EOF.
### `drivers/binary_io.rs`
- `pub enum Phase { Text, Binary }` (`is_text`/`is_binary`/`as_str`) and `pub struct PhaseChange` — the
  `erl_child_setup` text⇄binary phase machine, default `Phase::Binary` (byte-identical to raw passthrough):
  `new()`, `with_max_line_len(max_len)`, `current()`, `set(Phase)`, `to_binary()`, `to_text()`, `max_line_len()`,
  `set_max_line_len(usize)`, `pending()`, `line_buffer()`, `handle(&[u8]) -> Vec<Vec<u8>>` (Binary ⇒ one raw
  frame; Text ⇒ line frames), `flush() -> Vec<Vec<u8>>` (trailing partial line, Text only).
### `drivers/efile.rs`
- `pub struct EfileDriver` (`name() == "efile"`) — delegates byte movement to a composed `FdDriver`:
  `unopened()`, `read_only(fd)`, `write_only(fd)`, `connected(read, write)`, `path() -> Option<&[u8]>`, `eof()`;
  `control` verbs `open <path>` / `close` / `path` / `eof` plus the delegated `line`/`noline`/`binary`/`text`/`getopts`.
  `open` works on **regular files** (served by the poller's `poll(2)` fallback) as well as on the descriptors
  epoll accepts (pipes/FIFOs/char devices/sockets); `write_only`/`connected` write regular files too.
  `pub fn make_efile() -> Result<Box<dyn Driver>>` is its `DriverFactory` (registered as the built-in `"efile"`).
### `drivers/spawn.rs` (the OS-child driver + its term-free spec)
- `pub enum SpawnStream { Binary, List }` (`Default = List`) with `atom_name(&self) -> &'static str`
  (`"binary"`/`"list"`) — the `binary`/`list` option, applied by the **runtime** when it builds
  `{Port, {data, Data}}` (the driver hands bytes through unchanged).
- `pub enum SpawnSpec { Spawn { command: String, argv: Option<Vec<String>> }, SpawnExecutable { path: String },
  Fd { fd: RawFd } }` — the term-free `open_port/2` `Name`: `/bin/sh -c Cmd` (or `argv` run directly),
  `Path` run directly (with `SpawnOptions::args`), or an adopted descriptor with no child. `command(&self) -> String`
  is the `port_info(Port, name)` value (`fd:<N>` for the descriptor form), `spawns_a_child(&self) -> bool` says
  whether a process is started.
- `pub struct SpawnOptions { pub stream: SpawnStream, pub cd: Option<String>, pub env: Vec<(String, String)>,
  pub args: Vec<String>, pub exit_status: bool }` (`Default`: `list`, nothing else) — the honored options.
- `pub fn build_command(spec: &SpawnSpec, options: &SpawnOptions) -> Result<Option<Command>>` — the single place
  the argv vector, shell-vs-direct, `{cd, …}`/`{env, …}` and the standard streams (`stdin`/`stdout` piped,
  `stderr` discarded to `/dev/null`) are set up; `None` for `SpawnSpec::Fd` (nothing is started), `BadArg` for an
  empty argv. Starts nothing, so it is unit-testable on its own.
- `pub struct SpawnDriver` (`name() == "spawn"`) — constructed **directly** from a spec/options pair (a
  `DriverFactory` is a no-arg `fn` and cannot carry one, so it is *not* in `DriverRegistry`):
  `new(&SpawnSpec, &SpawnOptions) -> Result<SpawnDriver>` **really spawns** (or adopts the fd) so a failure
  registers nothing and the runtime can read `os_pid()` before `PortRegistry::open`; `os_pid() -> Option<u32>`,
  `command() -> &str`, `stream() -> SpawnStream`, `eof() -> bool`, `exited() -> Result<Option<i32>>` (a plain
  liveness probe), `take_exit_status() -> Result<Option<i32>>` (the `exit_status` option, reported once).
  `start` only registers the already-created descriptors; the child's stdout is read into the port input queue
  (drained to `EAGAIN`, EOF latched + the descriptor deregistered/closed), its stdin drains the port output queue
  (set `O_NONBLOCK`, `EAGAIN` leaves the remainder queued and arms write readiness, a write failure is a typed
  `BeamError::Io`); `stop` and `Drop` kill + reap the child (an already-exited child is reaped via `try_wait`,
  no kill) and deregister/close every descriptor. Descriptors of a spawned child are owned and closed; an
  adopted `{fd, N}` descriptor is **duplex** — the single descriptor is registered **once** with `Interest::BOTH`
  and serves both the read stream and the stream `port_command` writes to — and is deregistered but **never**
  closed (the Erlang side owns it). The child's **stderr is discarded** (`Stdio::null()`): it is not port data
  and there is no pipe for it to fill. A failed **direct-exec** spawn (a missing program, `EACCES`, `ENOTDIR`, …)
  is a `BeamError::Raise` carrying the errno atom (`enoent`/`eacces`/`enotdir`/…), matching the oracle; an
  unmapped errno falls back to `BeamError::Io` (see `src/error.rs`).

### `drivers/inet.rs` (the TCP `tcp_inet` socket driver floor)
- `pub const DRIVER_NAME: &str = "tcp_inet"` — the name `port_info(Port, name)` reports and `prim_inet` opens;
  `pub fn make_tcp_inet() -> Result<Box<dyn Driver>>` is its `DriverFactory` (registered by `with_builtins`).
- `pub struct TcpInetDriver` (`name() == "tcp_inet"`) — a non-blocking TCP socket behind a port; `from_socket(fd)`
  adopts a descriptor and probes its kernel state (`SO_ACCEPTCONN` ⇒ listening, `getpeername` ⇒ connected, else
  open), like `inet_ctl_fdopen`; `connect_error() -> Option<i32>` exposes the late `SO_ERROR` outcome of a
  non-blocking `connect`. `pub enum InetState` is the socket's state (unopened/open/listening/connected).
- Control verbs (the C `INET_REQ_*`/`TCP_REQ_*` codes) and their reply bytes: `open` ⇒ `[1]`; `bind` ⇒
  `[1] ++ port_be16`; `listen` ⇒ `[1]`; `accept` ⇒ `[1] ++ fd_be32` (bare `[1]` when nothing is pending,
  `[0]einval` if not listening); `connect` ⇒ `[1]` (or `[0]exbadport`/`[0]eisconn`/`[0]einval`/`[0]++errno`);
  `send`/`recv`/`shutdown` ⇒ `[1]` (or `[0]enotconn`); `close` ⇒ `[1]`. Operational failures are
  `[0] ++ lowercase-errno-name` (`erl_errno_id`); malformed args and unknown verbs ⇒ `BeamError::BadArg`.
  Packed addresses use `INET_AF_INET`/`ANY`/`LOOPBACK`.
- Flow control mirrors `FdDriver`: `send(2)` with `MSG_NOSIGNAL`, `EAGAIN` leaves the remainder queued and arms
  write readiness, a drained queue drops writability and applies a deferred `shutdown`; reads drain to `EAGAIN`
  and latch EOF; `EINPROGRESS` arms write readiness.
- **Deferred** (documented in the module): the second-port `accept` (this floor transfers the port and answers
  `[1] ++ fd_be32` instead of the C's async id), the async-op queue, `driver_set_timer` timeouts,
  `fdopen`/`sendfile`/`unrecv`/`getopts`/`setopts`/`getstats`/`getfd`/`ignorefd`, `recv`'s timeout/length,
  IPv6/UDP/SCTP/Unix-domain sockets, `{active,…}` delivery modes, and DNS (literal addresses only).

## Constraints
- Errors: **`beam_common::{Result, BeamError}` only**; I/O/syscall failures ⇒ `BeamError::Io`,
  argument faults ⇒ `BeamError::BadArg`, id/registration exhaustion ⇒ `BeamError::SystemLimit`.
  **Never panic** on runtime data, never `unwrap()`, never `todo!`/`unreachable!` in shipped code.
- **Safe Rust by default.** `unsafe` is confined to the raw-fd syscall wrappers (`libc::read`/`write`/
  `pipe`/`fcntl`/`close`) and `mio::unix::SourceFd` construction. Every `unsafe` block carries a preceding
  `// SAFETY:` rationale; `unsafe_op_in_unsafe_fn` stays clean (`--workspace` lint).
- **Port ids are allocated by `beam_process::PortTable`** — do not add a second number allocator.
  `PortId` encoding stays `beam_term`'s internal-port immediate (via `beam_process::PortId`).
- **Single-thread invariant (enforced).** `Port`/`PortRegistry` are **not** thread-safe and this crate has no
  locking: they must be created and driven from **one OS thread**, and concurrent registration/dispatch is
  unsupported. `PortRegistry` carries a `PhantomData<core::cell::Cell<()>>` marker so it is neither `Send`
  nor `Sync`; a `Port` holds a `Box<dyn Driver>` with no `Send`/`Sync` bound, so it is single-threaded too.
  (Not yet enforced on `Poller`/`Waker` — the `Waker` is explicitly meant to cross threads.)
- **The `Driver` trait and `DriverContext` public methods are the frozen driver seam** — do not change their
  signatures. New driver capabilities must be additive with defaults that keep the built-in drivers compiling
  unchanged.
- Files target ~1000 lines, split by responsibility: poller / port / port_task / driver / timer / queue / drivers.
- Deps: `beam_common`, `beam_term`, `beam_process`, `libc`, `mio` only. No new deps.

## Test Strategy
Per the crate-layer gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io`
**GREEN**, and `... build --workspace` **GREEN, warning-free**. Current: **189 tests, 0 failures** (1 ignored
doc-test) — `145` lib (`#[cfg(test)]` in-module: poller, timer, queue, driver, port, port_task,
`drivers/{fd,line_buffer,binary_io,efile,spawn,inet}`) + `tests/poller.rs` `8` + `tests/port.rs` `4` +
`tests/port_task.rs` `4` + `tests/spawn.rs` `18` + `tests/inet.rs` `3` + `tests/driver_seams.rs` `5` +
`2` doctests (the `!Send`/`!Sync` compile-fail pins on `PortRegistry`).
- `tests/poller.rs` (integration, real OS fds): socketpair readability reports the right `key`; poll-with-no-events;
  blocking poll interrupted by `Waker::wake` (same + other thread); multiple shareable wakers; reregister flips
  direction; deregister stops events; double-register errors.
- `tests/port.rs` (integration, end-to-end): `open` a `FdDriver::pipe()` port → `output` → `poll` →
  `handle_events` → `read_input` round-trip; `close` releases the id (reused) and empties the poller; `open_named`
  uses the built-in registry, unknown name ⇒ `BadArg`; unknown keys (incl. `WAKER_KEY`) are skipped.
- `tests/port_task.rs` (integration): scheduled OUTPUT+INPUT end-to-end; a **pure EOF** event still reaches the
  driver; the reduction budget bounds a round; and the **wake-path evidence** — a `PortNotification::Data` from a
  scheduled task, delivered to a process blocked in receive via `beam_process::ProcessTable::deliver_simple`
  (`DeliverOutcome::Woke`) then `Scheduler::resume_woken` enqueues it.
- `tests/spawn.rs` (integration, **real child processes**): `echo` output read back through the port input queue;
  the argv form and `{spawn_executable, Path}` + `{args, …}`; `cat` echoing bytes written through the output queue
  (`port_command` → OUTPUT task); the same bytes through the `PortNotification` seam; EOF deregistering the child's
  stdout; a stalled write (256 KiB into a child that never reads, on a `sleep`) staying queued and arming write
  readiness (observed through `Poller::len`); the reaped `exit_status` taken once through
  `PortRegistry::take_exit_status`; a port without the option never reporting one; an unknown id answering `BadArg`;
  `stop` killing **and** reaping a long-lived child (evidence via `/proc/<pid>` disappearing, guarded on procfs
  being present); the `{fd, N}` form reading an adopted `pipe(2)` end with no child and leaving the descriptor open
  plus the **bidirectional** `{fd, N}` cases (write bytes into an adopted `pipe(2)` write end and read them from the
  peer; one adopted `UnixStream::pair()` end carrying both directions); a child flooding 200 KiB to **stderr** still
  delivering its stdout (stderr is discarded, so it can never block); a failed spawn being a constructor error that
  registers nothing; an `erl` oracle cross-check of the failed-spawn reason atom (`enoent`/`eacces`/`enotdir`); and
  `{cd, …}`/`{env, …}` reaching the child.
- `tests/inet.rs` (integration, real loopback TCP): the `tcp_inet` port resolved through `DriverRegistry` round-trips
  bytes both ways; a stalled write arms write readiness (a forged write-ready event schedules an OUTPUT task and
  `execute_port_tasks` runs it); unknown verb ⇒ `BadArg`; and an `erl` (OTP 29.1.1 / erts-17.1) **oracle cross-check**
  of the `INET_REQ_OPEN`/`BIND`/`LISTEN`/`TCP_REQ_SHUTDOWN`/`RECV`/`CLOSE` reply bytes (skips loudly without `erl`).
- `tests/driver_seams.rs` (integration, real `PortRegistry`): a driver-declared deadline (`Driver::next_timeout`) is
  armed and its `Driver::timeout` fires exactly once after it, never before; a `control` re-arms/clears the port
  timer; `PortRegistry::{schedule_timeout,cancel_timeout}` (incl. re-pointing); `notify_port_exit` reaching the
  driver once with the right `PortExitReason` and a later `close` still running `stop`.
- In-module unit tests cover: `ByteQueue`; `Interest`/`Ready`/`Event`; `Poller` register/deregister + `WAKER_KEY` +
  wakers + the **regular-file fallback** (register/readiness + `poll(None)` promptness, the mio+fallback merge,
  fallback `reregister`, independent fallback entries); `Timeout`/`Timer`; `DriverRegistry` (all three built-ins,
  dup rejection, missing-name `BadArg`); `PortState` transitions; `PortRegistry` alloc/close/reuse + unknown-id
  `BadArg`; `FdDriver` (pipe round-trip, EOF + the EOF de-registration, error paths, `stop` deregistration, fd
  closing on `Drop`+`stop`); `LineBuffer`/`PhaseChange`/`EfileDriver` framing and control + a regular-file
  open→read-to-EOF→close and a regular-file write; port-task ordering/reductions/dedupe + timer re-pointing
  (`arm_timeout`); the two new driver-seam defaults (`Driver::next_timeout` → `None`, `Driver::port_exit` → `Ok`)
  and the `PortExitReason` vocabulary; the errno→atom mapping (`error::errno_atom`/`spawn_failure`); the `tcp_inet`
  verb/reply-shape helpers (packed-address decoding, `errno_name`, `from_socket` state, the full loopback round
  trip); the failed-`start` teardown (`FailingStartDriver` double); `SpawnDriver`/`build_command` (the four name
  forms, `{cd, …}`/`{env, …}` on the built `Command`, empty argv `BadArg`, negative fd `BadArg`, the
  unknown-program constructor error, accessors, the real read path driven through a hand-built `DriverContext` —
  bytes, latched EOF and the EOF deregistration — the once-only `exit_status`, the `128 + signal` convention, and
  idempotent kill/reap).

## Design Decisions
- **Backend: `mio` (lib) vs raw epoll.** `mio` gives a portable, safe `Poll`/`Registry`/`Waker` and a correct
  one-shot re-arm model matching the C's `EPOLLONESHOT`/`EV_ONESHOT`. The public API is **mio-independent**
  (registration by raw `RawFd` through `Interest`/`Token`/`Ready`), so a future io_uring/`/dev/poll` backend swaps
  in without touching callers. `io_uring` is a documented alternative, not M1.
- **Async notification = one `mio::Waker` per `Poller`, shared via `Arc`.** `mio` permits exactly one `Waker`
  per `Poll`, so the `Poller` creates it once in `new()`; `Poller::waker` returns a cheap `Arc` clone
  (`pub struct Waker(Arc<mio::Waker>)`), making `waker()` idempotent and safe to drop.
- **Who owns the output queue.** The `Port` (not the driver) owns both `ByteQueue`s, mirroring the C `ioq`.
  `Port::output` appends and calls `Driver::output`, which **pulls** from `ctx.output()` and returns bytes
  written; `EAGAIN` leaves the remainder queued and the driver arms `WRITABLE` (no double buffering).
- **Port-task execution lives in `beam_io`; the scheduler thread lives above it.** `PortTask`/`PortTaskKind`/
  `PortTaskQueue`/reductions model `erl_port_task.h`; `PortRegistry::execute_port_tasks` runs a round. The poll
  loop that drives a round every iteration, port run queues and reductions charged to a *scheduler* belong to
  `beam_process`/`beam_rt`. `PortNotification` is the seam so the runtime (not this crate) turns port output into
  messages and wakes the owner via `beam_process` — this keeps the DAG clean and the engine/runtime split intact.
- **Readiness ⇒ task, gated by `PortArms`.** The poller keys every registration by port number, so an `Event`
  names the port but not which of its fds fired; `DriverContext` records the armed directions in `PortArms` so a
  full close (`EPOLLHUP` ⇒ `read_closed` **and** `write_closed`) does not schedule a bogus OUTPUT task on a
  read-only driver. This restores the information the C gets from per-event task handles (`ErlDrvEvent`).
  **Documented divergence:** the C keeps three FIFO queues with no cross-kind priority; this crate uses one queue
  ordered by kind priority (INPUT → OUTPUT → TIMEOUT) with enqueue sequence as FIFO tie-break — observable only in
  the order two *different* kinds run within one round.
- **`handle_events` schedules *and* runs the default round.** This preserves the historic "driving the reactor
  makes ports progress" behavior; callers wanting full budget control use `schedule_events` +
  `execute_port_tasks`. (The strict-enqueue-only variant would break callers that rely on the single call.)
  Unlike `execute_port_tasks`, it does **not** harvest `PortNotification`s: input bytes stay in the port's queue
  (reachable via `Port::drain_input`), and closes are not reported. Only `execute_port_tasks` yields the
  `Data`/`Closed` notifications a runtime turns into `{Port, {data, Data}}`/port-exit messages.
- **Driver = Rust trait now; C-ABI bridge layered later.** The frozen `ErlDrvEntry`/`ErlDrvApi`
  (`beam_abi::driver`) is an ABI, not a Rust interface; a later `beam_abi`-backed adapter implements `Driver`
  over an `ErlDrvEntry` + `ErlDrvApi` function table (`start→init/start`, `output→output/outputv`,
  `ready_input→ready_input`, `control→control`, `timeout→timeout`, `stop→stop`, `driver_select` ⇒
  `DriverContext::register`). Modeling the driver as a trait first keeps the poller/port core testable without
  the frozen-ABI machinery. (Record in `../../CHANGE.md` when the bridge lands.)
- **Port ids reuse `beam_process::PortTable`.** `PortRegistry` wraps it for allocation and adds the
  `HashMap<u64, Port>` keyed by port *number*; a failed driver `start` runs the driver's teardown (so any fd it
  registered is deregistered) and releases the number — the id space and the poller never leak.
- **`Poll` returns owned `Vec<Event>`.** The reactor glue needs `&mut Poller` while iterating events; an owned
  `Vec` keeps the borrow honest, reusing the internal `mio::Events` buffer.
- **Line/phase framing is driver-side.** `{line, N}` and the text⇄binary phase change live in the `FdDriver`
  (via `LineBuffer`/`PhaseChange`), matching `erl_child_setup`: the line flag affects how a port *delivers input
  to its owner*, not how it accepts output. Default phase = Binary, so nothing changes unless toggled.
- **Exit status travels through a defaulted trait method, not a notification variant.** The runtime must be able
  to obtain a child's reaped status **once** for a port opened with `exit_status`, without reaching into driver
  internals. The chosen seam is a defaulted `Driver::take_exit_status() -> Result<Option<i32>>` (= `Ok(None)`, so
  `FdDriver`/`EfileDriver` compile unchanged) plus the additive
  `PortRegistry::take_exit_status(id) -> Result<Option<i32>>` accessor — `Ok(None)` means "no child", "not reaped
  yet" or "already reported", and an unknown id is `BadArg`. It is `Result` rather than a bare `Option` so a
  failing `wait` is reported instead of swallowed. A `PortNotification` variant was rejected: exit status is not a
  *task-round* outcome (it is neither readiness- nor reduction-driven, exactly why the reference runtime polls it
  between stream rounds), and a variant would force every round to carry a status the port may not owe.
- **The spawn driver is constructed, not registered — and reaps its child eagerly.** A `DriverFactory` is a
  no-arg `fn`, so it cannot carry the command/argv/options a spawn port needs; the driver is therefore built
  directly from a `SpawnSpec` (its `Driver::name()` stays `"spawn"` for `port_info` reporting). The child is
  started in the **constructor** so a failed spawn is a constructor error that leaves nothing registered and the
  runtime can read `os_pid()` before `PortRegistry::open`. `{fd, N}` adopts one descriptor as a **duplex** stream
  (one `Interest::BOTH` registration; `arm_output`/`disarm_output`/`release_stdout` are no-ops for it) and never
  closes it (the Erlang side owns it, matching the reference runtime's `LivePort::close`). `stop`/`Drop` kill
  **and** reap, and `Drop` reaps too because Rust's `Child` does not: dropping it otherwise leaks a zombie.
- **Regular files ride a `poll(2)` fallback inside the poller.** `epoll` cannot watch a regular file, so `Poller`
  keeps a second, `libc::poll(2)`-based registration set for descriptors `mio` rejects and merges its readiness
  into `Poller::poll` — reproducing the reference runtime's `poll(2)` readiness path ("a regular file is trivially
  always ready", `erts/emulator/sys/common/erl_poll.c`) without changing the public `Poller`/`Port` API or adding a
  dependency, and without giving up the edge-triggered `mio` model for the descriptors it serves. Because the
  fallback is level-triggered, `FdDriver` de-registers a read fd at EOF.
- **A failed spawn raises the errno atom.** The reference raises `erl_errno_id(errno)` from
  `erts_internal_open_port_2` (`erts/emulator/beam/erl_bif_port.c`), so a direct-exec spawn failure is returned as a
  `BeamError::Raise` carrying the errno's lowercase name (`enoent`/`eacces`/`enotdir`/…) rather than `BeamError::Io`,
  which stays the fallback for an unmapped errno.
- **The child's stderr is discarded, not piped.** A piped-but-undrained stderr deadlocks a child that writes more
  than the pipe capacity; a child's stderr is not port data (the oracle does not deliver it either), so
  `build_command` sends it to `/dev/null` (`Stdio::null()`). `SpawnSpec`/`SpawnOptions` are unchanged (the runtime
  cannot pass a `stderr_to_stdout` option through them today).
- **The timer seam is driver-declared and mirrored; the port-exit channel is advisory and once-only.** A driver
  declares the next deadline it wants via `Driver::next_timeout`, which `Port` mirrors into its single `TIMEOUT` task
  after each interaction (deadline-gated, so it never fires early); `PortRegistry::{schedule_timeout,cancel_timeout}`
  are the runtime's explicit override. `Driver::port_exit` (via `PortRegistry::notify_port_exit`) is a once-only,
  advisory notification distinct from the teardown `stop`, so a driver can react to its port exiting without owning
  the lifecycle. Both are additive trait methods with defaults, so every existing driver compiles unchanged.
- **The TCP driver is a floor, not a full `inet`.** `tcp_inet` covers the core
  `open`/`bind`/`listen`/`accept`/`connect`/`send`/`recv`/`shutdown`/`close` verbs with the C's reply encodings; the
  async-op queue, per-op timeouts, the second-port `accept`, IPv6/UDP/SCTP/Unix-domain, `{active,…}` modes and DNS
  are deferred (see the module docs).

## Known Issues
- **Single-OS-thread only.** No dedicated poll threads, no `ThreadProgress`/generation handoff, no
  `ERTS_POLL_USE_WAKEUP` beyond `Waker`; the single-thread invariant is enforced on `Port`/`PortRegistry` (see
  Constraints) but **not** guarded on `Poller` itself — use it from one thread.
- **Readiness is edge-triggered** (`mio` registers with `EPOLLET`), so a driver *must* drain to `EAGAIN` on every
  callback — `FdDriver` reads/writes in an `EAGAIN`-terminated loop for exactly this reason. Readiness is only
  scheduled for directions the driver armed (`PortArms`), so a driver that never registers a direction never sees
  its events.
- **This crate has no poll thread of its own.** `PortRegistry::execute_port_tasks` runs a round on demand; the
  per-iteration drive (poll → `schedule_events`/`handle_events` → `execute_port_tasks`) and the reduction budget
  live in the single-threaded `beam_rt` main loop (which owns the one long-lived `Poller`). There is no port run
  queue or dedicated poll thread here.
- **`FdDriver` is deliberately minimal.** Byte-stream only; line mode frames input but does **not** line-frame
  output (matching the emulator's line flag). No `inet`/`forker` drivers; no packet framing.
- **`SpawnDriver` is the OS-child floor, not the whole `spawn` driver.** Decoding the `open_port/2` terms into a
  [`SpawnSpec`](src/drivers/spawn.rs)/`SpawnOptions` (and synthesizing `{Port, {data, …}}`,
  `{Port, {exit_status, …}}` and a `{Port, eof}` message from them) stays the runtime's job — this crate only
  moves the bytes and kills/reaps the child. The child's stderr is discarded (`Stdio::null()`); the driver signals
  EOF only by latching its own state (`eof()`), it does not report an event. Writing to a child that closed its
  stdin fails with a typed `BeamError::Io` (the queued remainder is kept, never silently discarded), and `{fd, N}`
  is a **duplex** adoption — one descriptor behind one `Interest::BOTH` registration, read and written by the port
  (end-to-end `port_command` to a `{fd, N}` port is still intercepted as `unsupported` in `beam_rt`, a handoff).
  Adopting a descriptor also requires it to be switchable to
  non-blocking (`F_SETFL` + `O_NONBLOCK`); a descriptor that refuses that (some inherited descriptors return
  `EPERM`) fails the open with `BeamError::Io`, because the reactor is edge-triggered and cannot serve a blocking
  descriptor. Input is not line-framed (there is no `line` control verb):
  the `binary`/`list` choice rides on `SpawnStream` for the runtime to apply.
- **`port_control`/control atoms are not decoded here.** `port_control(op, &[u8])` takes a `&str` op + raw bytes;
  the C decodes Erlang tuples in `io.c` — atom/tuple decoding of `{command, …}`/`{connect, …}` is the runtime's job.
- **`mio` fd registration is `os-ext`/Unix-only** (`mio::unix::SourceFd`); Windows/iocp is out of scope (as in
  `beam_abi`).
- **`src/port.rs` is ~1430 lines, ~590 of which are its in-module test suite** (source ≈ 845 lines, under the
  ~1000 baseline). A future split by responsibility (move `PortRegistry` + reactor glue to `registry.rs`, keeping
  `Port` in `port.rs`) is reasonable; the file is long only because the tests are comprehensive in-place.
- **`src/drivers/spawn.rs` is 996 lines, ~275 of which are its in-module test suite** (source ≈ 720 lines, still
  under the ~1000 baseline). It holds the whole OS-child driver plus the term-free spec/`build_command`; if it
  grows, split the spec (`SpawnSpec`/`SpawnOptions`/`SpawnStream`/`build_command`) into a sibling module and keep
  the driver in `spawn.rs`.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/poller.rs` → `Poller`, `Token`, `Interest`, `Ready`, `Event`, `Waker` (the `mio`-backed reactor)
- `src/timer.rs` → `Timeout`, `Timer` (poll-deadline primitives)
- `src/queue.rs` → `ByteQueue` (the port input/output byte buffers)
- `src/port_task.rs` → `PortTaskKind`/`PortTask`/`PortTaskQueue`, reduction constants, `Readiness`, `PortNotification`, `PortTaskOutcome` (the `erts_port_task_execute` model)
- `src/port.rs` → `Port`, `PortState`, `PortControl`, `PortCommandOptions`, `PortRegistry` (port id + queues + task scheduling + driver dispatch)
- `src/driver.rs` → `Driver` trait (incl. the defaulted `take_exit_status` exit-status seam), `DriverContext`, `DriverFlags`, `DriverRegistry`, `pub(crate) PortArms` (the driver seam)
- `src/drivers/fd.rs` → `FdDriver` (the built-in fd/pipe byte-stream driver, incl. line/phase modes + `control`)
- `src/drivers/line_buffer.rs` → `LineBuffer`, `DEFAULT_MAX_LINE_LEN` (`{line, N}` framing)
- `src/drivers/binary_io.rs` → `Phase`, `PhaseChange` (the text⇄binary phase-change protocol)
- `src/drivers/efile.rs` → `EfileDriver`, `make_efile` (the built-in `efile` driver)
- `src/drivers/spawn.rs` → `SpawnDriver` (the OS-child `spawn` driver), `SpawnSpec`/`SpawnOptions`/`SpawnStream` + `build_command` (the term-free `open_port/2` name/options model the runtime decodes into)
- `src/drivers/mod.rs` → driver-module wiring + re-exports
- `src/error.rs` → `pub(crate)` error constructors (no new error type)
- `tests/poller.rs` → registration + readiness on a real OS pipe/socketpair + timeout/waker
- `tests/port.rs` → `open`→`output`→`poll`→`handle_events`→`read_input` byte round-trip
- `tests/port_task.rs` → port-task scheduling end-to-end + the receive-wake path (`PortNotification` → `beam_process` scheduler)
- `tests/spawn.rs` → real child processes end-to-end (stdout→input queue, stdin→output queue, notifications, EOF, a stalled write arming write readiness, once-only exit status, kill+reap, the `{fd, N}` adoption, a failed spawn)
- `../beam_process/src/ptab.rs`, `scheduler.rs`, `process.rs` → `PortTable`/`PortId`, `Scheduler::resume_woken`, `ProcessTable::deliver`/`drain_woken`, `DeliverOutcome` (sibling — read-only)
- `../beam_rt/src/port.rs` → the reference `open_port/2` term decoding (`RuntimePorts::{name_spec, command_spec, port_options, env_list, arg_list, spawn}`) that `SpawnSpec`/`SpawnOptions`/`build_command` mirror term-free (sibling — read-only)
- `../beam_abi/CONTEXT.md` → the frozen `ErlDrvEntry`/`ErlDrvApi` driver C ABI to bridge later (sibling — read-only)
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
- `../beam_abi/CONTEXT.md` → the `ErlDrvEntry`/`ErlDrvApi` layouts the C-ABI bridge will target.
- `../beam_process/CONTEXT.md` → `PortTable`/`PortId`, `Scheduler`/`ProcessTable` and the scheduler that will drive ports.
- Owed upstream (repo-root owned, outside this node): a `../../CHANGE.md` entry for the port-task model +
  `PortNotification` seam + `port_command`/`port_control` surface, the line/phase/`efile` driver additions, and the
  `DriverRegistry` `"efile"` registration.
