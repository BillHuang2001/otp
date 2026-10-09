# `beam_io` — Poller/Reactor, Ports, Port-Task Scheduling, Built-in Drivers, io-device requests, Signals, CPU time, ddll

## Intent
Owns the **I/O subsystem** of the Rust ERTS rewrite (Stage 5, Part II §H) and the **OS/driver seam** — a clean rewrite seam (Part II §A) reproduced with the *frozen* driver C ABI (workspace `../CONTEXT.md` §1 **A.4**): a driver `.so` compiled against the unmodified `erl_driver.h` links and behaves identically.

Hosts nine subsystems: the `mio`-backed **poller/reactor**, the **`Port`** (a process-like I/O endpoint over a `Box<dyn Driver>` + two byte queues), **port-task scheduling** (`erts_port_task_execute`), the **io-device request layer** (`src/io_device/` — the `{get_until,…}` incremental reader: `file_io_server`'s `get_chars*` loop and `io_lib:get_until/4`'s `latin1`/`unicode` conversions, with the `Mod:Func` application a runtime-installed seam), the **`Driver` trait + built-in drivers** (`fd`, `efile`, OS-child `spawn`, `tcp_inet`, `{line,N}` buffering, the text⇄binary phase-change protocol), the **driver C-ABI host layer** (`src/abi/`), the **OS-signal facility** backing `os:set_signal/2` (the signal `unsafe` work `beam_rt`, `#![deny(unsafe_code)]`, cannot do itself), the **CPU-time facility** (`src/cpu_time.rs`) behind `erlang:statistics(runtime)`, and the **dynamic-library primitive** (`src/ddll.rs`) behind `beam_rt`'s `erl_ddll` floor.
The API Surface below carries the frozen detail, and the Routing Table names one file per subsystem.

`beam_rt` is the only consumer: it owns the poll loop, the port run queues and the scheduler thread, drives one long-lived `Poller` + `PortRegistry` plus a `SpawnDriver` built from its decoded `open_port/2` terms, and turns `PortNotification`s and the once-only `take_exit_status`/`take_eof` into `{Port,{data,…}}`/`{Port,{exit_status,…}}`/`{Port,eof}` for the owning process (waking a blocked receiver through `beam_process`); `beam_process` observes ports by id (the peer of pids in `ptab`).
Only `SpawnDriver` is on that path — `tcp_inet`, signal, CPU-time, ddll and the io-device request layer are **not** adopted yet (*Handoffs*).

Legacy C ground truth (read-only): `erl_poll.c` + `erl_check_io.c` (poll loop, one-shot re-armed readiness, self-pipe wakeup), `io.c` (the `ioq` queue + the driver callback call sites), `erl_port_task.{c,h}`, `inet_drv.c` + `packet_parser.{c,h}`, `erl_child_setup.c`, `erl_bif_ddll.c` + `erl_unix_sys_ddll.c`, `sys.c`, `erl_time_sup.c` + `erl_bif_info.c` — full paths in the Routing Table and the two child docs.

## API Surface (FROZEN — dependents route here)
Re-exported at the crate root and via `beam_io::prelude::*`.
Only **frozen contracts and non-obvious behaviour** are listed; rustdoc carries mechanical signatures.

### `poller.rs`
- `Token(usize)` — opaque registration handle; **token `0` is the waker's**, user tokens start at `1`.
  · `Interest` — readiness mask (`READABLE`/`WRITABLE`/`BOTH`).
  · `Ready` — observed readiness, **no public constructor** (only `Ready::default()`/`From<Ready> for Readiness`).
  · `Event` (`token`/`key`/`ready`); `Event::new` lets tests forge one.
- `Poller` — owns `mio::Poll` **and the single `mio::Waker`** created in `new()`; registered by raw `RawFd` + `Interest` + a caller `key`; `poll(timeout)` returns an **owned `Vec<Event>`** (`None` blocks); `waker()` an infallible cheap clone.
  `Waker` is the `Arc`-backed, cloneable, thread-safe handle to that one wakeup handle; its event carries `key() == WAKER_KEY` (`u64::MAX`).
- **`poll` is `EINTR`-tolerant**: an `ErrorKind::Interrupted` wait is **retried**, never returned as `BeamError::Io` (`mio` does not retry `EINTR`; `epoll_wait`/`poll`/`select` are never restarted after a handler runs, `SA_RESTART` notwithstanding).
  The retry **preserves the deadline** (`poll(Some(d))` subtracts elapsed time, so repeated signals never block past `d`; `poll(None)` resumes).
- **Regular-file fallback (internal, no signature change):** epoll refuses regular files (`EPOLL_CTL_ADD`⇒`EPERM`), so `register` keeps a `libc::poll(2)`-backed fallback set — an unsupported descriptor is watched with `poll(2)` and its readiness merged into `poll` in the same `Event`/`Ready` shape; the probe reports its `std::io::Error`; `poll` never blocks indefinitely while the set is non-empty (`FALLBACK_POLL_CAP`, 1 ms).

### `timer.rs` / `queue.rs`
- `Timeout` (`Copy`) — a single deadline; `Timer` — deadlines keyed by `u64`.
  · `ByteQueue` — a byte buffer whose `front()` is always contiguous.

### `port_task.rs` (port-task scheduling — `erts_port_task_execute`)
- Reduction constants: `CONTEXT_REDS = 4000`; `PORT_REDS_{INPUT,OUTPUT,TIMEOUT} = CONTEXT_REDS/100 (40)`; `PORT_REDS_EXECUTE = CONTEXT_REDS/4 (1000)`.
  `PortTaskOutcome::reds_used` models the per-kind cost only: the C additionally charges `erts_port_driver_callback_epilogue` + `ERTS_PORT_CALLBACK_VREDS` (`CONTEXT_REDS/20`) per task and `ERTS_PORT_REDS_FREE` on teardown, and bounds the round at `CONTEXT_REDS` (`erl_port_task.c:1730,1833,1848,1854-1860`).
  · `PortTaskKind { Input, Output, Timeout }` (`priority()`: Input < Output < Timeout; `reductions()`); `PortTask` (`Copy + Eq + Debug`).
  · `PortTaskQueue` — one pending task per kind (the C guards that in the caller, via the per-direction `ErtsIoTask` handle / the port's single `timeout_task` handle, `erl_check_io.c:1848-1871`, `erl_hl_timer.c:714-716`), ordered by kind **priority only** (the `seq` enqueue sequence is recorded but, with one task per kind, is never a live tie-break); a not-yet-due `TIMEOUT` stays queued (divergence: the C dequeues it, charges 0 reds and drops it, `erl_port_task.c:1759-1774`); `arm_timeout(deadline)` re-points/replaces a pending `TIMEOUT` (unlike the dedupe-only `enqueue_timeout`).
- `Readiness { readable, writable, error, read_closed, write_closed }` — `read_ready()` (readable ‖ read_closed ‖ error), `write_ready()` (writable ‖ write_closed ‖ error — a poll error is re-triggered for **every** armed direction, `ERTS_POLL_EV_ERR` = `EPOLLERR|EPOLLHUP`, `erl_poll.h:184`; `revents = state->active_events`, `erl_check_io.c:2018-2027`), `is_idle()`, `From<Ready>`.
  · `PortNotification::Data { port, bytes } | Closed { port }` — the seam the runtime turns into `{Port,{data,Data}}`/port-exit messages.
  · `PortTaskOutcome { pub ran, pub reds_used, pub notifications }`.

### `port.rs`
- `PortState { Opening, Open, Closing, Closed }`.
  · `DEFAULT_READINESS_RED_BUDGET = PORT_REDS_EXECUTE` — the budget `handle_events` runs.
  · `PortControl::CLOSE == "close"` — the one **reserved** control-op name (a namespace marker: any other name is a driver verb).
  · `PortCommandOptions { nosuspend, force }` — carried/validated but no-ops (ports never busy; the BIF layer owns their effect once a busy-port command queue exists).
- `Port` — owns a `Box<dyn Driver>` + the two byte queues; the scheduling surface (`queue_output`/`schedule_task`/`schedule_timeout`/`arm_timeout`/`cancel_timeout`/`next_timeout`/`pending_tasks`/`next_task`/`has_pending_task`/`timeout`) and the exit/async/stream surfaces (`take_exit_status`/`take_eof`/`has_pending_async`/`drive_async`/`notify_exit`).
  **Every driver interaction** (`start`/`output`/`ready_input`/`ready_output`/`control`/`timeout`) refreshes the port timer from `Driver::next_timeout` via `refresh_timeout`: `Some(d)` arms/**re-points** the single `TIMEOUT` task at `d`, `None` clears it.
  Re-exports `PortRegistry`.

### `registry.rs`
- `PortRegistry` — id allocation (inner `beam_process::PortTable`) + `HashMap<u64, Port>` by port number; basic ops (`new`/`open`/`open_named`/`get`/`get_mut`/`output`/`contains`/`len`/`is_empty`/`close`); `port_table`/`port_table_mut` expose that **same** id table (see the port-monitor handoff; `port_table_mut`'s rustdoc carries the borrowing rule).
  Frozen seams (all `BadArg` for an unknown id): `handle_events` (**schedules *and* runs** one round under `DEFAULT_READINESS_RED_BUDGET`); `execute_port_tasks` (port-number order under a reduction budget ⇒ a `PortTaskOutcome`; drives async completions for every live port whose driver answers `has_pending_async` *before* harvesting input, so a `driver_async` job reaches `ready_async`/`async_free` with no runtime call); `schedule_events`/`schedule_readiness` (enqueue only — an `INPUT`/`OUTPUT` task for directions the driver armed (`PortArms`)); `port_command` (queue output + schedule an `OUTPUT` task; the write happens in the task); `port_control` (reserved `"close"` closes with an empty reply; **any other `op`** goes verbatim to `control(op, arg)`, its reply returned); `take_exit_status`/`take_eof` (once-only reaped child status / EOF report); `schedule_timeout`/`cancel_timeout`/`next_deadline` (`next_deadline` = min over live drivers' `next_timeout`); `notify_port_exit` (`Driver::port_exit` **at most once**, port left live, independent of `close`/`stop`); `drive_async` (a manual async drive point for one port).

### `driver.rs`
- `DriverFlags(u32)` — `Copy + Eq`; `SOFT_BUSY`.
  · `PortExitReason { Closed, OwnerExited, Normal }`.
  · `DriverContext<'a>` — the driver's view of its port; **the only way a driver touches the poller or the queues** (`port`/`register`/`reregister`/`deregister`/`input`/`output`); registration keys are the port number (`pub(crate) PortArms` records the armed directions).
  It carries **no** message-send path and no per-frame drain (see the `tcp_inet` active-mode handoff).
- `pub trait Driver` — **frozen**.
  Required: `name()` (+ `flags()` default `empty()`), `start`, `output`, `ready_input`, `stop`.
  Defaulted: `ready_output` (= `output`), `control`, `timeout` (its `Result<()>` is discarded by `Port::run_task`'s `Timeout` arm — no reply channel), `next_timeout() -> Option<Instant>` (the declared deadline, mirrored into the port's `TIMEOUT` after each interaction; default `None`), `port_exit(PortExitReason)` (= `Ok(())`; the once-only advisory port-exit notification, distinct from the teardown `stop`), `drive_async` (= `Ok(0)`; `DriverPort` overrides with `poll_async`), `has_pending_async() -> bool` (= `false`; overridden via `AbiHost`), `take_exit_status() -> Result<Option<i32>>` (= `Ok(None)`; only `SpawnDriver` overrides), `take_eof() -> Result<bool>` (= `Ok(false)`; only `SpawnDriver`).
  Further capabilities must be additive with defaults that keep the built-ins compiling.
  · `DriverFactory = fn() -> Result<Box<dyn Driver>>`; `DriverRegistry` — name→factory (the `driver_tab.c` seam); `with_builtins()` registers **`"fd"`, `"efile"`, `"tcp_inet"`**.

### `drivers/` (each module's role is in the Routing Table)
- `fd.rs` — `FdDriver`, the built-in `"fd"` byte-stream driver; **owns** its fds (closed on `stop`/`Drop`).
  Ctors `pipe`/`connected(read_fd,write_fd)`/`read_only`/`write_only`; modes `with_line_mode`/`set_line_mode` (0 = unlimited)/`set_binary_mode`/`is_line_buffered`/`phase`/`set_phase`/`max_line_len`.
  `Driver`: `start` registers the read fd `READABLE`; `output`/`ready_output` drain `ctx.output()` to the write fd (`EAGAIN`⇒keep queued + arm `WRITABLE`); `ready_input` reads into `ctx.input()` (loops to `EAGAIN`, latches `read_eof` on `read(2)==0`, flushes a partial line at EOF, then **de-registers the read fd** so an exhausted level-triggered fallback file stops firing; the fd stays attached/owned, so a post-EOF `ready_input` is still `Ok`).
  `control` verbs `line [decimal(N)|"line,<N>"]`/`noline`/`binary`/`text`/`getopts` (reply `b"line,<N>"`/`b"binary"`); unknown⇒`BadArg`.
  `pub(crate)` write-slot seams `adopt_write_fd`/`release_write_fd`/`has_write_fd`/`attached_fd` (the efile read+write open attaches one fd in both slots).
- `line_buffer.rs` — `LineBuffer` + `DEFAULT_MAX_LINE_LEN = 1024`: frames on `\n` (a preceding `\r` kept as part of the terminator), splits a line longer than `max_len`, holds a partial line until `flush_all`/EOF.
  · `binary_io.rs` — `Phase { Text, Binary }` + `PhaseChange`, the `erl_child_setup` text⇄binary machine, default `Phase::Binary` (byte-identical to raw passthrough); `handle` frames Binary as one raw frame, Text as line frames; `flush` returns the trailing partial line (Text only).
- `efile.rs` — `EfileDriver` (`name() == "efile"`) delegates byte movement to a composed `FdDriver`.
  `control` verbs: **`open [<flags> ]<path>`** (mode list translated exactly as the C's `efile_translate_modelist`+`get_flags`, always plus `O_NONBLOCK|O_CLOEXEC`/`FILE_MODE 0666`; a bare `<path>` keeps the read-only form, spaces included, `open ./read` the documented disambiguation), `close` (both slots), `path`, `eof`, `position [<N>]`/`seek <whence>[,<offset>]` (whence = the C's `EFILE_SEEK_*`; reply = decimal absolute offset), `sync` (`fsync(2)`), `truncate [<len>]` (no arg ⇒ `ftruncate(fd, lseek(fd,0,SEEK_CUR))`; negative⇒`BadArg`), `binary`/`list` (records the public `FileDataMode` via `data_mode()`, selects the raw byte phase), plus the delegated `line`/`noline`/`text`/`getopts`.
  **`read_ahead`/`delayed_write` are NOT `open(2)` flags** (they are `prim_file`/`file`-level policies; the C translator drops them, `prim_file_nif.c:521-524`) and the unserved C modes `directory`/`skip_type_check` mean **every** directory open is `EISDIR`.
  **`truncate <len>` is an additive extension not in the C** (its `efile_truncate` takes no length; `file:truncate/2` is `undef` in the oracle).
  `open` refuses a **directory** with `EISDIR` and attaches a **read+write** open's descriptor in *both* slots via a `dup(2)` of one file description (each fd closed exactly once).
  Works on **regular files** (poller `poll(2)` fallback) and on epoll-servable descriptors (pipes/FIFOs/char devices/sockets).
  A failed syscall⇒the raw `BeamError::Io` errno (the `{error,atom}` rendering is the runtime's `prim_file` floor); an unopened port / unparsable argument⇒`BadArg`.
  `make_efile()` = its `DriverFactory`.
- `spawn.rs` — `SpawnStream { Binary, List }` (`Default = List`) = the `binary`/`list` option (the **runtime** builds `{Port,{data,Data}}`).
  `SpawnSpec { Spawn { command, argv }, SpawnExecutable { path }, Fd { fd } }` = the term-free `open_port/2` `Name` (`command()` = the `port_info(Port, name)` value, `fd:<N>` for the descriptor form); `SpawnOptions { stream, cd, env, args, exit_status }` (Default `list`); `StreamOptions { stderr_to_stdout, eof }` (Default both `false`) shapes only the child's streams.
  `build_command(spec, options)` sets argv, shell-vs-direct, `{cd,…}`/`{env,…}` and the standard streams (`stdin`/`stdout` piped, `stderr` inherited or — under `stderr_to_stdout` — an **exact fd-1 duplicate** via `pre_exec` `dup2(1,2)`); an empty command/path or argv⇒`BadArg`.
  `SpawnDriver` (`name() == "spawn"`) is **constructed** (`with_stream_options`), **really spawning** in the constructor and reaping eagerly; accessors `os_pid`/`command`/`stream`/`eof`/`exited()`, plus the once-only `take_exit_status()`/`take_eof()`.
  A failed spawn is either an `access(X_OK)`-rejected path (a constructor `BeamError::Raise` with the errno atom) or a path the check accepts but `exec` rejects (a searchable **directory**, a no-executable-format file) — the latter **opens the port**, the child's `_exit(errno)` becoming the exit status.
  `stop`/`Drop` kill + reap + deregister/close every descriptor, but an adopted `{fd,N}` is **never** closed.
  **The only module here that may spawn OS children** (project invariant).
- `inet/` — the built-in **`tcp_inet`** TCP socket driver and its read-path `{packet, N}` framing.
  Full detail (the control-verb table, the option wire, the `PacketType` decoder, the four `gen_tcp` capability handoffs, the test strategy) is in the child node **`src/drivers/inet/CONTEXT.md`** — read it there; this doc only records that `tcp_inet` is one socket behind one port and is **unreachable from the running runtime today** (see *Handoffs*).

### `src/abi/` — the driver C-ABI host layer (A.4)
Presents a **loaded C driver** (`beam_abi::driver::loader::load`) as a `Driver`; a `pub` sub-tree (`pub use crate::abi::*`); the only place in this crate holding raw `ErlDrv*` pointers and calling foreign code.
Four modules — `host.rs` (`PortHost` + `AbiHost`), `job.rs` (`AsyncJobs`, the bounded `driver_async` pool), `port.rs` (`DriverPort`, which prefers the entry's **`outputv`** on both write paths, plus the once-only `stop_select`), `table.rs` (the process-global `DriverTable` + the four registry slots reproducing the oracle's `0,1,0,-1`).
Full detail is in the child node **`src/abi/CONTEXT.md`** — read it there.

### `io_device/` — the io-device request layer (`{get_until, Enc, Prompt, Mod, Func, XtraArgs}`)
- `Encoding { Latin1, Unicode }` — the request's `Enc` / the device's `unic`; `name()`/`from_name()` (any other name — a tuple encoding `{utf16,binary}`, … — is refused, never guessed).
- `ByteSource` — the device's `read`: `read_chunk(size) -> Result<Option<Vec<u8>>>` (`None` = eof); impls for `std::fs::File` (EINTR-retrying), `SliceSource` (an in-memory slice) and `EmptySource`.
- `IoDevice` — `file_io_server`'s `#state{handle,buf,read_mode,unic}`: `new(source, encoding, read_mode)` / `with_buffer` / `encoding()`/`set_encoding` / `read_mode()`/`set_read_mode` / `buffer()`/`set_buffer`/`take_buffer`/`has_stashed_buffer` / `source_mut`; `get_until::<C>(&mut self, continuation: &mut C, out_encoding) -> Result<C::Reply>` is the request.
- The **device loop** (`read_size` = `READ_SIZE_BINARY` 8 KiB / `READ_SIZE_LIST` 128): `get_chars/5` (a stashed leftover is fed **before** any read, and cleared) → `get_chars_empty`/`get_chars_notempty` → `get_chars_apply`: the `latin1` clause matches only when `Enc == latin1` **and** `unic == latin1`; otherwise the general clause decodes/transcodes with `InEnc = unic`, `OutEnc = Enc`, carries the incomplete-UTF-8 tail (`NewBuff`) and, on a `{more,…}`, reads on; the reply's leftover is converted back to the stream encoding and joined with the tail. EOF is delivered as `eof`; a `unicode` sequence that spans the end of file is the `invalid_unicode` stop.
- The **`Continuation` seam** (`Continuation`/`ContData`/`Applied`/`Leftover`) — the oracle's `apply(Mod, Func, [Cont, Chars | XtraArgs])` (`erl_scan:tokens/4` for `io`'s own calls): `apply(cont, ContData, out_encoding) -> Result<Applied<Cont, Reply>>`; the charlist the continuation receives is a `Vec<u32>` of code points; `cast_reply` is `io_lib`'s list→binary cast (identity by default). `NoEngineContinuation` is the not-installed default: every `apply` is the typed `unsupported` (`NO_ENGINE_CONTINUATION` names the missing `erl_scan:tokens/4`), never a fabricated reply.
  **Deviations:** term-free (byte/code-point data, caller-defined reply/cont types); the `{stop, Reason, {error, Fn}, State}` reply boxing and the `erl_scan`-specific `invalid_unicode_error/4` position hack are the runtime/protocol layer's; `Leftover` is a flat `binary | charlist | nothing` (no deep iolist).

### `signal.rs` — the OS-signal facility (safe `sigaction` + self-pipe)
- `Signal` — the twelve signals `os:set_signal/2` accepts, fixed by `signalterm_to_signum` (`sys.c`) and `lib/kernel/src/os.erl`: `SigHup`/`SigQuit`/`SigAbrt`/`SigAlrm`/`SigTerm`/`SigUsr1`/`SigUsr2`/`SigChld`/`SigStop`/`SigTstp`/`SigCont`/`SigWinch` (`sigcont`/`sigwinch` ARE accepted; `sigttin`/`sigttou` are **not** — `!`-commented rows in the C table; `siginfo` is `#ifdef SIGINFO` and excluded).
  `Signal::ALL: [Signal; 12]` (the `drain` report order); `name()`/`from_name()` (the lowercase BEAM atom, exact/case-sensitive); `signo()`/`from_signo()` (the `libc::SIG*`).
  · `Disposition { Default, Handle, Ignore }` — `ALL`; `name()`/`from_name()` for `default`/`handle`/`ignore`.
- `SignalFacility` — `new() -> Result<Self>` (self-pipe via `pipe2(O_CLOEXEC|O_NONBLOCK)` on Linux/Android, `pipe`+`fcntl` fallback on other unix; publishes the write end to the process-global handler plumbing); `set(&mut self, Signal, Disposition)` (`sigaction(2)`: `SIG_DFL`/`SIG_IGN`/the trampoline, **empty `sa_mask`**, **`SA_RESTART`**; the prior action is captured only on the first change of a signal); `disposition(&self, Signal) -> Result<Disposition>` (read back from the kernel); `read_fd(&self) -> RawFd` (the fd the runtime registers with `Poller` — **the runtime owns the registration/token**); `drain(&mut self) -> Vec<Signal>` (**each distinct signal since the last drain exactly once**, in `Signal::ALL` order, then drains the pipe).
  `Drop` restores every prior disposition and closes the pipe; it resets the pending bitmask **only** when the global write-end CAS shows this facility was still the live one, so a *superseded* facility never clobbers its successor's pending reports.
- **Async-signal-safety (frozen).** The installed handler does exactly two async-signal-safe things: one atomic OR into a `static AtomicU64` per-signal bitmask and one `libc::write` of the signal number to the non-blocking pipe; `drain` does the signo→`Signal` mapping in normal context.
  The pipe byte count is **not** the source of truth (a stray/coalesced byte never duplicates a report; a dropped byte never loses one).
  **Single instance:** the trampoline, the bitmask and the write fd are process-global statics — keep exactly one facility alive; `new`-after-`drop` is safe.
  **Portability:** unix only; no `signalfd`.
  **Errors:** `BeamError::Io` for a failing `pipe`/`sigaction`; no panic/`unwrap`/`todo!`/`unreachable!`.

### `cpu_time.rs` — the CPU-time facility (safe `getrusage`/`clock_gettime`)
- `CpuTimes { user_ms, system_ms }` (`Copy + Eq`) — a CPU-time reading in whole **milliseconds** (the oracle's unit: µs ÷ 1000, truncating).
- `process_cpu_times() -> Result<CpuTimes>` — `getrusage(RUSAGE_SELF)`, the **process's** user+system time (the sum over its threads); exactly the C's measurement for `erlang:statistics(runtime)` (`erts_runtime_elapsed_both`, `erl_time_sup.c:1421-1481`).
- `thread_cpu_times() -> Result<CpuTimes>` — `clock_gettime(CLOCK_THREAD_CPUTIME_ID)`, the calling thread's **combined** user+system time reported as `user_ms` with `system_ms == 0` (that clock does not split the two); the analogue of the emulator's `enif_cpu_time`, **not** what `statistics(runtime)` uses.
- `RuntimeClock` (`new`/`Default` anchor **0**, `runtime() -> Result<(u64, u64)>`, `reset()`) — the `erlang:statistics(runtime)` counter: `runtime()` reads the process's user CPU time and returns `(Total, SinceLastCall)` in **milliseconds**, `SinceLastCall` being the difference from the previous call, so the **first** call returns two equal elements (the C's `runtime_prev.data.user = 0`).
  A **failed** `getrusage` returns the typed `BeamError::Io` and leaves the anchor **unchanged** (never a fabricated `0`); there is no lock (the C's mutex exists because any scheduler thread may call it; this is a plain value the single-threaded runtime owns).
  `reset()` is an additive seam the C has no equivalent of.
- Faithful to `erts/emulator/beam/erl_time_sup.c:1421-1481` + `erl_bif_info.c:4277-4291` (`statistics(runtime)` = `{Total, SinceLastCall}`; **system time is NOT used** by that arm).
  `unsafe` appears only in the two syscall wrappers, each with a `// SAFETY:` rationale.
  Re-exported at the crate root and in `prelude`.

### `ddll.rs` — the dynamic-library primitive (safe `dlopen`/`dlsym`/`dlclose`)
- `DynLib` — an owned `dlopen(3)` handle; `Drop` calls `dlclose(3)` exactly once.
  `open(&Path)`/`open_with(&Path, DynLibFlags)`/`open_cstr(&CStr, DynLibFlags)`; `symbol(&CStr) -> Result<*mut c_void, DllError>`/`try_symbol(&CStr) -> Option<*mut c_void>`; `handle() -> *mut c_void`.
  Deliberately **no** `into_raw`/`from_raw` (an owned, reliably-closed handle) and not `Send`/`Sync` (raw pointer — single-OS-thread, the crate's invariant).
- `DynLibFlags` — `EMULATOR` (`RTLD_NOW | RTLD_LOCAL`, the default and the emulator's mode, `erl_unix_sys_ddll.c:133`; **matches** `beam_abi::driver::loader`/`nif::loader`), `NOW_GLOBAL`, `LAZY_LOCAL`, `LAZY_GLOBAL`, `from_bits`/`bits`.
- `DllError { kind, text }` + `DllErrorKind` — the `erl_ddll` reason vocabulary: `Open` (the dynamic `{open_error, Code}` case), `SymbolNotFound`, `NoDriverInit`, `DriverInitFailed`, `BadDriverName`, `DriverNameTooLong`, `DriverIncorrectVersion`, `NoDdllAvailable`, `Unspecified`, `BadArg`; `reason_atom()`, `errcode()`, `text()`, `is_missing_file()`/`is_format_error()`/`is_undefined_symbol()`.
  `DllErrorKind::errcode()` is **derived** from the single `ERRCODE_TAB` via `atom_to_errcode(reason_atom())` (not a second hand-written match).
  Classification is **text-based only** (a failed `dlopen` does not set `errno` reliably and a stale value must never be trusted), mirroring the emulator's `find_errcode`.
- `ERRCODE_TAB` (the emulator's `errcode_tab`, `erl_bif_ddll.c:1709-1720`, byte-for-byte), `errcode_to_atom`/`atom_to_errcode` (`const fn`), `format_error_text`, `symbol_name(&str) -> Result<CString, DllError>`, and the `ERL_DE_*` numeric constants.
  Path spelling is the caller's: **no** `.so` suffix is appended (unlike `erts_sys_ddll_open`).
  The `-Wl,--export-dynamic` requirement is a property of the **loading** link, shared with `beam_abi`/`beam_rt` (an `RTLD_NOW` load resolves `enif_*`/`driver_*` out of the loading executable's `.dynsym`); this crate's part is its `build.rs`.

## Constraints
- Errors: **`beam_common::{Result, BeamError}` only**; I/O/syscall failures⇒`BeamError::Io`, argument faults⇒`BeamError::BadArg`, id/registration exhaustion⇒`BeamError::SystemLimit`.
  Never panic on runtime data, never `unwrap()`, never `todo!`/`unreachable!` in shipped code.
- **Safe Rust by default.** `unsafe` only in the raw syscall wrappers (`libc::read`/`write`/`pipe`/`fcntl`/`open`/`dup`/`fstat`/`lseek`/`fsync`/`ftruncate`/`close`, the spawn driver's `fork`/`execve`/`waitpid`/`access`/`dup2`, the `inet` socket helpers incl.
  `setsockopt`/`getsockopt` in `drivers/inet/sys.rs`, the `cpu_time.rs` `getrusage`/`clock_gettime`, the `signal.rs` `pipe2`/`sigaction`, the `ddll.rs` `dlopen`/`dlsym`/`dlclose`), `mio::unix::SourceFd` construction, and the `abi` host's raw `ErlDrv*` handling.
  Every `unsafe` block carries a preceding `// SAFETY:` rationale; `unsafe_op_in_unsafe_fn` stays clean.
- **Port ids come from `beam_process::PortTable`** — no second number allocator; `PortId` encoding stays `beam_term`'s internal-port immediate; a failed `start` runs the driver's teardown (deregistering any fd) and releases the number — no id-space/poller leak.
- **Single-thread invariant (enforced).** `Port`/`PortRegistry` are **not** thread-safe and this crate has no locking: create and drive them from **one OS thread**.
  `PortRegistry` carries a `PhantomData<core::cell::Cell<()>>` marker so it is neither `Send` nor `Sync`; a `Port` holds a `Box<dyn Driver>` with no `Send`/`Sync` bound; `DynLib` is likewise not `Send`/`Sync`.
  (Not enforced on `Poller`/`Waker` — the `Waker` is meant to cross threads; `SignalFacility` is process-global.)
- **Only `drivers/spawn.rs` may spawn OS children**; the `signal`/`cpu_time`/`ddll`/`inet`/`file` modules never fork/`Command::spawn`.
- **The `Driver` trait and `DriverContext` public methods are the frozen driver seam** — do not change their signatures.
  Files target ~1000 lines, split by responsibility.
  Deps: `beam_common`, `beam_term`, `beam_process`, `beam_abi`, `beam_alloc`, `libc`, `mio` only; no new deps (see *Design Decisions* for the `beam_io → beam_abi`/`beam_alloc` edge).

## Test Strategy
Gate: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` **GREEN**, and `... build --workspace --all-targets` **GREEN, warning-free**.
Current: **484 passing / 0 failing / 2 ignored** — **322** lib in-module (`#[cfg(test)]`) + **160** across the 16 `tests/*.rs` suites + **4** doctests (2 passed: the `!Send`/`!Sync` compile-fail pins on `PortRegistry`; 2 `ignore`d: `cpu_time::RuntimeClock`, `port`).
- **In-module (322):** `drivers` 183 (`inet` 115, `spawn` 23, `line_buffer` 13, `fd` 13, `efile` 11, `binary_io` 8), `abi` 33 (`host` 15, `port` 8, `table` 7, `job` 3), `registry` 25, `signal` 19, `port_task` 12, `poller` 12, `ddll` 11, `timer` 9, `queue` 8, `driver` 6, `error` 4.
- The C-driver suites and the `erl`-oracle cases **skip loudly** without `cc`/`erl_nif.h`/`erl`.
  The C-driver fixtures assert a real `beam_abi::driver::loader::load`; **linking needs BOTH `build.rs`'s `-Wl,--export-dynamic` and a `driver_api_table()` reference from the test binary** (either alone ⇒ `dlopen(RTLD_NOW)` fails with `undefined symbol: driver_mk_port`).
- The signal suite is serialized on a global `Mutex` (the facility is process-global) and the `cpu_time` suite asserts only monotone/structural properties (real syscalls never exact values).

## Design Decisions
- **`mio` backend.** A portable, safe `Poll`/`Registry`/`Waker` with a one-shot re-arm matching the C's `EPOLLONESHOT`/`EV_ONESHOT`; the public API is **mio-independent** (`RawFd` + `Interest`/`Token`/`Ready`), so an io_uring/`/dev/poll` backend is a drop-in.
  `mio` fd registration is `os-ext`/Unix-only, so Windows/iocp is out of scope (as in `beam_abi`).
- **The `Port` owns both `ByteQueue`s, not the driver** (the C `ioq`): `Port::output` appends then calls `Driver::output`, which **pulls** from `ctx.output()` — no double buffering.
- **Task execution here, the scheduler thread above.** The poll loop, port run queues and scheduler-charged reductions are `beam_process`/`beam_rt`; `PortNotification` keeps the engine/runtime split and the DAG clean.
  Only `execute_port_tasks` yields `Data`/`Closed`.
- **Readiness ⇒ task, gated by `PortArms`**: a full close (`EPOLLHUP` ⇒ `read_closed` **and** `write_closed`) schedules no bogus OUTPUT task on a read-only driver.
  **Divergence:** the C keeps three FIFO queues with no cross-kind priority; here one queue is kind-priority-ordered (INPUT → OUTPUT → TIMEOUT, `seq` tie-break) — observable only in the order two *different* kinds run in a round.
- **The spawn driver is constructed, not registered, and reaps eagerly:** a `DriverFactory` cannot carry command/argv/options, so it is built from a `SpawnSpec`; the child starts in the **constructor** (so a failed spawn registers nothing and `os_pid()` is readable before `PortRegistry::open`); `{fd, N}` adopts a descriptor as a **duplex** stream and never closes it; `stop`/`Drop` kill **and** reap (`Drop` reaps because Rust's `Child` does not).
  The failed-spawn split and the `stderr_to_stdout` alias are detailed in the `spawn.rs` API bullet above.
- **The TCP driver is a floor, not a full `inet`** — the served verbs incl.
  `setopts`/`getopts` and the framing types are oracle-cross-checked (see `src/drivers/inet/CONTEXT.md`).

## Known Issues
- **`FdDriver` is deliberately minimal.** Byte-stream only; line mode frames input but does **not** line-frame output (the emulator's line flag); no `inet`/`forker` drivers; no packet framing.
- **`SpawnDriver` is the OS-child floor, not the whole `spawn` driver.** Decoding `open_port/2` into `SpawnSpec`/`SpawnOptions`/`StreamOptions` and synthesizing `{Port,{data,…}}`/`{Port,{exit_status,…}}`/`{Port,eof}` is the runtime's job — this crate only moves bytes, reports the latched EOF/exit status **once**, and kills/reaps.
  Writing to a child that closed its stdin fails with a typed `BeamError::Io` (the remainder kept); adoption requires non-blocking (`F_SETFL`+`O_NONBLOCK`), and a descriptor that refuses (some inherited ones ⇒ `EPERM`) fails the open with `BeamError::Io`.
  Input is not line-framed (no `line` verb): the `binary`/`list` choice rides `SpawnStream`.
- **`port_control`/control atoms are not decoded here.** `port_control(op, &[u8])` takes a `&str` op + raw bytes; the C decodes Erlang tuples in `io.c`, so decoding `{command,…}`/`{connect,…}` is the runtime's job.
- **This crate has no poll thread of its own.** `execute_port_tasks` runs a round on demand; the per-iteration drive (poll → `schedule_events`/`handle_events` → `execute_port_tasks`), the reduction budget and the single long-lived `Poller` live in `beam_rt`'s single-threaded main loop.
- **File sizes (`wc -l`).** Over 1000: `src/drivers/spawn.rs` 1966, `src/drivers/inet/tests.rs` 1447, `src/drivers/efile.rs` 1343, `src/drivers/inet/mod.rs` 1296, `src/registry.rs` 1278, `src/abi/host.rs` 1267, `src/drivers/inet/packet.rs` 1245, `src/drivers/fd.rs` 1141, `src/signal.rs` 1061 — each is a source half plus its suite (or, for `inet`, a subtree with its own child doc).
  All other modules are under 1000; if a source half grows, split its test module into a sibling `tests/` file (as `src/drivers/inet/` already does).

## Handoff to beam_rt — the CPU-time facility
`beam_rt` owns the `erlang:statistics(runtime)` BIF and must adopt `src/cpu_time.rs`:
- Own **ONE `RuntimeClock` for the runtime's entire life** (a fresh clock per call would make `SinceLastCall == Total` every time — the anchor is the C's process-global `runtime_prev`), and serve the `statistics/1` `runtime` arm from `RuntimeClock::runtime()` (the `RuntimeHooksBridge` arm, `crates/beam_rt/src/context/runtime_hooks.rs`), rendering the returned `(Total, SinceLastCall)` ms pair as the oracle's 2-tuple of integers (already the oracle's shape and units — no runtime arithmetic).
- On a failed `getrusage` the error is returned and the anchor left unchanged (never a fabricated `0`).
- `statistics(runtime)` is the typed `unsupported_bif` today; adopting this closes the gate gap **`proc_signals/statistics_runtime_unsupported`** (`../../tools/differential/GAPS.md`).

## Handoffs (beam_rt — out of this node)
**Runtime adoption still owed** (seams exist and are tested here):
- **The `tcp_inet` driver is unreachable through the runtime's `open_port/2`.** `open_port({spawn_driver,"tcp_inet"}, …)` never finds the built-in: `beam_rt`'s `driver_lookup` (`crates/beam_rt/src/port.rs:1670`, used from the `SpawnDriver` branch at `:1322`) only `dlopen`s `.so` candidates and never consults `beam_io::DriverRegistry::with_builtins()` (`src/driver.rs`), so the open is `badarg`; `make_tcp_inet` and its registry entry are implemented and tested here.
  **Owner: `beam_rt` (its driver resolver).**
- **Four `tcp_inet` capabilities need a cross-crate seam** — `{active,N|once}`/`{deliver,…}`/`active_count` input delivery, `send_timeout`/`send_timeout_close`, `controlling_process`, and the async-op queue.
  Each is a real `gen_tcp` feature this floor refuses honestly today; the owner, the missing seam and the C ground truth for each are stated in **`src/drivers/inet/CONTEXT.md` › *Handoffs***.
- **The OS-signal facility (`os:set_signal/2`).** `beam_bifs` registers `os:set_signal/2` as a `RuntimeHooks`-gated key that `beam_rt` does not install, so a call is the typed `unsupported_bif`.
  **API contract:** map the atom via `Signal::from_name`/`Disposition::from_name` (anything else ⇒ the oracle's `badarg`); keep **one** long-lived `SignalFacility` for the runtime's life; `set(sig, disp)` installs (`Ok(())` = the oracle's `ok`, else `BeamError::Io`); register `read_fd()` with the process `Poller` (`Interest::READABLE`, runtime-chosen key — the runtime owns the registration/token, and `poll` is `EINTR`-tolerant); on readiness `drain()` → `Vec<Signal>` in `Signal::ALL` order → deliver `{notify, Signal::name()}` to the registered `erl_signal_server`.
  Drop it (rather than signalling after shutdown) so `Drop` restores every disposition.
  `beam_bifs/CONTEXT.md` is internally inconsistent on `os:set_signal/2` (owned by that node — worth reconciling).
- **`erl_ddll` → `beam_io::ddll`.** `DynLib` is the safe `dlopen`/`dlsym`/`dlclose` primitive `beam_rt`'s `erl_ddll` floor must build on: open with `DynLibFlags::EMULATOR`, resolve `driver_init`/`_driver_init`, render `{error, Atom}` from `DllError::reason_atom()`/`ERRCODE_TAB`.
  **The dynamic `{open_error, Code}` is the floor's to own** — an index into the process-local table of raw `dlerror` strings (`find_errcode`, `ERL_DE_DYNAMIC_ERROR_OFFSET = -10`), so `beam_rt` keeps that table (built from `DllError::text()`) and builds the tuple; `DllErrorKind::BadArg` is a **raise**, not an `{error, Atom}` return.
  **Dependency direction:** `beam_io` depends on `beam_abi`, so `beam_abi`'s own loaders cannot call this module — a future consolidation could move the wrapper **down** into `beam_abi` and re-export it from `beam_io`.
- **The `erl_ddll` error taxonomy is duplicated OUTSIDE this crate (report — not fixable here).** A full atom+code+message copy lives in `crates/beam_bifs/src/runtime/ddll.rs` (`ERRCODE_ERRORS` ≈`:119-135`, `CODE_ERRORS` ≈`:144-154`, `DYNAMIC_ERROR_OFFSET` ≈`:139`) and a partial one in `crates/beam_abi/src/driver/loader.rs` (`DriverLoadError::reason_atom` ≈`:151-160` + message consts ≈`:112-116`).
  The recommended single owner is `beam_abi` (with `beam_io::ddll` re-exporting) — but `beam_abi` cannot depend on `beam_io`, and `beam_bifs` does not depend on `beam_abi`, so that choice adds a new `beam_bifs → beam_abi` DAG edge; the zero-new-edge alternative is the LCA `beam_common`.
  Either way it is an **owner decision + a `CHANGE.md` entry**, not this node's.
  Artifact-validity validation itself is correctly single-owner per kind (driver: `beam_abi::driver::loader::load`; NIF: `beam_abi::nif::loader::load`) and `beam_io::ddll` validates no entry.
- **File-module surface: the `file:*` API belongs to `beam_rt`'s `prim_file` floor, not to this driver.** The `efile` driver offers the driver-level verb set, but nothing in the running runtime reaches it — `file:*`/`prim_file` is a Rust-`std::fs` floor in `beam_rt/src/native/prim.rs`.
  **Served there:** `get_cwd/0,1`, `read_file/1`, `delete/1`, `rename/2`, `make_dir/1`, `del_dir/1`, `read_link/1`, `make_link/2`, `make_symlink/2`, `altname/1`.
  **Unimplemented** (they fall through to the preloaded `.beam`): `file:open/2`, `read/2`, `write/2`, `position/2`, `sync/1`, `truncate/1`, `copy/2`, `list_dir/1`, `write_file/2`, the `read_file_info/*`+`read_link_info/*` family, `datasync/1`, `advise/4`.
  Wiring `file:open/2`+`read/2`+`write/2`+`position/2` onto this driver is the natural next step: the driver returns the raw errno and the raw byte stream, so `prim_file`'s remaining job is the `{error,atom}` rendering, the `FileDataMode` → binary/string rendering (`EfileDriver::data_mode`), and `read_ahead`/`delayed_write` (**not** `open(2)` flags — the C translator drops them, `prim_file_nif.c:521-524`).
- **`erlang:port_control/3` → `PortRegistry::port_control`.** The registry seam (with the reserved `"close"`) and a loaded C driver's `control` reply (through `rbuf`/`cres`) are pinned by tests; the BIF/`DrvPort` wiring is owed, and the runtime's own `.so` `DriverPort` adapter must forward `control` too (its number-verb form).
- **`open_port/2` stream options → `SpawnDriver::with_stream_options`.** The runtime must pass `stderr_to_stdout`/`eof` as a `StreamOptions` and, for an `eof` port, poll `PortRegistry::take_eof(id)` and emit the bare `{Port, eof}` message (the oracle's shape) exactly once.
- **`tcp_inet` peer close → `{tcp_closed, Port}`.** `TcpInetDriver::peer_closed()` latches the close and the driver's `recv`/`send` already answer the C's `[0]closed`/`[0]enotconn`; the runtime needs a `Driver` accessor (e.g. `port_closed()`) to observe it and deliver `{tcp_closed, Port}`.
- **A `>64`-byte `binary`-stream read is a `beam_rt` gap** (a port read larger than the on-heap binary limit must become an off-heap refc binary in the runtime's binary-build path); and **file/`prim_file`/`standard_io`/`io_request` handling is `beam_rt`/`beam_bifs`, not `beam_io`** (this crate provides only the `efile`/`fd` drivers and framing).
- **`erlang:monitor/2` on a port target → `PortRegistry::{port_table,port_table_mut}`.** Those accessors hand out the registry's **own** id table (the `PortTable` `open`/`close` allocate/release through) — the `&mut PortTable` operand `beam_process::ProcessTable::{monitor_port,monitor_port_term,demonitor_port}` require; `ProcessTable::monitor_term` answers a typed `unsupported` for a port target, so a port monitor MUST be routed through the port API with this table.
  **Borrowing rule (in `port_table_mut`'s rustdoc):** the `&mut PortTable` borrows the registry, so a caller holding it behind a `RefCell` (`beam_rt`: `Rc<RefCell<PortRegistry>>`) must take the registry's `RefMut` + the `&mut PortTable`, run the single monitor/demonitor call, and drop the whole chain before any nested borrow of the same registry (else `beam_rt`'s `registry_mut` returns the typed `error::unsupported(...)`, or a bare `borrow_mut()` panics); the `&mut ProcessTable` side has the same rule.
  **Runtime adoption is owed.**

## Routing Table
- `src/lib.rs` → crate docs, module wiring, root re-exports, `prelude`
- `src/poller.rs` → `Poller` (`EINTR`-tolerant `poll`, `libc::poll(2)` fallback), `Token`, `Interest`, `Ready`, `Event`, `Waker`
- `src/timer.rs` → `Timeout`, `Timer`; `src/queue.rs` → `ByteQueue`
- `src/port_task.rs` → `PortTaskKind`/`PortTask`/`PortTaskQueue`, the reduction constants, `Readiness`, `PortNotification`, `PortTaskOutcome`
- `src/port.rs` → `Port`, `PortState`, `PortControl`, `PortCommandOptions`, `DEFAULT_READINESS_RED_BUDGET`; re-exports `PortRegistry`
- `src/registry.rs` → `PortRegistry` (ids over `beam_process::PortTable`, the live-port map, reactor glue, the timer/exit/eof/async seams incl.
  `port_table`/`port_table_mut`)
- `src/driver.rs` → `Driver`, `PortExitReason`, `DriverContext`, `DriverFlags`, `DriverRegistry`, `pub(crate) PortArms`
- `src/signal.rs` → `Signal`/`Disposition`/`SignalFacility`
- `src/cpu_time.rs` → `CpuTimes`, `process_cpu_times`, `thread_cpu_times`, `RuntimeClock`
- `src/ddll.rs` → `DynLib`, `DynLibFlags`, `DllError`/`DllErrorKind`, `ERRCODE_TAB`/`errcode_to_atom`/`atom_to_errcode`/`format_error_text`/`symbol_name`
- `src/drivers/fd.rs` → `FdDriver` (line/phase modes, `control`, EOF de-registration, the write-slot seams)
- `src/drivers/line_buffer.rs` → `LineBuffer`, `DEFAULT_MAX_LINE_LEN`; `src/drivers/binary_io.rs` → `Phase`, `PhaseChange`
- `src/drivers/efile.rs` → `EfileDriver`, `make_efile`, `FileDataMode`
- `src/drivers/spawn.rs` → `SpawnDriver`, `SpawnSpec`/`SpawnOptions`/`StreamOptions`/`SpawnStream`, `build_command`
- `src/drivers/inet/` → the `tcp_inet` driver + read-path framing (child node `src/drivers/inet/CONTEXT.md`)
- `src/drivers/mod.rs` → driver-module wiring + re-exports
- `src/abi/` → the driver C-ABI host layer (child node `src/abi/CONTEXT.md`)
- `src/error.rs` → `pub(crate)` error constructors, `errno_atom`, `spawn_failure`
- `tests/poller.rs` (11) → reactor gate: registration/readiness on real pipes/socketpairs (`key`, no-events, `reregister` flip, `deregister`, double-register), timeout/waker, `EINTR` tolerance
- `tests/port.rs` (4) → the `open`→`output`→`poll`→`handle_events`→`read_input` round-trip, `close` id release, `open_named`, unknown keys incl.
  `WAKER_KEY`
- `tests/port_task.rs` (4) → scheduling + the receive-wake path (`PortNotification::Data` → `deliver_simple` → `Scheduler::resume_woken`)
- `tests/signal.rs` (21) → the real-signal gate (the 12-signal set, coalescing, the async-signal-safe trampoline, `Drop` restore/global-end CAS)
- `tests/cpu_time.rs` (8) → the `erlang:statistics(runtime)` floor: real `getrusage`/`clock_gettime` monotone/structural properties + the `RuntimeClock` anchor semantics
- `tests/ddll.rs` (11) → `dlopen`/`dlsym`/`dlclose` round-trips, the `cc` fixture `.so`, the hermetic undefined-symbol case, the error→atom mapping + the single-sourcing pins
- `tests/efile.rs` (16) → real files through the reactor + the `poll(2)` fallback: flag modes, `position`/`seek`/`sync`/`truncate`, `binary`/`list`+`data_mode`, `EISDIR`/`ENOTDIR`/`EEXIST`/`ENOENT`, `BadArg`, the all-mode oracle sweep
- `tests/spawn.rs` (27) → real children: both queues, notifications, every `Name`/option form, EOF deregistration, write-readiness, unknown-id `BadArg`, once-only exit status, kill+reap, duplex `{fd,N}`, a 200 KiB stderr flood, a failed spawn's errno atom + the oracle reason
- `tests/inet.rs` (8) → `tcp_inet` on loopback TCP: round-trip, write-readiness, unknown-verb `BadArg`, the peer-close shapes + the `erl`-oracle cross-checks of the control replies, the socket-option replies and the driver-state option replies (10 byte-for-byte)
- `tests/driver_seams.rs` (5) → the driver-declared timer (`next_timeout` → `timeout`) + the once-only `notify_port_exit` seam
- `tests/fixture_builds.rs` (6) + `tests/fixtures/{host_driver.c,host_driver_no_ready_async.c,vector_driver.c}` → the `cc`-built C-driver fixtures (incl. the `outputv`-only one), with a load assertion
- `tests/support/mod.rs` → shared `cc`-compile/scratch-dir helpers (per-run fresh dirs, so a fixture's append-mode log never leaks between runs)
- `tests/cdriver/mod.rs` → the shared C-driver helpers (`compile_so`/`load_driver`/`open_port`/`round_data`/`await_log_line`/`logged_port_command`/`LoggedPort`) reused by `abi_{driver,async,vector}.rs`
- `tests/abi_driver.rs` (16) → the end-to-end C-driver path: command parsing, `output` echo, write-readiness → `ready_output`, `driver_output` → one `send_term`, timer arm/fire/cancel, `driver_select`/deselect → a `Data` notification, `stop_select` on deselect/teardown/`Drop` (each exactly once), `driver_async` via a plain round, `close` → `stop` + `Closed`, the sticky `ERL_DRV_USE` re-selects
- `tests/abi_async.rs` (8) → the `driver_async` completion arms (`ready_async`; `async_free` via the variant build; the NULL-invoke release; drop-while-in-flight; drop-before-drain — each `async_free` exactly once) + the `ErlDrvPort` pointer-identity test + a plain-thread `driver_output` refused
- `tests/abi_vector.rs` (7) → the `outputv`-only entry on both write paths + the async-free shapes, over `tests/fixtures/vector_driver.c`
- `tests/fixtures/ddll_symbol.c` + `tests/fixtures/ddll_undefined.c` → the `cc` ddll fixtures (a resolvable symbol; a hermetic undefined symbol)
- `tests/driver_table.rs` (5) → the process-global registry end-to-end: `add_loaded` → `create_port` → `start`/`output`/`stop` in order (+ `driver_output` → port data), a second port from one registration, the `badarg`/`notsup`/NUL-command failures, the reactor drive path
- `tests/driver_registry_slots.rs` (3) → the four registry slots over a live fixture port (the oracle's `0,1,0,-1`) + the port-less defaults
- `../beam_process/src/{ptab,scheduler,process}.rs` → `PortTable`/`PortId`, `Scheduler::resume_woken`, `ProcessTable::deliver`/`drain_woken`, `DeliverOutcome` (sibling — read-only)
- `../beam_rt/src/port.rs` → the reference `open_port/2` term decoding (`RuntimePorts::{name_spec,command_spec,port_options,env_list,arg_list,spawn}`) that `SpawnSpec`/`SpawnOptions`/`build_command` mirror term-free, and `driver_lookup` (the `tcp_inet` reachability gap) (sibling — read-only)
- `../beam_abi/CONTEXT.md` → the frozen `ErlDrvEntry`/`ErlDrvApi` driver C ABI that `src/abi/` bridges onto the `Driver` trait (sibling — read-only)
- `../../erts/emulator/{sys/common/erl_poll.c,sys/common/erl_check_io.c,beam/io.c,beam/erl_port.h,beam/erl_port_task.c,beam/erl_port_task.h,drivers/common/inet_drv.c,beam/packet_parser.c,beam/packet_parser.h,sys/unix/erl_child_setup.c,beam/erl_bif_ddll.c,sys/unix/sys.c,beam/erl_time_sup.c,beam/erl_bif_info.c}` → the legacy C ground truth listed under *Intent* (read-only reference)

## Notes for Agents
- Read `../CONTEXT.md` (crate-layer contracts, the poller/`Port`/driver-registry seam), `../beam_process/CONTEXT.md` (`PortTable`/`PortId`, `Scheduler`/`ProcessTable`) and `../../CONTEXT.md` §1 A.4/§3 before changing this crate.
- Build/test: `erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_io` (sandbox `$HOME` is read-only — always the wrapper).
  Never `cd`; never touch `../../tools/`.
- A driver never touches the queues or the poller directly — it goes through `DriverContext`.
  When a `Port` method calls the driver, **destructure `self`** (`let Port { driver, input, output, id, .. }`) so the driver and its context borrow disjoint fields; do not add a combined raw-pointer accessor.
- `Poller::poll` returns owned events and is `EINTR`-tolerant; call `registry.handle_events(&mut poller, &events)` after it, or drive the queue explicitly with `schedule_events` + `execute_port_tasks`.
  Keys are port *numbers* (`PortId::number()`).
  To be woken by a handled OS signal, register `SignalFacility::read_fd()` with the poller.
- `FdDriver` closes the fds it is given (on `stop` and on `Drop`); do not close them elsewhere.
  `EfileDriver` composes an `FdDriver`, so the same ownership rule applies through it.
- `mio::unix::SourceFd` borrows `&mut RawFd` only for the duration of the syscall — build it from a local `let mut fd = fd;` inside the wrapper; the raw fd number is what mio registers.
- `Ready` has **no public constructor**: tests that need a specific readiness forge it through `Event::new(token, key, ready)` with a real `Ready` or through the public `schedule_readiness(key, Readiness)`.
- Signal tests must NEVER raise SIGTERM/SIGKILL/SIGSTOP to self; install a `Handle` disposition for SIGUSR1/SIGUSR2/SIGHUP and restore it.
  `SignalFacility` test raises are thread-directed — the facility is process-global, so serialize tests on a shared `Mutex`, raise to the process with `kill(getpid(), …)` or, where deterministic delivery is needed, use `pthread_kill(pthread_self(), …)` (POSIX delivers a target thread's pending signal before it returns).

## See Also
- `../CONTEXT.md` → crate-layer contracts, the seam types, the Stage-0…6 mapping (§7), wave-5 position.
- `../../CONTEXT.md` → workspace contracts §1 A.4 (driver C ABI) / A.6 (CLI/config), §3 (error strategy).
- `../../ERTS_RUST_REWRITE_DEEPDIVE.md` §A (the two clean seams), §H (Stage 5).
- `src/drivers/inet/CONTEXT.md` → the `tcp_inet` driver + read-path framing (child node).
- `src/abi/CONTEXT.md` → the driver C-ABI host layer (child node).
- `../beam_process/CONTEXT.md` → `PortTable`/`PortId`, `Scheduler`/`ProcessTable`, the scheduler that drives ports.
- Owed upstream (`../../CHANGE.md`, repo-root owned): the port-task model/`PortNotification`/`port_command`+`port_control`, the `line`/phase/`efile` drivers, the poller regular-file fallback + `FdDriver` EOF de-registration + `EINTR`-tolerant `poll`, the spawn driver (`StreamOptions`, the failed-spawn split + deferred `fork`/`execve` child, the duplex `{fd,N}` form), the bounded `driver_async` pool + round-driven completion + exactly-once release + `KeepAlive`, the sticky `ERL_DRV_USE`/`stop_select` callback, the defaulted driver seams (`take_exit_status`/`take_eof`/`has_pending_async`/`next_timeout`/`port_exit` + `PortExitReason`), the `outputv` write-path preference, `signal`, `cpu_time`, `ddll`.
