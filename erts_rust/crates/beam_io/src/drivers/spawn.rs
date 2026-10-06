//! [`SpawnDriver`] — the OS-child `spawn` driver — plus the term-free [`SpawnSpec`] it is
//! built from.
//!
//! This is the Rust counterpart of the legacy C `spawn` driver
//! (`erts/emulator/drivers/unix/spawn_driver.c`): it starts an OS process for a port, feeds
//! the child's **stdlib** in as port input and drains the port's output queue into the
//! child's **stdin**, killing and reaping the child when the port closes.
//!
//! # The three `open_port/2` name forms
//!
//! [`SpawnSpec`] is the term-free description of the `Name` argument — the caller (the
//! runtime, which owns the Erlang term decoding) validates the terms and hands the driver a
//! plain Rust value:
//!
//! | [`SpawnSpec`] | `open_port/2` name | child |
//! |---|---|---|
//! | [`SpawnSpec::Spawn`] with `argv: None` | `{spawn, Cmd}` | `/bin/sh -c Cmd` |
//! | [`SpawnSpec::Spawn`] with `argv: Some(v)` | `{spawn, [Prog, Arg…]}` | `Prog Arg…` directly |
//! | [`SpawnSpec::SpawnExecutable`] | `{spawn_executable, Path}` | `Path` directly, `args` appended |
//! | [`SpawnSpec::Fd`] | `{fd, N}` | **no child**: descriptor `N` is adopted |
//!
//! [`SpawnOptions`] carries the honored `open_port/2` options: `binary`/`list`, `{cd, Dir}`,
//! `{env, [{K, V}]}`, `{args, [String]}` and `exit_status`. [`build_command`] is the single
//! place the argv vector, the shell-vs-direct choice, `cd`/`env` and the three piped
//! standard streams live; it is unit-testable without starting anything.
//!
//! # Lifecycle: the child exists before the port does
//!
//! [`SpawnDriver::new`] really starts the process (or adopts `{fd, N}`) and returns a
//! `Result`, so the runtime can read [`SpawnDriver::os_pid`] **before** handing the driver
//! to [`crate::PortRegistry::open`]; a failed spawn is a constructor error that registers
//! nothing. [`Driver::start`] only registers the already-created descriptors with the
//! poller. [`Driver::stop`] and the `Drop` impl both kill and reap the child (a child that
//! already exited is reaped through `try_wait` and needs no kill), deregister every fd they
//! registered and close the fds they own. Nothing else may close those fds.
//!
//! # Readiness model
//!
//! The poller's backend is **edge-triggered** (`mio` registers descriptors with `EPOLLET`),
//! so every callback drains to `EAGAIN`: [`Driver::ready_input`] reads the child's stdout
//! until the fd is empty or reports EOF, and the write path loops until the output queue is
//! drained or the kernel buffer is full. The child's stdin is put in non-blocking mode when
//! it is created, so a write can never stall the reactor.
//!
//! A short write (`EAGAIN`) leaves the remainder queued and **arms** write readiness by
//! (re-)registering the stdin fd; a fully drained queue drops the registration again so the
//! port cannot spin. `read(2) == 0` is end-of-file: EOF is latched, the descriptor is
//! deregistered and the driver stops reading it (further readiness is a no-op rather than an
//! error). A write failure (a child that closed its stdin, hence `EPIPE`) is reported as the
//! typed [`beam_common::BeamError::Io`] rather than silently discarding queued bytes.
//!
//! # Stream and descriptor ownership
//!
//! The driver hands the child's stdout bytes to the port input queue **unchanged**. The
//! `binary`/`list` distinction ([`SpawnStream`], default `list`) is the *runtime's* when it
//! turns those bytes into a `{Port, {data, Data}}` message — as in the emulator, where the
//! stream type is the port's, not the driver's. Line framing is therefore absent here (the
//! built-in [`crate::drivers::FdDriver`] carries it for the `{line, N}` option).
//!
//! The descriptors of a spawned child (`stdin`/`stdout`) belong to the driver and are closed
//! on EOF/`stop`/`Drop`. The descriptor of a `{fd, N}` port belongs to the **Erlang side**:
//! the driver deregisters it but never closes it, exactly as the reference implementation
//! leaves an adopted descriptor alone. The child's stderr is piped (so the child never
//! inherits the emulator's own captured output) but is never read.
//!
//! # Registration and the driver registry
//!
//! A [`crate::DriverRegistry`] factory is a *no-argument* function, so it cannot carry a
//! [`SpawnSpec`]. The spawn driver is therefore **constructed directly**
//! ([`SpawnDriver::new`]) and handed to [`crate::PortRegistry::open`]; it is deliberately not
//! registered as a built-in factory under a name it could never honour. Its
//! [`Driver::name`] is `"spawn"`.
//!
//! # Exit status
//!
//! A port opened with the `exit_status` option can have its child's reaped status taken
//! **once** through [`Driver::take_exit_status`] (surfaced as
//! [`crate::PortRegistry::take_exit_status`]). Reaping is repeatable — `Child::try_wait`
//! caches the status it reaped — so the later `stop`/`Drop` teardown still sees it.
//!
//! # Relation to [`crate::drivers::FdDriver`]
//!
//! The byte-movement loops deliberately mirror `FdDriver`'s rather than sharing them: a
//! spawn port has two independently-owned descriptors with a child lifecycle attached,
//! deregisters (and closes) its read fd at EOF, and applies no input framing — the `FdDriver`
//! state machine would have to be split open to express that. The `fcntl` non-blocking
//! wrapper *is* shared (see [`crate::drivers::fd`]).
//!
//! All raw syscalls (`read`/`write`/`close`) live here; every `unsafe` block carries a
//! `// SAFETY:` rationale.

use std::os::unix::io::{IntoRawFd, RawFd};
use std::process::{Child, Command, Stdio};

use beam_common::Result;

use crate::driver::{Driver, DriverContext};
use crate::drivers::fd::set_nonblocking;
use crate::error;
use crate::poller::{Interest, Token};

/// The read buffer size of a single [`Driver::ready_input`] syscall.
const READ_CHUNK: usize = 64 * 1024;

/// The stream type of a spawned port — the `binary`/`list` option of `open_port/2`.
///
/// The driver never framed its input by this value (see the module docs); the runtime uses
/// it to build the `{Port, {data, Data}}` message, where [`SpawnStream::Binary`] yields an
/// Erlang binary and [`SpawnStream::List`] a char list.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SpawnStream {
    /// `binary`: the runtime delivers `{Port, {data, Bin}}`.
    Binary,
    /// `list`: the runtime delivers `{Port, {data, Str}}` — the `erl` default.
    #[default]
    List,
}

impl SpawnStream {
    /// The atom name `port_info(Port, stream)` answers with (`"binary"` / `"list"`).
    pub fn atom_name(&self) -> &'static str {
        match self {
            SpawnStream::Binary => "binary",
            SpawnStream::List => "list",
        }
    }
}

/// The validated `Name` of an `open_port/2` call — which child (if any) backs the port.
///
/// Decoding the Erlang term is the runtime's job (`{spawn, Cmd}`, `{spawn_executable, Path}`
/// and `{fd, N}` are the accepted forms); this type is the term-free result of that
/// validation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SpawnSpec {
    /// `{spawn, Cmd}`: a shell command line run through `/bin/sh -c`, or — when `argv` is
    /// `Some` — an argv vector run directly.
    Spawn {
        /// The command line to run, or its display form when `argv` is given (the argv
        /// join, exactly as the runtime renders it).
        command: String,
        /// The argv vector to run directly: `argv[0]` is the program and the rest are its
        /// arguments. `None` runs `command` through the shell.
        argv: Option<Vec<String>>,
    },
    /// `{spawn_executable, Path}`: run `Path` directly; [`SpawnOptions::args`] supplies its
    /// arguments.
    SpawnExecutable {
        /// The absolute or relative path of the program to run.
        path: String,
    },
    /// `{fd, N}`: adopt descriptor `N` as the port's read fd; no child is started.
    Fd {
        /// The descriptor to adopt (must be non-negative).
        fd: RawFd,
    },
}

impl SpawnSpec {
    /// The port "name" string — the `port_info(Port, name)` value.
    ///
    /// For [`SpawnSpec::Fd`] it is `fd:<N>`, the same rendering the reference
    /// implementation uses.
    pub fn command(&self) -> String {
        match self {
            SpawnSpec::Spawn { command, .. } => command.clone(),
            SpawnSpec::SpawnExecutable { path } => path.clone(),
            SpawnSpec::Fd { fd } => format!("fd:{fd}"),
        }
    }

    /// Whether this spec starts a child (everything but [`SpawnSpec::Fd`]).
    pub fn spawns_a_child(&self) -> bool {
        !matches!(self, SpawnSpec::Fd { .. })
    }
}

/// The validated options of an `open_port/2` call.
///
/// The fields are the honored options; an option the runtime recognizes but that has no
/// effect here (the `eof` option) is simply not represented, and an unknown option is
/// tolerated by the runtime's validation rather than rejected.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SpawnOptions {
    /// The `binary`/`list` stream choice (default [`SpawnStream::List`]).
    pub stream: SpawnStream,
    /// The `{cd, Dir}` working directory.
    pub cd: Option<String>,
    /// The `{env, [{Name, Value}]}` environment additions.
    pub env: Vec<(String, String)>,
    /// The `{args, [String]}` arguments of a [`SpawnSpec::SpawnExecutable`] port (ignored by
    /// the `{spawn, [Prog, Arg…]}` argv form, which carries its own).
    pub args: Vec<String>,
    /// The `exit_status` flag: the runtime may take the child's reaped exit status once
    /// (see [`Driver::take_exit_status`]).
    pub exit_status: bool,
}

/// Builds the [`Command`] a spec/options pair describes, or `None` for the `{fd, N}` form.
///
/// This is the single place the argv vector, the shell-vs-direct choice, `{cd, Dir}`,
/// `{env, …}` and the three **piped** standard streams are set up; the caller decides
/// whether to start it (the driver's constructor does). It starts nothing itself, so it is
/// directly unit-testable.
///
/// The child's `stdin`, `stdout` and `stderr` are all pipes: `stdout` is the port's input
/// stream, `stdin` is what [`crate::PortRegistry::port_command`] writes to, and stderr is
/// held open (never read) so the child cannot inherit the emulator's own output.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for an argv vector that names no program (an empty
/// `{spawn, []}`).
pub fn build_command(spec: &SpawnSpec, options: &SpawnOptions) -> Result<Option<Command>> {
    let mut command = match spec {
        // `{fd, N}` starts nothing: the descriptor is the port's whole I/O stream.
        SpawnSpec::Fd { .. } => return Ok(None),
        SpawnSpec::Spawn { command, argv: None } => {
            let mut shell = Command::new("/bin/sh");
            shell.arg("-c").arg(command);
            shell
        }
        SpawnSpec::Spawn {
            argv: Some(argv), ..
        } => program_command(argv)?,
        // `{spawn_executable, Path}` builds its argv from the `args` option.
        SpawnSpec::SpawnExecutable { path } => {
            let mut argv = Vec::with_capacity(1 + options.args.len());
            argv.push(path.clone());
            argv.extend(options.args.iter().cloned());
            program_command(&argv)?
        }
    };
    if let Some(dir) = &options.cd {
        command.current_dir(dir);
    }
    for (key, value) in &options.env {
        command.env(key, value);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(Some(command))
}

/// The [`Command`] for an argv vector: `argv[0]` is the program, the rest are its arguments.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] when the vector is empty (there is no program to run).
fn program_command(argv: &[String]) -> Result<Command> {
    let program = argv.first().ok_or_else(error::badarg)?;
    let mut command = Command::new(program);
    command.args(&argv[1..]);
    Ok(command)
}

/// The `spawn` driver: an OS child process (or an adopted descriptor) behind a port.
///
/// Constructed directly from a [`SpawnSpec`]/[`SpawnOptions`] pair (see the module docs) and
/// handed to [`crate::PortRegistry::open`], which runs [`Driver::start`] and thereby
/// registers the child's stdout with the poller.
///
/// The driver is single-threaded like the port that owns it: it holds a
/// [`std::process::Child`] and raw descriptors, and it takes no locks.
pub struct SpawnDriver {
    /// The port "name" string (the shell command line, the argv join, or `fd:<N>`).
    command: String,
    /// The child process; `None` for a `{fd, N}` port.
    child: Option<Child>,
    /// The child's write end (the port's output stream), once created and put in
    /// non-blocking mode.
    stdin_fd: Option<RawFd>,
    /// The child's read end (the port's input stream); `None` after EOF, or for a `{fd, N}`
    /// port once it reported EOF.
    stdout_fd: Option<RawFd>,
    /// The poller token of the stdout registration, once started.
    in_token: Option<Token>,
    /// The poller token of the stdin registration, once write readiness is armed.
    out_token: Option<Token>,
    /// Set once the input stream reported end-of-file, so no further reads are attempted.
    read_eof: bool,
    /// The `binary`/`list` option (the runtime's input framing choice).
    stream: SpawnStream,
    /// Whether the `exit_status` option asked for the child's status.
    exit_status: bool,
    /// Whether that status has already been handed out, so it is reported exactly once.
    exit_reported: bool,
    /// Whether the driver owns (and must close) the descriptors. False for an adopted
    /// `{fd, N}` descriptor, which belongs to the Erlang side.
    own_fds: bool,
    /// The OS process id; `None` for a `{fd, N}` port.
    os_pid: Option<u32>,
}

impl SpawnDriver {
    /// Starts the child `spec`/`options` describe — or adopts `{fd, N}` — and returns the
    /// driver.
    ///
    /// This **really** spawns (the poller is not touched until [`Driver::start`]), so a
    /// failure is reported here, before anything is registered anywhere.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] when the process cannot be started (an unknown program,
    /// a bad `{cd, …}`, …), or when putting a descriptor in non-blocking mode fails;
    /// [`beam_common::BeamError::BadArg`] for an `{fd, N}` spec with a negative descriptor or
    /// for an empty argv vector.
    pub fn new(spec: &SpawnSpec, options: &SpawnOptions) -> Result<SpawnDriver> {
        let command = spec.command();
        if let SpawnSpec::Fd { fd } = spec {
            if *fd < 0 {
                return Err(error::badarg());
            }
            set_nonblocking(*fd)?;
            return Ok(SpawnDriver {
                command,
                child: None,
                stdin_fd: None,
                stdout_fd: Some(*fd),
                in_token: None,
                out_token: None,
                read_eof: false,
                stream: options.stream,
                exit_status: options.exit_status,
                exit_reported: false,
                // The descriptor belongs to the Erlang side: deregistered, never closed.
                own_fds: false,
                os_pid: None,
            });
        }
        // `build_command` answers `None` only for the `{fd, N}` form, handled above.
        let Some(mut built) = build_command(spec, options)? else {
            return Err(error::badarg());
        };
        let mut child = built.spawn().map_err(error::io)?;
        let os_pid = child.id();
        // Take both ends out of the child so the driver owns them as plain descriptors (the
        // same reason `FdDriver` owns its fds: the poller and the syscall wrappers work on
        // raw descriptors). stderr stays inside the `Child`, held open but unread.
        let stdin_fd = child.stdin.take().map(IntoRawFd::into_raw_fd);
        let stdout_fd = child.stdout.take().map(IntoRawFd::into_raw_fd);
        // Non-blocking must be in place before either descriptor reaches the poller.
        let prepared = [stdin_fd, stdout_fd]
            .into_iter()
            .flatten()
            .try_for_each(set_nonblocking);
        if let Err(err) = prepared {
            // The child is alive but unusable: reap it, then release the descriptors taken
            // from it. The teardown outcome is deliberately discarded — the failure the
            // caller must observe is the one being returned, and the cleanup cannot fail in
            // a way the caller could act on.
            for fd in [stdin_fd, stdout_fd].into_iter().flatten() {
                close_fd(fd);
            }
            let _reap = kill_and_reap(&mut child);
            return Err(err);
        }
        Ok(SpawnDriver {
            command,
            child: Some(child),
            stdin_fd,
            stdout_fd,
            in_token: None,
            out_token: None,
            read_eof: false,
            stream: options.stream,
            exit_status: options.exit_status,
            exit_reported: false,
            own_fds: true,
            os_pid: Some(os_pid),
        })
    }

    /// The OS process id of the child, or `None` for a `{fd, N}` port.
    pub fn os_pid(&self) -> Option<u32> {
        self.os_pid
    }

    /// The port "name" string: the shell command line, the argv join, or `fd:<N>`.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// The `binary`/`list` stream option of the port.
    pub fn stream(&self) -> SpawnStream {
        self.stream
    }

    /// Whether the child's output stream reported end-of-file.
    pub fn eof(&self) -> bool {
        self.read_eof
    }

    /// The reaped exit status of the child, when it has exited.
    ///
    /// `Ok(None)` means the child is still running, or there is no child at all (a
    /// `{fd, N}` port); a child that was **killed by a signal** reports `128 + signal`, the
    /// shell convention (and what the legacy driver's status carries).
    ///
    /// Unlike [`Driver::take_exit_status`] this is a plain liveness probe: it neither honors
    /// the `exit_status` option nor reports a status only once, and the reaped status stays
    /// cached in the child for later calls.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] when the wait itself fails.
    pub fn exited(&mut self) -> Result<Option<i32>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait().map_err(error::io)?;
        Ok(status.map(exit_status_code))
    }

    /// Takes the child's reaped exit status **once**, when the `exit_status` option asked for
    /// it.
    ///
    /// `Ok(None)` when the option was not asked for, when the status was already taken, or
    /// when the child has not exited yet.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] when the wait itself fails.
    pub fn take_exit_status(&mut self) -> Result<Option<i32>> {
        if !self.exit_status || self.exit_reported {
            return Ok(None);
        }
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        // `try_wait` caches the status it reaped, so a later teardown `try_wait` still
        // answers `Some` and needs no kill.
        let status = child.try_wait().map_err(error::io)?;
        match status {
            Some(status) => {
                self.exit_reported = true;
                Ok(Some(exit_status_code(status)))
            }
            None => Ok(None),
        }
    }

    /// Arms write readiness for `fd` — registering it the first time, re-registering it
    /// after that — so [`Driver::ready_output`] runs once the kernel buffer drains.
    fn arm_output(&mut self, ctx: &mut DriverContext<'_>, fd: RawFd) -> Result<()> {
        match self.out_token {
            Some(token) => ctx.reregister(fd, token, Interest::WRITABLE),
            None => {
                self.out_token = Some(ctx.register(fd, Interest::WRITABLE)?);
                Ok(())
            }
        }
    }

    /// Drops the write registration, so no further writable events arrive.
    fn disarm_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if let Some(token) = self.out_token.take() {
            if let Some(fd) = self.stdin_fd {
                ctx.deregister(fd, token)?;
            }
        }
        Ok(())
    }

    /// Deregisters the child's stdout — and, for a child the driver owns, closes it — after
    /// end-of-file.
    ///
    /// Deregistering is what keeps the reactor from reporting a descriptor that can never
    /// become ready again; an adopted `{fd, N}` descriptor is left open (the Erlang side
    /// owns it).
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] when the poller rejects the deregistration.
    fn release_stdout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let (Some(token), Some(fd)) = (self.in_token.take(), self.stdout_fd.take()) else {
            return Ok(());
        };
        let result = ctx.deregister(fd, token);
        if self.own_fds {
            close_fd(fd);
        }
        result
    }

    /// Drains the port's output queue into the child's stdin, returning the number of bytes
    /// written.
    ///
    /// A short write leaves the remainder queued and arms write readiness; a fully drained
    /// queue drops the write registration again.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] for a `{fd, N}` port (it owns no child stream);
    /// [`beam_common::BeamError::Io`] when the write fails (a child that closed its stdin
    /// yields `EPIPE` — a typed failure rather than silently discarded bytes).
    fn write_pending(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        let fd = self.stdin_fd.ok_or_else(error::badarg)?;
        let mut written = 0usize;
        let mut stalled = false;
        loop {
            let front = ctx.output().front();
            if front.is_empty() {
                break;
            }
            let len = front.len();
            // SAFETY: `fd` is an open, non-blocking descriptor owned by this driver (the
            // child's stdin) and `front` points to `len` initialized bytes — `write(2)`
            // reads at most that many and never retains the pointer.
            let n = unsafe { libc::write(fd, front.as_ptr().cast(), len) };
            if n > 0 {
                let n = n as usize;
                ctx.output().consume(n);
                written += n;
                continue;
            }
            if n == 0 {
                // No progress: treat it like a full buffer and let readiness re-drive us
                // instead of spinning here.
                stalled = true;
                break;
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                // A signal interrupted the write; retry.
                Some(libc::EINTR) => continue,
                // The kernel buffer is full (`EWOULDBLOCK` shares `EAGAIN`'s value on every
                // platform this crate targets).
                Some(libc::EAGAIN) => {
                    stalled = true;
                    break;
                }
                _ => return Err(error::io(err)),
            }
        }
        if ctx.output().is_empty() {
            self.disarm_output(ctx)?;
        } else if stalled {
            self.arm_output(ctx, fd)?;
        }
        Ok(written)
    }

    /// Closes every descriptor this driver owns.
    ///
    /// Each field is `take()`n first, so this is idempotent: `stop`, `release_stdout` and
    /// `Drop` can never close the same descriptor twice. An adopted `{fd, N}` descriptor is
    /// left open.
    fn close_fds(&mut self) {
        let stdout = self.stdout_fd.take();
        let stdin = self.stdin_fd.take();
        if !self.own_fds {
            return;
        }
        for fd in [stdout, stdin].into_iter().flatten() {
            close_fd(fd);
        }
    }

    /// Kills and reaps the child, if one is still held.
    ///
    /// A child that already exited is reaped by `try_wait` and needs no kill, so the one
    /// case where `kill` legitimately fails is never mistaken for a genuine failure. The
    /// wait runs whatever the kill outcome: a child that has died is a zombie until it is
    /// waited on, and a leaked zombie is worse than a secondary error.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] when the kill or the wait fails.
    fn reap_child(&mut self) -> Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        kill_and_reap(&mut child)
    }
}

impl Driver for SpawnDriver {
    fn name(&self) -> &str {
        "spawn"
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // Only the already-created descriptors are registered here; the child was started by
        // the constructor, so nothing can fail midway through a spawn.
        if let Some(fd) = self.stdout_fd {
            self.in_token = Some(ctx.register(fd, Interest::READABLE)?);
        }
        if let Some(fd) = self.stdin_fd {
            // Only arm the child's stdin when there is already something to flush; otherwise
            // `output` writes synchronously and write interest is never needed.
            if !ctx.output().is_empty() {
                self.out_token = Some(ctx.register(fd, Interest::WRITABLE)?);
            }
        }
        Ok(())
    }

    fn output(&mut self, ctx: &mut DriverContext<'_>) -> Result<usize> {
        self.write_pending(ctx)
    }

    fn ready_input(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if self.read_eof {
            // End of file was already observed: further read readiness is a no-op rather
            // than an error.
            return Ok(());
        }
        let fd = self.stdout_fd.ok_or_else(error::badarg)?;
        let mut buf = [0u8; READ_CHUNK];
        loop {
            // SAFETY: `fd` is an open, non-blocking descriptor owned by this driver (the
            // child's stdout) and `buf` is a writable array of `READ_CHUNK` bytes, so
            // `read(2)` writes at most `READ_CHUNK` bytes into it.
            let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), READ_CHUNK) };
            if n > 0 {
                ctx.input().push(&buf[..n as usize]);
                // Edge-triggered readiness: keep reading until the fd is empty.
                continue;
            }
            if n == 0 {
                // End of file: the child closed its stream. Latch it, then deregister (and,
                // for an owned descriptor, release) it so the reactor cannot report a
                // descriptor that will never become ready again.
                self.read_eof = true;
                return self.release_stdout(ctx);
            }
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => return Ok(()),
                _ => return Err(error::io(err)),
            }
        }
    }

    fn stop(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        let in_result = match (self.in_token.take(), self.stdout_fd) {
            (Some(token), Some(fd)) => ctx.deregister(fd, token),
            _ => Ok(()),
        };
        let out_result = match (self.out_token.take(), self.stdin_fd) {
            (Some(token), Some(fd)) => ctx.deregister(fd, token),
            _ => Ok(()),
        };
        // The child is killed and reaped first, so nothing it produces can reach the queues
        // after the port is gone; then the descriptors are released (taken here, so `Drop`
        // will not touch them again).
        let child_result = self.reap_child();
        self.close_fds();
        in_result.and(out_result).and(child_result)
    }

    fn take_exit_status(&mut self) -> Result<Option<i32>> {
        SpawnDriver::take_exit_status(self)
    }
}

impl Drop for SpawnDriver {
    fn drop(&mut self) {
        // `stop` already released the child and the descriptors on the normal teardown path;
        // this covers a driver dropped without ever being stopped (a `start` that failed
        // inside `PortRegistry::open`, for instance), so neither a running child nor a
        // descriptor leaks. A `Drop` cannot report anything to a caller, so both outcomes are
        // discarded — the same rule `FdDriver` follows for its descriptor closes.
        let _reap = self.reap_child();
        self.close_fds();
    }
}

/// Kills `child` when it is still running and reaps it.
///
/// # Errors
///
/// [`beam_common::BeamError::Io`] when the kill or the wait fails; the wait is attempted
/// either way so a dead child is never left as a zombie.
fn kill_and_reap(child: &mut Child) -> Result<()> {
    match child.try_wait().map_err(error::io)? {
        // Already reaped (or already exited): no kill is needed and none is attempted.
        Some(_status) => Ok(()),
        None => {
            let killed = child.kill();
            // Reap whatever the kill outcome: a child that has died is a zombie until it is
            // waited on.
            let waited = child.wait();
            match killed {
                Err(err) => Err(error::io(err)),
                Ok(()) => waited.map(|_status| ()).map_err(error::io),
            }
        }
    }
}

/// Closes `fd`, discarding the outcome.
///
/// There is no recovery from a failed `close(2)` and the only alternative — leaking the
/// descriptor — is worse, so the result is dropped deliberately. Every call site `take()`s
/// the field it closes, so no descriptor is closed twice.
fn close_fd(fd: RawFd) {
    // SAFETY: `fd` is a descriptor this driver created (a child's stdio end) or adopted
    // (`{fd, N}`), and it is closed exactly once by the ownership rules above.
    let _ = unsafe { libc::close(fd) };
}

/// The status a `{Port, {exit_status, Status}}` message reports for `status`.
///
/// A child that exited reports its own exit code; one that was **killed by a signal** reports
/// `128 + signal` — the shell convention, and what the legacy `spawn` driver's status
/// carries. The two are told apart through `ExitStatusExt` because `ExitStatus::code` is
/// `None` for a signal.
fn exit_status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::{build_command, SpawnDriver, SpawnOptions, SpawnSpec, SpawnStream};
    use crate::driver::{Driver, DriverContext, PortArms};
    use crate::poller::Poller;
    use crate::queue::ByteQueue;
    use beam_common::BeamError;
    use beam_process::PortId;
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// A `{spawn, Command}` spec (the shell form).
    fn shell(command: &str) -> SpawnSpec {
        SpawnSpec::Spawn {
            command: command.to_string(),
            argv: None,
        }
    }

    /// A `{spawn, [Prog, Arg…]}` spec (the direct argv form).
    fn argv(program: &str, args: &[&str]) -> SpawnSpec {
        let mut vector = vec![program.to_string()];
        vector.extend(args.iter().map(|arg| arg.to_string()));
        SpawnSpec::Spawn {
            command: vector.join(" "),
            argv: Some(vector),
        }
    }

    /// The argument vector of a built command, as strings.
    fn args_of(command: &std::process::Command) -> Vec<String> {
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn the_shell_form_runs_the_command_through_bin_sh() {
        let spec = shell("echo hi");
        let command = build_command(&spec, &SpawnOptions::default())
            .expect("a command")
            .expect("the shell form has one");
        assert_eq!(command.get_program(), OsStr::new("/bin/sh"));
        assert_eq!(args_of(&command), vec!["-c".to_string(), "echo hi".into()]);
        assert_eq!(spec.command(), "echo hi");
        assert!(spec.spawns_a_child());
    }

    #[test]
    fn the_argv_form_runs_the_program_directly() {
        let spec = argv("echo", &["hi", "there"]);
        let command = build_command(&spec, &SpawnOptions::default())
            .expect("a command")
            .expect("the argv form has one");
        assert_eq!(command.get_program(), OsStr::new("echo"));
        assert_eq!(
            args_of(&command),
            vec!["hi".to_string(), "there".to_string()]
        );
        assert_eq!(spec.command(), "echo hi there");
    }

    #[test]
    fn spawn_executable_prepends_its_path_to_the_option_args() {
        let spec = SpawnSpec::SpawnExecutable {
            path: "/bin/echo".to_string(),
        };
        let options = SpawnOptions {
            args: vec!["hi".to_string(), "there".to_string()],
            ..SpawnOptions::default()
        };
        let command = build_command(&spec, &options)
            .expect("a command")
            .expect("the executable form has one");
        assert_eq!(command.get_program(), OsStr::new("/bin/echo"));
        assert_eq!(
            args_of(&command),
            vec!["hi".to_string(), "there".to_string()]
        );
        assert_eq!(spec.command(), "/bin/echo");
    }

    #[test]
    fn cd_and_env_are_applied_to_the_command() {
        let options = SpawnOptions {
            cd: Some("/tmp".to_string()),
            env: vec![
                ("FOO".to_string(), "bar".to_string()),
                ("BAZ".to_string(), "qux".to_string()),
            ],
            ..SpawnOptions::default()
        };
        let command = build_command(&shell("true"), &options)
            .expect("a command")
            .expect("the shell form has one");
        assert_eq!(command.get_current_dir(), Some(Path::new("/tmp")));
        let env: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
        assert!(env.contains(&(OsStr::new("FOO"), Some(OsStr::new("bar")))));
        assert!(env.contains(&(OsStr::new("BAZ"), Some(OsStr::new("qux")))));
    }

    #[test]
    fn an_empty_argv_vector_names_no_program() {
        let spec = SpawnSpec::Spawn {
            command: String::new(),
            argv: Some(Vec::new()),
        };
        assert!(matches!(
            build_command(&spec, &SpawnOptions::default()),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn the_fd_form_has_no_command() {
        let spec = SpawnSpec::Fd { fd: 7 };
        assert!(build_command(&spec, &SpawnOptions::default())
            .expect("the fd form is accepted")
            .is_none());
        assert_eq!(spec.command(), "fd:7");
        assert!(!spec.spawns_a_child());
        assert!(matches!(
            SpawnDriver::new(&SpawnSpec::Fd { fd: -1 }, &SpawnOptions::default())
                .map(|_| ())
                .err(),
            Some(BeamError::BadArg)
        ));
    }

    #[test]
    fn the_stream_option_names_the_erlang_atoms() {
        assert_eq!(SpawnStream::default(), SpawnStream::List);
        assert_eq!(SpawnStream::List.atom_name(), "list");
        assert_eq!(SpawnStream::Binary.atom_name(), "binary");
    }

    #[test]
    fn an_unknown_program_is_a_constructor_error() {
        let spec = SpawnSpec::SpawnExecutable {
            path: "/nonexistent/xyz-not-real".to_string(),
        };
        assert!(matches!(
            SpawnDriver::new(&spec, &SpawnOptions::default()).map(|_| ()),
            Err(BeamError::Io(_))
        ));
    }

    #[test]
    fn the_driver_reports_the_child_it_started() {
        let mut driver = SpawnDriver::new(&shell("sleep 30"), &SpawnOptions::default())
            .expect("sleep starts");
        assert_eq!(driver.name(), "spawn");
        assert!(driver.os_pid().is_some());
        assert_eq!(driver.command(), "sleep 30");
        assert_eq!(driver.stream(), SpawnStream::List);
        assert!(!driver.eof());
        assert_eq!(
            driver.exited().expect("the wait succeeds"),
            None,
            "the child is still running"
        );
        // The `exit_status` option was not asked for, so nothing is ever reported.
        assert_eq!(
            driver.take_exit_status().expect("the wait succeeds"),
            None
        );
        // Dropping the driver kills and reaps the child (the `Drop` path).
        drop(driver);
    }

    #[test]
    fn the_binary_stream_option_is_carried() {
        let options = SpawnOptions {
            stream: SpawnStream::Binary,
            ..SpawnOptions::default()
        };
        let driver = SpawnDriver::new(&shell("sleep 30"), &options).expect("sleep starts");
        assert_eq!(driver.stream(), SpawnStream::Binary);
    }

    #[test]
    fn the_read_path_delivers_the_child_output_and_the_eof() {
        let mut driver =
            SpawnDriver::new(&shell("echo hi"), &SpawnOptions::default()).expect("echo starts");
        let mut poller = Poller::new().expect("a poll set");
        let mut input = ByteQueue::new();
        let mut output = ByteQueue::new();
        let mut arms = PortArms::default();
        {
            let mut ctx = DriverContext::new(PortId::new(1), &mut poller, &mut input, &mut output, &mut arms);
            driver.start(&mut ctx).expect("the stdout registration");
        }
        assert_eq!(poller.len(), 1, "only stdout is registered by `start`");

        // Drive the real readiness loop until the child's EOF is latched (bounded, so a
        // broken driver fails the test instead of hanging it).
        let deadline = Instant::now() + Duration::from_secs(5);
        while !driver.eof() && Instant::now() < deadline {
            let events = poller.poll(Some(Duration::from_millis(100))).expect("poll");
            if events
                .iter()
                .any(|event| event.ready().readable() || event.ready().is_read_closed())
            {
                let mut ctx =
                    DriverContext::new(PortId::new(1), &mut poller, &mut input, &mut output, &mut arms);
                driver.ready_input(&mut ctx).expect("the read");
            }
        }
        assert!(driver.eof(), "the child's end-of-file is latched");
        assert_eq!(input.front(), b"hi\n");
        assert!(
            poller.is_empty(),
            "EOF deregisters the descriptor that can never become ready again"
        );
        // A read after EOF is a no-op, not an error.
        let mut ctx = DriverContext::new(PortId::new(1), &mut poller, &mut input, &mut output, &mut arms);
        driver
            .ready_input(&mut ctx)
            .expect("a post-EOF ready_input is not an error");
    }

    #[test]
    fn the_exit_status_is_reported_once_when_the_option_asked_for_it() {
        let options = SpawnOptions {
            exit_status: true,
            ..SpawnOptions::default()
        };
        let mut driver = SpawnDriver::new(&shell("exit 7"), &options).expect("the shell starts");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut status = None;
        while status.is_none() && Instant::now() < deadline {
            status = driver.take_exit_status().expect("the wait succeeds");
            if status.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(status, Some(7));
        assert_eq!(
            driver.take_exit_status().expect("the wait succeeds"),
            None,
            "a status is handed out exactly once"
        );
        assert_eq!(driver.exited().expect("the wait succeeds"), Some(7));
    }

    #[test]
    fn a_killed_child_reports_the_signal_convention() {
        // `kill -9 $$` kills the shell itself, so the status is a signal, not an exit code.
        let options = SpawnOptions {
            exit_status: true,
            ..SpawnOptions::default()
        };
        let mut driver =
            SpawnDriver::new(&shell("kill -9 $$"), &options).expect("the shell starts");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut status = None;
        while status.is_none() && Instant::now() < deadline {
            status = driver.take_exit_status().expect("the wait succeeds");
            if status.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        assert_eq!(status, Some(128 + 9));
    }

    #[test]
    fn killing_and_reaping_is_idempotent() {
        let mut driver = SpawnDriver::new(&shell("sleep 30"), &SpawnOptions::default())
            .expect("sleep starts");
        driver.reap_child().expect("the first reap succeeds");
        driver.reap_child().expect("a second reap is a no-op");
        assert_eq!(driver.exited().expect("the wait succeeds"), None);
    }
}
