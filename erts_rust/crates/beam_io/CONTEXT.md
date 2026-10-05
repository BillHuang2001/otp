# `beam_io` — Poller/Reactor, Ports, Built-in Drivers

## Intent
Owns the **I/O subsystem** of the Rust ERTS rewrite (Stage 5, Part II §H) and the **OS/driver seam** —
one of the two clean, already-existing rewrite seams (Part II §A): it is the layer that must be
reproduced with the *frozen* driver C ABI on top of it (workspace `../CONTEXT.md` §1 **A.4**), so a
driver `.so` compiled against the unmodified `erl_driver.h` links and behaves identically.

Three things live here:
1. the **event poller/reactor** — a `mio`-backed readiness loop (epoll/kqueue/…) with a
   `Token`→registration map (stage 5's "I/O" row);
2. the **`Port`** — a process-like I/O endpoint: a port id (bit-exact with `beam_term`'s internal-port
   immediate), an **input** byte queue and an **output** byte queue, a lifecycle, and dispatch to a
   driver;
3. the **driver interface + built-in drivers** — a Rust `Driver` trait now, with the frozen C-ABI
   `ErlDrvEntry` bridge as documented future work, plus one built-in byte-stream driver.

Consumed by `beam_rt` (boot/`system_info`/driver registration) and, through port ids, observed by
`beam_process` (ports are the peer of pids in `ptab`). This crate is the **library**; the *scheduler*
wiring (port tasks, reductions, the reactor thread) belongs to `beam_process`/`beam_rt`.

Legacy C ground truth (read-only): `../../erts/emulator/sys/common/erl_poll.c` + `erl_check_io.c` (the
poll loop, one-shot re-armed read/write readiness, self-pipe wakeup), `../../erts/emulator/beam/io.c`
(`erts_port_command`/`erts_port_control`, the `ioq` output queue), `erl_port.h`
(`struct _erl_drv_port`, `ERTS_PORT_SFLG_*`), `erl_io_queue.{c,h}` (`SysIOVec` queue),
`erl_port_task.{c,h}` (the asynchronous work unit), `../../erts/emulator/drivers/common/inet_drv.c`
(the representative fd+poller driver: lazy `driver_select`, EAGAIN leaves data queued).

## Milestone-1 scope (what "implemented" means for THIS pass)
A **real, tested, single-OS-thread library core** — documented subsets, not a full port:
1. `Poller`: register/reregister/deregister of **raw fds** (`mio::unix::SourceFd`), `Token`→key map,
   `poll(timeout)` returning owned `Event`s, and an async **`Waker`** (mio's eventfd/pipe).
2. `Port` + `PortRegistry`: id allocation **reusing `beam_process::PortTable`** (never duplicated),
   input/output `ByteQueue`s, lifecycle (`open`/`output`/`ready_input`/`ready_output`/`control`/`close`),
   and event→port dispatch (`handle_events`).
3. One built-in driver, `FdDriver` (an fd/pipe byte-stream driver) implemented end-to-end over the
   poller: `pipe()` self-echo round-trip is the proof of the poller→port→driver path.
4. `Timeout` + `Timer` (multi-deadline) primitives for poll timeouts, and `ByteQueue`.
Out of scope (documented follow-ups): real OS-threaded poll threads, port tasks/reductions, the
`ERL_DRV_FLAG_USE_PORT_LOCKING` locking, distribution fd handling, `/dev/poll`-style backends,
`io_uring`, and the frozen C-ABI bridge (see Design Decisions).

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
- `pub struct Event` — `token() -> Token`, `key() -> u64` (the caller key it was registered with),
  `ready() -> Ready`.
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
  `pub fn wake(&self) -> Result<()>` (thread-safe async notification, safe to call repeatedly and
  from any thread; the woken event carries `key() == WAKER_KEY`).
- `pub const WAKER_KEY: u64` — the key of a waker event.
### `timer.rs`
- `pub struct Timeout` (`Copy`) — a single deadline: `infinite()` (no deadline), `after(Duration)`,
  `at(Instant)`, `poll_timeout() -> Option<Duration>` (remaining; `Some(0)` once elapsed; `None` if
  infinite), `is_expired() -> bool`.
- `pub struct Timer` — a set of scheduled deadlines keyed by `u64`: `new()`, `schedule(key, Instant)`,
  `next_deadline() -> Option<Instant>`, `poll_timeout() -> Option<Duration>`, `pop_expired(now) ->
  Vec<u64>`, `cancel(key) -> bool`, `len()`, `is_empty()`.
### `queue.rs`
- `pub struct ByteQueue` — a byte buffer (read-cursor `Vec<u8>`, `front()` is always contiguous):
  `new()`, `with_capacity(n)`, `len()`, `is_empty()`, `push(&[u8])`, `front() -> &[u8]`,
  `consume(n) -> usize`, `read_into(&mut [u8]) -> usize`, `clear()`.
### `driver.rs`
- `pub struct DriverFlags(u32)` — `Copy + Eq`; `empty()`; const `SOFT_BUSY`; `contains(DriverFlags)`.
- `pub struct DriverContext<'a>` — the driver's view of its port; **the only way a driver touches the
  poller or the queues**: `port() -> PortId`, `register(fd, interest) -> Result<Token>`,
  `reregister(fd, token, interest) -> Result<()>`, `deregister(fd, token) -> Result<()>`,
  `input() -> &mut ByteQueue` (append bytes the driver read), `output() -> &mut ByteQueue` (pull bytes
  waiting to be written). Registration keys are assigned by the context (the port number).
- `pub trait Driver` — `name() -> &str` (+ `flags()` default `empty()`); required `start(ctx)`,
  `output(ctx) -> Result<usize>` (drain `ctx.output()`, return bytes written), `ready_input(ctx)`,
  `stop(ctx)`; defaulted `ready_output(ctx)` (calls `output`), `control(ctx, command, arg) ->
  Result<Vec<u8>>` (`Ok(vec![])`), `timeout(ctx)` (`Ok(())`).
- `pub type DriverFactory = fn() -> Result<Box<dyn Driver>>`.
- `pub struct DriverRegistry` — name→factory (the `driver_tab.c` seam): `new()`,
  `with_builtins()` (registers `"fd"`), `register(name, factory) -> bool`, `create(name) ->
  Result<Box<dyn Driver>>`, `contains(name)`, `names() -> Vec<&str>`.
### `port.rs`
- `pub enum PortState { Opening, Open, Closing, Closed }` (`Copy + Eq + Debug`).
- `pub struct Port` — owns a `Box<dyn Driver>` + the two queues:
  `id() -> PortId`, `state() -> PortState`, `driver_name() -> &str`, `input_len()`, `read_input(&mut
  [u8]) -> usize`, `drain_input() -> Vec<u8>`, `output_len()`, `open(&mut Poller)`, `output(&mut
  Poller, &[u8]) -> Result<usize>`, `ready_input(&mut Poller)`, `ready_output(&mut Poller) ->
  Result<usize>`, `control(&mut Poller, command, arg) -> Result<Vec<u8>>`, `close(&mut Poller)`.
- `pub struct PortRegistry` — id allocation (inner `beam_process::PortTable`) + `HashMap<u64, Port>`
  keyed by port number: `new()`, `open(poller, Box<dyn Driver>) -> Result<PortId>`,
  `open_named(poller, &DriverRegistry, name) -> Result<PortId>`, `get(id)`, `get_mut(id)`,
  `output(poller, id, &[u8]) -> Result<usize>`, `handle_events(poller, &[Event]) -> Result<usize>`
  (the reactor glue: dispatch each event's `ready` to `ready_input`/`ready_output`), `close(poller,
  id) -> Result<()>`, `contains(id)`, `len()`, `is_empty()`.
### `drivers/fd.rs`
- `pub struct FdDriver` — the built-in byte-stream driver, **owns** its fds (closed on `stop`/`Drop`):
  `pipe() -> Result<FdDriver>` (a fresh OS `pipe(2)`: writes to `write_fd` are read from `read_fd`),
  `connected(read_fd, write_fd)`, `read_only(fd)`, `write_only(fd)`. Implements `Driver`: registers
  the read fd `READABLE` on `start`; `output` drains `ctx.output()` to the write fd (`libc::write`),
  keeping the remainder queued and selecting `WRITABLE` on `EAGAIN`; `ready_input` reads into
  `ctx.input()` (`libc::read`, `EAGAIN`-tolerant, EOF ⇒ no more reads).

## Constraints
- Errors: **`beam_common::{Result, BeamError}` only**; I/O/syscall failures ⇒ `BeamError::Io`,
  argument faults ⇒ `BeamError::BadArg`, id/registration exhaustion ⇒ `BeamError::SystemLimit`.
  **Never panic** on runtime data, never `unwrap()`, never `todo!`/`unreachable!` in shipped code.
- **Safe Rust by default.** `unsafe` is confined to the raw-fd syscall wrappers (`libc::read`/`write`/
  `pipe`/`fcntl`/`close`) and `mio::unix::SourceFd` construction. Every `unsafe` block carries a
  preceding `// SAFETY:` rationale; `unsafe_op_in_unsafe_fn` stays clean (`--workspace` lint).
- **Port ids are allocated by `beam_process::PortTable`** — do not add a second number allocator.
  `PortId` encoding stays `beam_term`'s internal-port immediate (via `beam_process::PortId`).
- Files ~1000 lines, split by responsibility: poller / port / driver / timer / queue / drivers.
- Deps: `beam_common`, `beam_term`, `beam_process`, `libc`, `mio` only (already declared). No new deps.

## Test Strategy
Per the crate-layer gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io`
**GREEN**, and `... build --workspace` **GREEN**. All of the following exist and pass (43 in-module
unit tests + 12 integration tests):
- `tests/poller.rs` (integration, real OS fds): a **socketpair** registered `READABLE` reports a
  readiness event with the right `key` after a write to the peer; **poll-with-no-events** returns empty
  after ≈ the timeout; `poll(Some(2s))` is interrupted by `Waker::wake` from another thread; **two
  wakers obtained from the same `Poller`** both fire (and one may be dropped mid-test), which is safe
  because the poller owns a single shared wakeup handle; a waker clone moved to another
  thread wakes a later blocking poll; reregister switches readability → writability; deregister stops
  events; a double-register of the same fd errors (`AlreadyExists` → `Io`).
- `tests/port.rs` (integration, end-to-end): `PortRegistry::open` a `FdDriver::pipe()` port →
  `port.output(b"hello")` → `poller.poll` → `registry.handle_events` → `port.read_input` returns
  `"hello"`; `close` releases the id (a later alloc reuses it) and leaves the poller empty; `open_named`
  uses the built-in registry and an unknown name is `BadArg`; `handle_events` skips unknown keys.
- In-module `#[cfg(test)]` unit tests: `ByteQueue` (push/front/consume/read_into/clear + wrap-around
  compaction), `Interest`/`to_mio_interest`, `Ready`/`Event` accessors, `Poller` register/deregister +
  `WAKER_KEY` + repeated shareable `waker()` handles, `Timeout` (infinite/remaining/expired/saturating),
  `Timer` (ordering, `pop_expired`, `cancel`, duplicate-key reschedule), `DriverRegistry`
  (`with_builtins`/`create`/missing name ⇒ `BadArg`/`names`), `PortState` transitions, `PortRegistry`
  alloc/close/reuse, and `FdDriver` (pipe round-trip, EOF, write-only/read-only/inert error paths,
  `stop` deregistration, fd closing on `Drop` + `stop`).

## Design Decisions
- **Backend: `mio` (lib) vs raw epoll.** `mio` gives a portable, safe `Poll`/`Registry`/`Waker`
  (epoll on Linux, kqueue elsewhere) and a correct one-shot re-arm model, matching the C's one-shot
  `EPOLLONESHOT`/`EV_ONESHOT` semantics. The public API is **mio-independent**: registration is by
  **raw `RawFd`** through `Interest`/`Token`/`Ready` newtypes, so a future io_uring or `/dev/poll`
  backend can be swapped without touching callers. `io_uring` (Part II §A) is a documented
  alternative backend, not M1.
- **Async notification = one `mio::Waker` per `Poller`, shared via `Arc`.** The C uses a non-blocking
  `pipe()` (`ps->wake_fds[2]`) because its pollsets are its own; `mio` already provides a portable,
  internally-correct wakeup (eventfd/pipe). Reuse it (Prevent-Duplication); `Waker` token `0`/key
  `WAKER_KEY` identify it. `mio` permits **exactly one** `Waker` per `Poll` (a debug-build assertion),
  so the `Poller` creates the wakeup handle once in `new()` and owns it; `Poller::waker` returns a
  cheap `Arc` clone (a `pub struct Waker(Arc<mio::Waker>)`), making `waker()` idempotent, callable from
  any number of places, and safe to drop — the handle lives as long as the `Poller`.
- **Who owns the output queue.** The `Port` (not the driver) owns both `ByteQueue`s, mirroring the C
  `ioq` (Erlang→driver) being the single source of truth. `Port::output` appends to the output queue
  and calls `Driver::output`, which **pulls** from `ctx.output()` and returns bytes written; `EAGAIN`
  leaves the remainder queued and the driver arms `WRITABLE`. This avoids double-buffering the same
  bytes in both the port and the driver.
- **Driver = Rust trait now; C-ABI bridge layered later.** The frozen `ErlDrvEntry`/`ErlDrvApi`
  (`beam_abi::driver`) is an ABI, not a Rust interface; a NIF/driver `.so` cannot implement a Rust
  trait. The plan: a later `beam_abi`-backed adapter implements `Driver` over an `ErlDrvEntry` +
  `ErlDrvApi` function table, mapping `start→init/start`, `output→output/outputv`, `ready_input→
  ready_input`, `ready_output→ready_output`, `control→control`, `timeout→timeout`, `stop→stop`, and
  delivering `driver_select` registrations through `DriverContext::register`. Modeling the driver as a
  trait first keeps the poller/port core testable without the frozen-ABI machinery and lets the bridge
  be purely additive. (Record in `../../CHANGE.md` when the bridge lands.)
- **Port ids reuse `beam_process::PortTable`.** `PortRegistry` wraps it for allocation and adds the
  `HashMap<u64, Port>` of live port objects keyed by port *number*; it never allocates its own number.
  This keeps `ptab` the single id authority (Prevent-Duplication) and lets `beam_process` keep seeing
  the same id space.
- **Driver→Erlang output is a byte input queue here, not (yet) messages.** The C emits driver output
  as *messages* to the port owner (no inbound queue). For M1 the port keeps an **input byte queue** so
  the poller→port→driver path is provable and testable without the message layer; the runtime (M2+)
  replaces `drain_input` with `beam_message` sends to the owner.
- **`Poll` returns owned `Vec<Event>`.** The reactor glue (`handle_events`) needs `&mut Poller` to
  re-register while iterating events; returning a borrowed slice would borrow `Poller` for the loop.
  An owned `Vec` keeps the borrow honest; the internal `mio::Events` buffer is still reused.

## Known Issues
- **Single-OS-thread only.** No dedicated poll threads, no `ThreadProgress`/generation handoff, no
  `ERTS_POLL_USE_WAKEUP` semantics beyond `Waker`; concurrent registration races are not guarded.
- **Readiness is edge-triggered** (`mio` registers with `EPOLLET`), so a driver *must* drain to
  `EAGAIN` on every callback — `FdDriver` reads/writes in a `EAGAIN`-terminated loop for exactly
  this reason. `handle_events` dispatches on `Ready::readable()`/`Ready::writable()` only, so a
  pure EOF/`EPOLLHUP` event with neither bit set reaches the port only through the readable edge.
- **A failed `Port::start` leaks the poller registration.** `PortRegistry::open` releases the port
  id and drops the driver (closing its fds) when `start` fails, but a registration the driver made
  before failing is only reclaimed when the OS closes the fd.
- **Ports are not yet scheduled.** No port tasks (`INPUT`/`OUTPUT`/`TIMEOUT`), reductions, or
  `erts_port_task_execute`; `PortRegistry::handle_events` is a synchronous stand-in. `beam_rt` wires
  ports into the scheduler later.
- **`FdDriver` is deliberately minimal.** Byte-stream only; no line buffering, no `binary_io`
  phase-change, no `control` protocol family (`inet`/`efile` are future drivers). `read_only`/
  `write_only` variants exist and are covered, but the primary tested path is `pipe()`.
- **`mio` fd registration is `os-ext`/Unix-only** (`mio::unix::SourceFd`); Windows/iocp is out of
  scope (as in `beam_abi`). Registration is lazy-capable but M1 registers in `start`.
- **`port_control`/command atoms** are not decoded here (the C decodes Erlang tuples in `io.c`); M1
  `control(command, arg)` takes a `&str` command and raw bytes.

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/poller.rs` → `Poller`, `Token`, `Interest`, `Ready`, `Event`, `Waker` (the `mio`-backed reactor)
- `src/timer.rs` → `Timeout`, `Timer` (poll-deadline primitives)
- `src/queue.rs` → `ByteQueue` (the port input/output byte buffers)
- `src/driver.rs` → `Driver` trait, `DriverContext`, `DriverFlags`, `DriverRegistry` (the driver seam)
- `src/port.rs` → `Port`, `PortState`, `PortRegistry` (port id + queues + driver dispatch)
- `src/drivers/fd.rs` → `FdDriver` (the built-in fd/pipe byte-stream driver)
- `src/error.rs` → `pub(crate)` error constructors (no new error type)
- `tests/poller.rs` → registration + readiness on a real OS pipe/socketpair + timeout/waker
- `tests/port.rs` → `open`→`output`→`poll`→`handle_events`→`read_input` byte round-trip
- `../beam_process/src/ptab.rs` → `PortTable` port-id allocation (sibling — read-only, escalate writes to `..`)
- `../beam_abi/CONTEXT.md` → the frozen `ErlDrvEntry`/`ErlDrvApi` driver C ABI to bridge later (sibling — read-only)
- `../../erts/emulator/sys/common/erl_poll.c`, `../../erts/emulator/beam/io.c`, `../../erts/emulator/beam/erl_port.h`, `../../erts/emulator/drivers/common/inet_drv.c` → legacy C ground truth (read-only reference)

## Notes for Agents
- Read `../CONTEXT.md` (crate-layer contracts, the poller/`Port`/driver-registry seam), `../beam_process/CONTEXT.md`
  (`PortTable`/`PortId`) and `../../CONTEXT.md` §1 A.4/§3 before changing this crate.
- Build/test: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` (sandbox
  `$HOME` is read-only — always the wrapper). Never `cd`; never touch `../../tools/`.
- A driver never touches the queues or the poller directly — it goes through `DriverContext`. When a
  `Port` method calls the driver, **destructure `self`** (`let Port { driver, input, output, id, .. }`)
  so the driver and its context borrow disjoint fields; do not add a combined raw-pointer accessor.
- `Poller::poll` returns owned events; call `registry.handle_events(&mut poller, &events)` after it.
  Keys are port *numbers* (`PortId::number()`), so the registry looks the port up by `u64`.
- `FdDriver` closes the fds it is given (on `stop` and on `Drop`); do not close them elsewhere.
- `mio::unix::SourceFd` borrows `&mut RawFd` only for the duration of the syscall — build it from a
  local `let mut fd = fd;` inside the wrapper; the raw fd number is what mio registers.

## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types, the Stage-0…6 mapping (§7) and wave-5 position.
- `../../CONTEXT.md` → workspace contracts: §1 A.4 (driver C ABI) / A.6 (CLI/config), §3 (error strategy).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A (the two clean seams) and §H (Stage 5).
- `../beam_abi/CONTEXT.md` → the `ErlDrvEntry`/`ErlDrvApi` layouts the C-ABI bridge will target.
- `../beam_process/CONTEXT.md` → `PortTable`/`PortId` and the scheduler that will drive ports.
