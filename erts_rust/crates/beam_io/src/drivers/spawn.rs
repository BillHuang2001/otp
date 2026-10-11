// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

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
//! place the argv vector, the shell-vs-direct choice, `cd`/`env` and the three standard streams
//! live; it is unit-testable without starting anything. The two options that shape the child's
//! *streams* rather than its process description — `stderr_to_stdout` and `eof` — travel in
//! their own [`StreamOptions`] value (see below), which
//! [`SpawnDriver::with_stream_options`] accepts; [`SpawnDriver::new`] uses the defaults.
//!
//! # Lifecycle: the child exists before the port does
//!
//! [`SpawnDriver::new`] really starts the process (or adopts `{fd, N}`) and returns a
//! `Result`, so the runtime can read [`SpawnDriver::os_pid`] **before** handing the driver
//! to [`crate::PortRegistry::open`]; a failed spawn is a constructor error that registers
//! nothing and carries the **errno-derived atom** the oracle raises (`error:enoent`,
//! `error:eacces`, … — see `error::spawn_failure`). [`Driver::start`] only
//! registers the already-created descriptors with the poller. [`Driver::stop`] and the
//! `Drop` impl both kill and reap the child (a child that already exited is reaped through
//! `try_wait` and needs no kill), deregister every fd they registered and close the fds
//! they own. Nothing else may close those fds.
//!
//! The reference splits a failed spawn in two, and so does this driver. `spawn_start` checks
//! `access(Path, X_OK)` **in the emulator** (`sys_drivers.c:521`): a path that check cannot
//! reach — a missing program, a file without its execute bit, a path through a non-directory —
//! is an `open_port/2` **error** carrying the failing `errno`'s atom. A path the check
//! *accepts* but the `exec` rejects (a searchable **directory**, a file with no recognizable
//! executable format) instead **opens the port**: the `exec` happens in the forked child,
//! which exits with the failing `errno` (`erl_child_setup.c:361-369`). So
//! `{spawn_executable, "/etc"}` is a real port whose child reports `{exit_status, 13}`, while
//! `{spawn_executable, "/etc/hosts"}` (no execute bit) is `error:eacces`. Because the standard
//! library's `Command::spawn` reports a failing `exec` synchronously, those deferred cases are
//! started by [`fork_exec_child`]'s own `fork`/`execve` — a real child that reproduces the
//! reference exactly, never a stand-in.
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
//! port cannot spin. A `{fd, N}` port is the exception: its one descriptor is registered with
//! `Interest::BOTH`, so write readiness is permanently armed and neither arming nor disarming
//! touches its registration. `read(2) == 0` is end-of-file: EOF is latched, the descriptor is
//! deregistered and the driver stops reading it (further readiness is a no-op rather than an
//! error; for `{fd, N}` the shared registration stays in place because it still serves the
//! write direction). A write failure (a child that closed its stdin, hence `EPIPE`) is
//! reported as the typed [`beam_common::BeamError::Io`] rather than silently discarding
//! queued bytes.
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
//! on EOF/`stop`/`Drop`. The child's **stderr is inherited** by default (it appears on the
//! emulator's own stderr and is *not* port data — see the stream options below).
//!
//! # The stream options ([`StreamOptions`])
//!
//! Two `open_port/2` options shape the child's *streams* rather than its process description,
//! so they live in their own [`StreamOptions`] value instead of [`SpawnOptions`]. Both default
//! to `false`, reproducing the oracle:
//!
//! * **`stderr_to_stdout`** — the child's descriptor 2 becomes an **exact duplicate of its
//!   descriptor 1**, so its stderr bytes are delivered as port data and interleave with its
//!   stdout in true write order. The reference does exactly that
//!   (`stderrfd = opts->redir_stderr ? ifd[1] : dup(2);`, `spawn_start`,
//!   `erts/emulator/sys/unix/sys_drivers.c:500`): it points the child's stderr at the very
//!   pipe its stdout uses. **Without** the option the child's stderr is **inherited** from the
//!   emulator (`dup(2)`), so it is neither delivered to the port nor discarded — it appears on
//!   the emulator's own stderr.
//! * **`eof`** — the port is told when the child's stdout reaches end of file
//!   ([`Driver::take_eof`], surfaced as [`crate::PortRegistry::take_eof`]), which the runtime
//!   turns into a bare `eof` message; without it the end of the stream is not reported.
//!
//! `stderr_to_stdout` is applied by a child-side `dup2(1, 2)` in a `pre_exec` hook, so no
//! second pipe is created and byte ordering is exact; `eof` only gates the reporting of the
//! `read(2) == 0` the driver latches anyway.
//!
//! The descriptor of a `{fd, N}` port belongs to the **Erlang side** and is *bidirectional*:
//! the single adopted descriptor is registered **once** with [`Interest::BOTH`], so it is
//! both the port's read stream and its write stream ([`crate::PortRegistry::port_command`]
//! bytes go out through it). The driver deregisters it but never closes it, exactly as the
//! reference implementation leaves an adopted descriptor alone.
//!
//! # Registration and the driver registry
//!
//! A [`crate::DriverRegistry`] factory is a *no-argument* function, so it cannot carry a
//! [`SpawnSpec`]. The spawn driver is therefore **constructed directly**
//! ([`SpawnDriver::new`]) and handed to [`crate::PortRegistry::open`]; it is deliberately not
//! registered as a built-in factory under a name it could never honour. Its
//! [`Driver::name`] is `"spawn"`.
//!
//! # Exit status and end of file
//!
//! A port opened with the `exit_status` option can have its child's reaped status taken
//! **once** through [`Driver::take_exit_status`] (surfaced as
//! [`crate::PortRegistry::take_exit_status`]). Reaping is repeatable — `Child::try_wait`
//! caches the status it reaped — so the later `stop`/`Drop` teardown still sees it.
//!
//! A port opened with the `eof` option can have its input stream's end taken **once** through
//! [`Driver::take_eof`] (surfaced as [`crate::PortRegistry::take_eof`]); the driver latches the
//! `read(2) == 0` end-of-file regardless, so the option only decides whether it is reported.
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

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::{IntoRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};

use beam_common::{BeamError, Result};

use crate::driver::{Driver, DriverContext};
use crate::drivers::fd::set_nonblocking;
use crate::error;
use crate::poller::{Interest, Token};

/// The read buffer size of a single [`Driver::ready_input`] syscall.
const READ_CHUNK: usize = 64 * 1024;

/// The two `open_port/2` options that shape a spawned child's **streams** rather than its
/// process description (see the module docs), kept out of [`SpawnOptions`] so that struct stays
/// the argv/environment description the runtime already builds.
///
/// [`Default`] is both options off, which is the oracle's behaviour for an `open_port/2` call
/// that does not name them.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct StreamOptions {
    /// The `stderr_to_stdout` option: the child's stderr is delivered as port data, merged with
    /// its stdout in true write order (the child's descriptor 2 is made an exact duplicate of
    /// its descriptor 1). Default: the child's stderr is **inherited** from the emulator.
    pub stderr_to_stdout: bool,
    /// The `eof` option: the port is told once when the child's stdout reaches end of file
    /// ([`Driver::take_eof`]). Default: the end of the stream is not reported.
    pub eof: bool,
}

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
/// `{env, …}` and the three standard streams are set up; the caller decides whether to start
/// it (the driver's constructor does). It starts nothing itself, so it is directly
/// unit-testable.
///
/// Every stream option is left at its default, i.e. it behaves exactly like
/// `build_command_with_stream` with [`StreamOptions::default`]: the child's `stdout` is a
/// pipe (the port's input stream), its `stdin` is a pipe (what
/// [`crate::PortRegistry::port_command`] writes to) and its **stderr is inherited**
/// ([`Stdio::inherit`]) — the oracle's `dup(2)` (`sys_drivers.c:500`) — so a child's
/// diagnostics reach the emulator's stderr and are never port data. The
/// `stderr_to_stdout`/`eof` stream options are [`SpawnDriver::with_stream_options`]'s.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for an argv vector that names no program (an empty
/// `{spawn, []}`), and for the **empty** command/path the oracle rejects
/// (`open_port({spawn,""},[])` and `open_port({spawn_executable,""},[])` are both
/// `error:badarg`, verified against erts-17.1, while `{spawn," "}` is accepted).
pub fn build_command(spec: &SpawnSpec, options: &SpawnOptions) -> Result<Option<Command>> {
    build_command_with_stream(spec, options, StreamOptions::default())
}

/// [`build_command`] with the child-stream options [`StreamOptions`] describes.
///
/// The process description ([`SpawnSpec`]/[`SpawnOptions`]) is handled exactly as
/// `build_command` documents; the stream options add the child-side `dup2(1, 2)` of
/// `stderr_to_stdout` (an **exact** merge into the stdout pipe, never a second pipe). `eof`
/// needs nothing here — it only gates the *reporting* of the end-of-file the driver latches —
/// so it is not consulted.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] exactly as `build_command` documents.
fn build_command_with_stream(
    spec: &SpawnSpec,
    options: &SpawnOptions,
    stream_options: StreamOptions,
) -> Result<Option<Command>> {
    let mut command = match spec {
        // `{fd, N}` starts nothing: the descriptor is the port's whole I/O stream.
        SpawnSpec::Fd { .. } => return Ok(None),
        SpawnSpec::Spawn { command, argv: None } => {
            // The oracle rejects an empty name in its own `open_port/2` validation, before any
            // driver is reached; letting it through would exec an empty program and report
            // `enoent` for a call the oracle calls `badarg`.
            if command.is_empty() {
                return Err(error::badarg());
            }
            let mut shell = Command::new("/bin/sh");
            shell.arg("-c").arg(command);
            shell
        }
        SpawnSpec::Spawn {
            argv: Some(argv), ..
        } => program_command(argv)?,
        // `{spawn_executable, Path}` builds its argv from the `args` option.
        SpawnSpec::SpawnExecutable { path } => {
            if path.is_empty() {
                return Err(error::badarg());
            }
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
        // The child's stderr is **inherited** by default, exactly as the reference's
        // `dup(2)` makes it (`sys_drivers.c:500`): a child's diagnostics reach the emulator's
        // stderr, are not port data, and can never block the child on an undrained pipe.
        .stderr(Stdio::inherit());
    if stream_options.stderr_to_stdout {
        // The option the reference honors by handing the child its stdout pipe as stderr as
        // well (`stderrfd = opts->redir_stderr ? ifd[1] : dup(2)`). The stdio of a
        // `Command` is set up by the standard library *before* the `pre_exec` hooks run, so by
        // the time this closure runs the child's descriptor 1 already is the stdout pipe:
        // duplicating it onto descriptor 2 makes stderr an **exact** alias of stdout (true
        // write ordering, one pipe, no second buffer).
        // SAFETY: `CommandExt::pre_exec` is unsafe because the closure runs in the forked
        // child between `fork` and `exec`, where only async-signal-safe operations are legal;
        // `dup2` is async-signal-safe (POSIX) and the closure does nothing else. Both
        // descriptors are the child's own and are open when it runs.
        unsafe {
            command.pre_exec(dup_stderr_onto_stdout);
        }
    }
    Ok(Some(command))
}

/// The child-side `pre_exec` hook of the `stderr_to_stdout` option: makes descriptor 2 an exact
/// duplicate of descriptor 1.
///
/// It runs in the forked child after the standard library has set the child's standard streams
/// up and before `exec`, so descriptor 1 is the port's stdout pipe; `dup2` clears
/// `FD_CLOEXEC` on the duplicate, so it survives the `exec` as the child's stderr.
///
/// # Errors
///
/// The failing `dup2` as a [`std::io::Error`]; the standard library turns a failing `pre_exec`
/// into a failed `Command::spawn`.
fn dup_stderr_onto_stdout() -> std::io::Result<()> {
    // SAFETY: `dup2` is async-signal-safe, which is the whole constraint on a `pre_exec`
    // closure; both descriptors are the child's own and are already open.
    if unsafe { libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) } == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
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

/// The reference's parent-side `access(Path, X_OK)` check for a `{spawn_executable, Path}`
/// port (`spawn_start`, `sys_drivers.c:521`).
///
/// This check is what separates the oracle's two failure shapes: a path *it* rejects is an
/// `open_port/2` **error** — `error:enoent` for a program that does not exist, `error:eacces`
/// for a directory it cannot search or a file without its execute bit, `error:enotdir` for a
/// path whose component is a file — while a path the check accepts but the later `exec`
/// rejects (a searchable directory, a file with no recognizable executable format) **opens
/// the port** and reports the failing `errno` as the child's exit status (see
/// [`fork_exec_child`]).
///
/// The check runs in the *emulator's* working directory, exactly as the reference's does; a
/// `{cd, Dir}` option is applied later, in the child.
///
/// The raw syscall is [`crate::fs_access::try_access`], the crate's one `access(2)` call site —
/// only the errno's rendering into the oracle's raise is this driver's.
///
/// # Errors
///
/// [`error::spawn_failure`] of the failing `access(2)` — the errno atom the oracle raises
/// (`error:enoent`, `error:eacces`, `error:enotdir`, …) — and [`beam_common::BeamError::BadArg`]
/// for a path containing an interior NUL byte.
fn check_executable(path: &str) -> Result<()> {
    crate::fs_access::try_access(Path::new(path), crate::fs_access::AccessMode::Execute).map_err(
        |err| match err {
            // A refused check carries the untouched `access(2)` errno; render it as the
            // oracle's raise.
            BeamError::Io(err) => error::spawn_failure(err),
            // The wrapper's own fault (an interior-NUL path) stays `badarg`, unchanged.
            other => other,
        },
    )
}

/// Forks a child that runs `path` directly, reproducing the reference's **deferred** `exec`.
///
/// The reference never `exec`s a `{spawn_executable, Path}` program itself: it hands the name
/// to its forked child, which `execve`s it and calls `_exit(errno)` when that fails
/// (`erl_child_setup.c:361-369`). A `Command::spawn` cannot express that — it reports the
/// failing `exec` synchronously — so this is used for the paths whose `access(Path, X_OK)`
/// check passed ([`check_executable`]) but whose `exec` the standard library rejects. The
/// result is a real child process whose exit status is the failing `errno` (13 for a directory)
/// and whose stdout is an empty pipe that reaches end of file, exactly like the oracle's.
///
/// Everything that allocates — the program, its argv, the environment block and the `{cd, Dir}`
/// path, as NUL-terminated C strings — is built **before** the fork, so the child only calls
/// async-signal-safe operations (`dup2`/`close`/`chdir`/`signal`/`execve`/`_exit`) and never
/// touches the allocator. The child's stderr is left alone unless the `stderr_to_stdout`
/// option asked for it, matching the reference's `dup(2)`.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] for an argument or environment entry containing an
/// interior NUL, and [`beam_common::BeamError::Io`] when a `pipe(2)`, `fork(2)` or the parent
/// side's `fcntl(2)` fails (nothing is left registered or running in that case).
fn fork_exec_child(
    path: &str,
    options: &SpawnOptions,
    stream_options: StreamOptions,
) -> Result<(ChildProc, Option<RawFd>, Option<RawFd>)> {
    // The program and its argv: `argv[0]` is the path itself (the reference substitutes its
    // default `argv[0]` with the command line), then the `{args, …}` entries.
    let program = CString::new(path.as_bytes()).map_err(|_| error::badarg())?;
    let mut argv = Vec::with_capacity(options.args.len() + 2);
    argv.push(program.clone());
    for arg in &options.args {
        argv.push(CString::new(arg.as_bytes()).map_err(|_| error::badarg())?);
    }
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|arg| arg.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());

    // The environment: the emulator's own, with the `{env, …}` additions overlaid (a repeated
    // name replaces the inherited entry, the same rule `Command::env` applies).
    let mut envp = Vec::with_capacity(options.env.len());
    for (key, value) in std::env::vars_os() {
        let key_bytes = key.as_os_str().as_bytes();
        if options
            .env
            .iter()
            .any(|(name, _)| name.as_bytes() == key_bytes)
        {
            continue;
        }
        envp.push(env_entry(key_bytes, value.as_os_str().as_bytes())?);
    }
    for (key, value) in &options.env {
        envp.push(env_entry(key.as_bytes(), value.as_bytes())?);
    }
    let mut envp_ptrs: Vec<*const libc::c_char> = envp.iter().map(|entry| entry.as_ptr()).collect();
    envp_ptrs.push(std::ptr::null());

    let dir = match &options.cd {
        Some(dir) => Some(CString::new(dir.as_bytes()).map_err(|_| error::badarg())?),
        None => None,
    };

    let mut stdin_pipe = [0 as RawFd; 2];
    let mut stdout_pipe = [0 as RawFd; 2];
    // SAFETY: both arrays are valid, writable 2-element arrays, so `pipe(2)` writes two fresh
    // descriptors into each. On the failure paths below the descriptors already created are
    // closed before returning, so none leaks.
    if unsafe { libc::pipe(stdin_pipe.as_mut_ptr()) } != 0 {
        return Err(error::io(std::io::Error::last_os_error()));
    }
    // SAFETY: as above, and the first pipe is closed if this one cannot be created.
    if unsafe { libc::pipe(stdout_pipe.as_mut_ptr()) } != 0 {
        let err = std::io::Error::last_os_error();
        close_fd(stdin_pipe[0]);
        close_fd(stdin_pipe[1]);
        return Err(error::io(err));
    }

    // SAFETY: the child runs only async-signal-safe calls on buffers and descriptors that were
    // fully built and opened above — nothing allocates or takes a lock between `fork` and
    // `exec` — and every pointer it passes to `execve` is borrowed from a value that outlives
    // the call (`argv`/`envp`/`dir`/`program` are dropped only after the fork has returned).
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let err = std::io::Error::last_os_error();
        for fd in [stdin_pipe[0], stdin_pipe[1], stdout_pipe[0], stdout_pipe[1]] {
            close_fd(fd);
        }
        return Err(error::io(err));
    }
    if pid == 0 {
        // The forked child. Only async-signal-safe operations from here on: a failure is
        // reported the way the reference reports it, by exiting with the failing `errno`.
        // SAFETY: every descriptor below is one this function just created and still owns, and
        // every pointer is a live NUL-terminated C string owned by the frame above. `_exit(2)`
        // never returns, so the child cannot touch the parent's state.
        unsafe {
            if libc::dup2(stdin_pipe[0], libc::STDIN_FILENO) < 0
                || libc::dup2(stdout_pipe[1], libc::STDOUT_FILENO) < 0
                || (stream_options.stderr_to_stdout
                    && libc::dup2(libc::STDOUT_FILENO, libc::STDERR_FILENO) < 0)
            {
                libc::_exit(current_errno());
            }
            // The pipe ends above descriptor 2 are the parent's now; leaving them open would
            // keep the port's streams alive past the `exec`.
            for fd in [stdin_pipe[0], stdin_pipe[1], stdout_pipe[0], stdout_pipe[1]] {
                if fd > libc::STDERR_FILENO {
                    libc::close(fd);
                }
            }
            // The `{cd, Dir}` option is the child's, as in the reference (`chdir(wd)` before
            // `exec`, `erl_child_setup.c:302`): a directory it cannot enter is the child's
            // failure, reported as that errno by its exit status.
            if let Some(dir) = &dir {
                if libc::chdir(dir.as_ptr()) < 0 {
                    libc::_exit(current_errno());
                }
            }
            // The standard library ignores `SIGPIPE` in this process; a spawned child must not
            // inherit that (its own `Command::spawn` resets it the same way).
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            libc::execve(program.as_ptr(), argv_ptrs.as_ptr(), envp_ptrs.as_ptr());
            libc::_exit(current_errno());
        }
    }

    // The parent: the child owns the two ends it was handed, so only the port's ends are kept.
    close_fd(stdin_pipe[0]);
    close_fd(stdout_pipe[1]);
    let stdin_fd = stdin_pipe[1];
    let stdout_fd = stdout_pipe[0];
    // Non-blocking must be in place before either descriptor reaches the poller.
    for fd in [stdin_fd, stdout_fd] {
        if let Err(err) = set_nonblocking(fd) {
            // The child is alive but unusable: reap it, then release the descriptors.
            close_fd(stdin_fd);
            close_fd(stdout_fd);
            let mut child = ChildProc::Forked(ForkedChild::new(pid));
            let _reap = child.kill_and_reap();
            return Err(err);
        }
    }
    Ok((
        ChildProc::Forked(ForkedChild::new(pid)),
        Some(stdin_fd),
        Some(stdout_fd),
    ))
}

/// The `NAME=VALUE` environment entry `name`/`value` describe, as a NUL-terminated C string.
///
/// # Errors
///
/// [`beam_common::BeamError::BadArg`] when the entry contains an interior NUL (which cannot be
/// passed through a `char *`, and which the oracle would reject as a `badarg` too).
fn env_entry(name: &[u8], value: &[u8]) -> Result<CString> {
    let mut entry = Vec::with_capacity(name.len() + value.len() + 1);
    entry.extend_from_slice(name);
    entry.push(b'=');
    entry.extend_from_slice(value);
    CString::new(entry).map_err(|_| error::badarg())
}

/// The current `errno` without allocating — the one thing the forked child needs beyond the
/// async-signal-safe syscalls themselves.
///
/// `std::io::Error::from_raw_os_error` only stores the code (formatting it, which allocates, is
/// what the child must not do), so this is safe to call between `fork` and `_exit(2)`.
fn current_errno() -> libc::c_int {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EINVAL)
}

/// The `spawn` driver: an OS child process (or an adopted descriptor) behind a port.
///
/// Constructed directly from a [`SpawnSpec`]/[`SpawnOptions`] pair (see the module docs) and
/// handed to [`crate::PortRegistry::open`], which runs [`Driver::start`] and thereby
/// registers the child's stdout with the poller.
///
/// The driver takes no locks and adds no `Send`/`Sync` bound, so once it is boxed into a
/// [`crate::Port`] it stays on that port's **single OS thread** (the same invariant the other
/// drivers carry; see the crate docs).
pub struct SpawnDriver {
    /// The port "name" string (the shell command line, the argv join, or `fd:<N>`).
    command: String,
    /// The child process; `None` for a `{fd, N}` port.
    child: Option<ChildProc>,
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
    /// Whether the `eof` stream option asked for the end-of-file notification.
    eof_notify: bool,
    /// Whether that end-of-file has already been handed out, so it is reported exactly once.
    eof_reported: bool,
    /// Whether the driver owns (and must close) the descriptors. False for an adopted
    /// `{fd, N}` descriptor, which belongs to the Erlang side.
    own_fds: bool,
    /// Whether one adopted `{fd, N}` descriptor serves **both** directions (so it is
    /// registered once with [`Interest::BOTH`] and its read registration *is* its write
    /// registration). Always false for a child port, whose read and write streams are two
    /// distinct pipes.
    duplex: bool,
    /// The OS process id; `None` for a `{fd, N}` port.
    os_pid: Option<u32>,
}

impl SpawnDriver {
    /// Starts the child `spec`/`options` describe — or adopts `{fd, N}` — and returns the
    /// driver, with every [`StreamOptions`] at its default (stderr inherited, no `eof`
    /// notification).
    ///
    /// This is [`SpawnDriver::with_stream_options`] with [`StreamOptions::default`]; see it for
    /// the full contract.
    ///
    /// # Errors
    ///
    /// Exactly as [`SpawnDriver::with_stream_options`].
    pub fn new(spec: &SpawnSpec, options: &SpawnOptions) -> Result<SpawnDriver> {
        SpawnDriver::with_stream_options(spec, options, StreamOptions::default())
    }

    /// Starts the child `spec`/`options` describe — or adopts `{fd, N}` — and returns the
    /// driver, honoring the child-stream options `stream_options` carries.
    ///
    /// This **really** spawns (the poller is not touched until [`Driver::start`]), so a
    /// failure is reported here, before anything is registered anywhere. The `stderr_to_stdout`
    /// option is applied in the child (a `dup2(1, 2)`); the `eof` option only decides whether
    /// [`Driver::take_eof`] ever reports the end-of-file the driver latches anyway.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Raise`] carrying the **errno atom** the oracle raises when
    /// the child cannot be started — `error:enoent` for an unknown program, `error:eacces`
    /// for an unreadable one, `error:enotdir` for a path through a non-directory, … The
    /// reference answers `erl_errno_id(errno)` (`erts_internal_open_port_2`,
    /// `erl_bif_port.c:84`) and the atom is the lowercase errno name (see the crate's `error`
    /// module). For a `{spawn_executable, Path}` port that atom is the one
    /// [`check_executable`]'s `access(Path, X_OK)` reports, exactly as `spawn_start` does it
    /// (`sys_drivers.c:521`); a program the check accepts but the `exec` rejects is **not** an
    /// error — the port opens and the child exits with that errno (see [`fork_exec_child`]).
    /// An errno this crate does not recognise falls back to
    /// [`beam_common::BeamError::Io`], as does a failure putting a descriptor in non-blocking
    /// mode (or reaping a child a failed spawn left behind); [`beam_common::BeamError::BadArg`]
    /// for an `{fd, N}` spec with a negative descriptor, for an empty argv vector, or for the
    /// empty command/path the oracle rejects.
    pub fn with_stream_options(
        spec: &SpawnSpec,
        options: &SpawnOptions,
        stream_options: StreamOptions,
    ) -> Result<SpawnDriver> {
        let command = spec.command();
        if let SpawnSpec::Fd { fd } = spec {
            if *fd < 0 {
                return Err(error::badarg());
            }
            set_nonblocking(*fd)?;
            return Ok(SpawnDriver {
                command,
                child: None,
                // The single adopted descriptor serves **both** directions: it is the port's
                // read stream and the stream `port_command` writes to (`start` registers it
                // once with `Interest::BOTH`).
                stdin_fd: Some(*fd),
                stdout_fd: Some(*fd),
                in_token: None,
                out_token: None,
                read_eof: false,
                stream: options.stream,
                exit_status: options.exit_status,
                exit_reported: false,
                eof_notify: stream_options.eof,
                eof_reported: false,
                // The descriptor belongs to the Erlang side: deregistered, never closed.
                own_fds: false,
                duplex: true,
                os_pid: None,
            });
        }
        // `build_command_with_stream` answers `None` only for the `{fd, N}` form, handled above.
        // It runs first so the name validation it owns (an empty command/path is the oracle's
        // `badarg`, done before any driver is reached) precedes the driver's own check below.
        let Some(mut built) = build_command_with_stream(spec, options, stream_options)? else {
            return Err(error::badarg());
        };
        // The reference checks the program in the *emulator* before forking a
        // `{spawn_executable, Path}` port (`spawn_start`, `sys_drivers.c:521`), so a path it
        // rejects is a constructor error; only a path the check accepts reaches the child.
        let executable_path = match spec {
            SpawnSpec::SpawnExecutable { path } => {
                check_executable(path)?;
                Some(path.as_str())
            }
            _ => None,
        };
        let (mut child, stdin_fd, stdout_fd) = match built.spawn() {
            Ok(mut started) => {
                // Take both ends out of the child so the driver owns them as plain descriptors
                // (the same reason `FdDriver` owns its fds: the poller and the syscall wrappers
                // work on raw descriptors). stderr is never a pipe here
                // (`build_command_with_stream` inherits it, or makes it a duplicate of the
                // stdout pipe), so the `Child` holds no stderr pipe to drain.
                let stdin_fd = started.stdin.take().map(IntoRawFd::into_raw_fd);
                let stdout_fd = started.stdout.take().map(IntoRawFd::into_raw_fd);
                (ChildProc::Standard(started), stdin_fd, stdout_fd)
            }
            // The standard library reports a failing `exec` synchronously; the reference defers
            // it to the child. Only a `{spawn_executable, Path}` port can name a path here — its
            // `access(Path, X_OK)` check passed just above — so the failure is the `exec`
            // itself, and [`fork_exec_child`] reproduces the oracle: a real child that exits
            // with the failing errno.
            Err(err) => match executable_path {
                Some(path) => fork_exec_child(path, options, stream_options)?,
                None => return Err(error::spawn_failure(err)),
            },
        };
        let os_pid = child.id();
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
            let _reap = child.kill_and_reap();
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
            eof_notify: stream_options.eof,
            eof_reported: false,
            own_fds: true,
            duplex: false,
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
        child.try_wait().map_err(error::io)
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
        // The reaped status is cached by the child, so a later teardown still sees it and
        // needs no kill.
        match child.try_wait().map_err(error::io)? {
            Some(status) => {
                self.exit_reported = true;
                Ok(Some(status))
            }
            None => Ok(None),
        }
    }

    /// Takes the child's **end-of-file** notification once, when the `eof` stream option asked
    /// for it.
    ///
    /// `Ok(true)` only when the option was set *and* [`SpawnDriver::eof`] has latched the
    /// `read(2) == 0` the child's stdout reported, and only the first time; every other call
    /// answers `Ok(false)`. The latch itself is independent of the option, so the driver
    /// behaves identically without it — only the report is withheld.
    ///
    /// # Errors
    ///
    /// Currently infallible; the `Result` mirrors [`Driver::take_eof`] so a future check that
    /// can fail has a place to report it.
    pub fn take_eof(&mut self) -> Result<bool> {
        if !self.eof_notify || !self.read_eof || self.eof_reported {
            return Ok(false);
        }
        self.eof_reported = true;
        Ok(true)
    }

    /// Arms write readiness for `fd` — registering it the first time, re-registering it
    /// after that — so [`Driver::ready_output`] runs once the kernel buffer drains.
    ///
    /// A `{fd, N}` port needs nothing here: its single descriptor is already registered with
    /// [`Interest::BOTH`] (see [`SpawnDriver::start`]), so write readiness never lapses.
    fn arm_output(&mut self, ctx: &mut DriverContext<'_>, fd: RawFd) -> Result<()> {
        if self.duplex {
            return Ok(());
        }
        match self.out_token {
            Some(token) => ctx.reregister(fd, token, Interest::WRITABLE),
            None => {
                self.out_token = Some(ctx.register(fd, Interest::WRITABLE)?);
                Ok(())
            }
        }
    }

    /// Drops the write registration, so no further writable events arrive.
    ///
    /// A `{fd, N}` port keeps its single `Interest::BOTH` registration in place: dropping it
    /// would also drop the *read* direction, and an empty output queue already makes a
    /// spurious `OUTPUT` task a no-op.
    fn disarm_output(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if self.duplex {
            return Ok(());
        }
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
    /// owns it) **and** left registered, because the one descriptor also serves the write
    /// direction (see [`SpawnDriver::duplex`]): `read_eof` alone stops further reads, while
    /// the shared `Interest::BOTH` registration keeps a stalled write flushable.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] when the poller rejects the deregistration.
    fn release_stdout(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        if self.duplex {
            return Ok(());
        }
        let (Some(token), Some(fd)) = (self.in_token.take(), self.stdout_fd.take()) else {
            return Ok(());
        };
        let result = ctx.deregister(fd, token);
        if self.own_fds {
            close_fd(fd);
        }
        result
    }

    /// Drains the port's output queue into the port's write stream, returning the number of
    /// bytes written.
    ///
    /// A short write leaves the remainder queued and arms write readiness; a fully drained
    /// queue drops the write registration again (except for a `{fd, N}` port, whose single
    /// registration is shared with the read direction — see [`SpawnDriver::disarm_output`]).
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::BadArg`] when the driver holds no write descriptor at all (a
    /// defensive guard: a live port always has one). [`beam_common::BeamError::Io`] when the
    /// write fails — the operating system's `EBADF` for a `{fd, N}` descriptor opened
    /// read-only, or `EPIPE` for a child that closed its stdin (a typed failure rather than
    /// silently discarded bytes).
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
        child.kill_and_reap()
    }
}

impl Driver for SpawnDriver {
    fn name(&self) -> &str {
        "spawn"
    }

    fn start(&mut self, ctx: &mut DriverContext<'_>) -> Result<()> {
        // Only the already-created descriptors are registered here; the child was started by
        // the constructor, so nothing can fail midway through a spawn.
        if self.duplex {
            // A `{fd, N}` port has a single descriptor serving both directions, so it gets a
            // single poller membership with `Interest::BOTH` (registering it twice would be
            // the poller's `EEXIST`). That one registration is the port's read readiness *and*
            // its write readiness, which is why the duplex write/EOF paths never re-arm or
            // deregister it.
            if let Some(fd) = self.stdout_fd {
                self.in_token = Some(ctx.register(fd, Interest::BOTH)?);
            }
            return Ok(());
        }
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

    fn take_eof(&mut self) -> Result<bool> {
        SpawnDriver::take_eof(self)
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

/// The child process behind a spawn port.
///
/// Two ways of getting one exist because the reference itself has two. The ordinary path is a
/// [`std::process::Command`], which reports a failing `exec` to its parent; the
/// `{spawn_executable, Path}` port whose `exec` the oracle *defers* is instead forked by
/// [`fork_exec_child`], and has no [`Child`] handle — only a pid, which this driver reaps with
/// `waitpid(2)`.
///
/// Both answer the same three questions (`id`, `try_wait`, `kill_and_reap`) in the same units:
/// the `{Port, {exit_status, N}}` code, i.e. a child's own exit code, or `128 + signal` for one
/// killed by a signal (the shell convention, and what the legacy `spawn` driver reports).
enum ChildProc {
    /// A child started through [`std::process::Command`]: the `{spawn, Cmd}`/argv forms, and a
    /// `{spawn_executable, Path}` whose `exec` succeeds.
    Standard(Child),
    /// A child this driver forked itself, whose `execve` has already been attempted in the
    /// child (see [`fork_exec_child`]).
    Forked(ForkedChild),
}

impl ChildProc {
    /// The child's OS process id (the value [`SpawnDriver::os_pid`] reports).
    fn id(&self) -> u32 {
        match self {
            ChildProc::Standard(child) => child.id(),
            ChildProc::Forked(child) => child.id(),
        }
    }

    /// Reaps the child if it has already exited, answering its `{exit_status, N}` code.
    ///
    /// A status that has been reaped is remembered, so a later call — including the teardown's
    /// — still answers `Some` and needs no kill.
    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        match self {
            ChildProc::Standard(child) => {
                child.try_wait().map(|status| status.map(exit_status_code))
            }
            ChildProc::Forked(child) => child.try_wait(),
        }
    }

    /// Kills the child when it is still running and reaps it.
    ///
    /// # Errors
    ///
    /// [`beam_common::BeamError::Io`] when the kill or the wait fails; the wait is attempted
    /// either way so a dead child is never left as a zombie.
    fn kill_and_reap(&mut self) -> Result<()> {
        match self.try_wait().map_err(error::io)? {
            // Already reaped (or already exited): no kill is needed and none is attempted.
            Some(_status) => Ok(()),
            None => {
                let killed = match self {
                    ChildProc::Standard(child) => child.kill(),
                    ChildProc::Forked(child) => child.kill(),
                };
                // Reap whatever the kill outcome: a child that has died is a zombie until it is
                // waited on.
                let waited = match self {
                    ChildProc::Standard(child) => child.wait().map(|_status| ()),
                    ChildProc::Forked(child) => child.wait().map(|_status| ()),
                };
                match killed {
                    Err(err) => Err(error::io(err)),
                    Ok(()) => waited.map_err(error::io),
                }
            }
        }
    }
}

/// A child this driver forked itself, reaped with `waitpid(2)`.
///
/// [`fork_exec_child`] needs a child of its own because [`std::process::Child`] reports a
/// failing `exec` to its parent, while the reference lets the *child* report it — by exiting
/// with the failing `errno`. Nothing else about the child differs, so this mirrors the two
/// things the driver asks of a `Child`: a non-blocking liveness probe and a kill-and-reap.
struct ForkedChild {
    /// The child's process id, as `fork(2)` returned it.
    pid: libc::pid_t,
    /// The `{exit_status, N}` code, once it has been reaped (so a later call still answers).
    status: Option<i32>,
}

impl ForkedChild {
    /// Wraps the pid `fork(2)` returned; nothing has been reaped yet.
    fn new(pid: libc::pid_t) -> ForkedChild {
        ForkedChild { pid, status: None }
    }

    /// The child's OS process id.
    fn id(&self) -> u32 {
        self.pid as u32
    }

    /// Reaps the child if it has already exited, without blocking.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] from a failing `waitpid(2)`.
    fn try_wait(&mut self) -> std::io::Result<Option<i32>> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let mut raw: libc::c_int = 0;
        // SAFETY: `raw` is a valid `c_int` for `waitpid(2)` to write the status into, and
        // `self.pid` is the child this driver forked and has not reaped yet. `WNOHANG` keeps
        // the call from blocking; it reaps (and thereby clears) the child when it has exited.
        let reaped = unsafe { libc::waitpid(self.pid, &mut raw, libc::WNOHANG) };
        if reaped == 0 {
            return Ok(None);
        }
        if reaped < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let status = wait_status_code(raw);
        self.status = Some(status);
        Ok(Some(status))
    }

    /// Sends `SIGKILL` to the child.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] from a failing `kill(2)` (`ESRCH` when the child is already gone).
    fn kill(&mut self) -> std::io::Result<()> {
        // SAFETY: `self.pid` is the child this driver forked; `SIGKILL` needs no argument and
        // `kill(2)` neither retains a pointer nor writes to the caller's memory.
        if unsafe { libc::kill(self.pid, libc::SIGKILL) } == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// Blocks until the child has exited, answering its `{exit_status, N}` code.
    ///
    /// # Errors
    ///
    /// [`std::io::Error`] from a failing `waitpid(2)`.
    fn wait(&mut self) -> std::io::Result<i32> {
        if let Some(status) = self.status {
            return Ok(status);
        }
        let mut raw: libc::c_int = 0;
        loop {
            // SAFETY: as in `try_wait`; without `WNOHANG` the call blocks until the child this
            // driver forked exits, and an `EINTR` (a signal delivered to the port's thread) is
            // retried rather than reported as a failure.
            let reaped = unsafe { libc::waitpid(self.pid, &mut raw, 0) };
            if reaped == self.pid {
                break;
            }
            if reaped < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(err);
            }
        }
        let status = wait_status_code(raw);
        self.status = Some(status);
        Ok(status)
    }
}

/// The `{Port, {exit_status, N}}` code a raw `waitpid(2)` status stands for.
///
/// A child that exited normally reports its own exit code; one **killed by a signal** reports
/// `128 + signal`, the shell convention the legacy `spawn` driver's status carries.
fn wait_status_code(raw: libc::c_int) -> i32 {
    if libc::WIFEXITED(raw) {
        libc::WEXITSTATUS(raw)
    } else if libc::WIFSIGNALED(raw) {
        128 + libc::WTERMSIG(raw)
    } else {
        // Neither an exit nor a signal (a stop or a continued child): no exit code exists, and
        // `WNOHANG` without `WUNTRACED` never reports one, so this is unreachable in practice.
        128
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
    use super::{build_command, SpawnDriver, SpawnOptions, SpawnSpec, SpawnStream, StreamOptions};
    use crate::driver::{Driver, DriverContext, PortArms};
    use crate::poller::Poller;
    use crate::queue::ByteQueue;
    use beam_common::BeamError;
    use beam_process::PortId;
    use std::ffi::OsStr;
    use std::os::unix::io::RawFd;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// The generous outer **hang guard** for this suite's waits.
    ///
    /// Every wait below polls the *condition* it is after — the child has exited, the once-only
    /// status is reported, the read path latched end-of-file — so it ends as soon as the
    /// condition holds, however heavily the host is loaded; the child is waited on for as long
    /// as it actually takes. This bound is deliberately **not** a budget tuned for an idle host
    /// (a 5-second cap of exactly that shape was the flake: on a loaded host the child needs far
    /// longer than that to be scheduled, run and be reaped). It exists only so a genuinely
    /// broken driver fails a test in bounded time instead of hanging the suite, and a healthy
    /// run returns long before it.
    const WAIT_HANG_GUARD: Duration = Duration::from_secs(60);

    /// Drives `driver`'s readiness loop until its input stream latches end-of-file.
    ///
    /// Waits on the **condition** (the end-of-file latch) under [`WAIT_HANG_GUARD`], never on a
    /// wall-clock budget tuned for an idle host, so a loaded host cannot fail it; the poll is
    /// retried on `EINTR`, which the `SIGCHLD` deliveries of the parallel cases in this binary
    /// make a normal outcome.
    fn drive_to_eof(driver: &mut SpawnDriver) {
        let mut poller = Poller::new().expect("a poll set");
        let mut input = ByteQueue::new();
        let mut output = ByteQueue::new();
        let mut arms = PortArms::default();
        {
            let mut ctx = DriverContext::new(
                PortId::new(1),
                &mut poller,
                &mut input,
                &mut output,
                &mut arms,
            );
            driver.start(&mut ctx).expect("the stdout registration");
        }
        let deadline = Instant::now() + WAIT_HANG_GUARD;
        while !driver.eof() && Instant::now() < deadline {
            let events = loop {
                match poller.poll(Some(Duration::from_millis(100))) {
                    Ok(events) => break events,
                    Err(BeamError::Io(err))
                        if err.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        continue
                    }
                    Err(err) => panic!("the poll succeeds: {err:?}"),
                }
            };
            if events
                .iter()
                .any(|event| event.ready().readable() || event.ready().is_read_closed())
            {
                let mut ctx = DriverContext::new(
                    PortId::new(1),
                    &mut poller,
                    &mut input,
                    &mut output,
                    &mut arms,
                );
                driver.ready_input(&mut ctx).expect("the read");
            }
        }
        assert!(driver.eof(), "the child's end-of-file is latched");
    }

    /// Waits for the child's once-only `{exit_status, N}` report under [`WAIT_HANG_GUARD`].
    ///
    /// It polls the **condition** — the status has arrived — rather than a wall-clock budget
    /// tuned for an idle host, so a loaded host does not fail it; the guard only stops a
    /// genuinely broken driver from hanging the suite.
    ///
    /// The driver's own reap is a caching `try_wait` (`ChildProc::try_wait`), so asking it
    /// again is idempotent: this neither double-reaps nor races the driver's teardown, and the
    /// status stays available for the `stop`/`Drop` path.
    fn wait_for_exit_status(driver: &mut SpawnDriver) -> i32 {
        let deadline = Instant::now() + WAIT_HANG_GUARD;
        loop {
            if let Some(status) = driver.take_exit_status().expect("the wait succeeds") {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the child's exit status arrives within the hang guard"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

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
    fn an_unknown_program_is_the_errno_atom_constructor_error() {
        let spec = SpawnSpec::SpawnExecutable {
            path: "/nonexistent/xyz-not-real".to_string(),
        };
        let error = SpawnDriver::new(&spec, &SpawnOptions::default())
            .map(|_| ())
            .expect_err("a program that cannot be started is a constructor error");
        // The oracle raises `error:enoent` for the same path, so the reason is the atom.
        assert_eq!(error.as_atom(), Some("enoent"));
        assert!(matches!(error, BeamError::Raise(_)));
    }

    #[test]
    fn a_searchable_directory_opens_the_port_and_the_child_reports_the_eacces() {
        // The reference checks `access(Path, X_OK)` **in the emulator** for a
        // `{spawn_executable, Path}` port (`spawn_start`, `sys_drivers.c:521`). A directory is
        // searchable, so the check passes: the port *opens*, and only the forked child's later
        // `exec` fails — and a failing `exec` makes the child `_exit(errno)`
        // (`erl_child_setup.c:361-369`), so the port reports `{exit_status, 13}` (EACCES)
        // followed by `eof` (the child wrote nothing). The oracle agrees (probed against
        // erts-17.1): `open_port({spawn_executable,"/etc"},[binary,exit_status,eof])` yields
        // `[{exit_status,13}, eof]`.
        let spec = SpawnSpec::SpawnExecutable {
            path: "/etc".to_string(),
        };
        let options = SpawnOptions {
            exit_status: true,
            ..SpawnOptions::default()
        };
        let mut driver = SpawnDriver::with_stream_options(
            &spec,
            &options,
            StreamOptions {
                stderr_to_stdout: false,
                eof: true,
            },
        )
        .expect("the `access(Path, X_OK)` check passes for a searchable directory");
        assert!(
            driver.os_pid().is_some(),
            "a real child process backs the port"
        );
        // The child's `exec` failed before it could write, so its stdout reaches end of file.
        drive_to_eof(&mut driver);
        assert!(
            driver.take_eof().expect("the check succeeds"),
            "the empty stdout reaches end of file"
        );

        let status = wait_for_exit_status(&mut driver);

        assert_eq!(
            status,
            libc::EACCES,
            "the child exits with the errno its `exec` failed with"
        );
        assert_eq!(
            driver.take_exit_status().expect("the wait succeeds"),
            None,
            "the status is handed out exactly once"
        );
    }

    #[test]
    fn a_bad_cd_is_the_childs_failure_not_the_constructor() {
        // The `{cd, Dir}` option is the **child's** (`chdir(wd)` before `exec`,
        // `erl_child_setup.c:302`), so a directory the child cannot enter is reported by its
        // exit status rather than as an `open_port/2` error — the oracle opens the port and
        // answers `{exit_status, 2}` (ENOENT) here. Probed against erts-17.1.
        let spec = SpawnSpec::SpawnExecutable {
            path: "/bin/sh".to_string(),
        };
        let options = SpawnOptions {
            cd: Some("/no/such/dir".to_string()),
            exit_status: true,
            ..SpawnOptions::default()
        };
        let mut driver = SpawnDriver::with_stream_options(
            &spec,
            &options,
            StreamOptions {
                stderr_to_stdout: false,
                eof: true,
            },
        )
        .expect("the port opens; only the child's `chdir` fails");
        assert!(
            driver.os_pid().is_some(),
            "a real child process backs the port"
        );
        drive_to_eof(&mut driver);

        let status = wait_for_exit_status(&mut driver);

        assert_eq!(status, libc::ENOENT, "the child's `chdir` errno");
    }

    #[test]
    fn a_file_with_no_executable_format_is_deferred_to_the_child_too() {
        // `access(Path, X_OK)` passes (the file carries its execute bit) but the `exec` rejects
        // the *format*, so the oracle opens the port and the child exits with `ENOEXEC` (8).
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("beam_io_spawn_noexec_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the scratch directory is writable");
        let file = dir.join("not-a-program");
        std::fs::write(&file, b"plain text, not an executable format\n").expect("the file");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755))
            .expect("the execute bit is set");

        let spec = SpawnSpec::SpawnExecutable {
            path: file.to_string_lossy().into_owned(),
        };
        let options = SpawnOptions {
            exit_status: true,
            ..SpawnOptions::default()
        };
        let mut driver = SpawnDriver::new(&spec, &options).expect("the port opens");
        assert!(
            driver.os_pid().is_some(),
            "a real child process backs the port"
        );
        drive_to_eof(&mut driver);

        let status = wait_for_exit_status(&mut driver);

        assert_eq!(status, libc::ENOEXEC, "the child's `exec` errno");
        let _ = std::fs::remove_dir_all(&dir);
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

        // Drive the real readiness loop until the child's EOF is latched — waiting on the
        // *condition* under the suite's hang guard, never on a budget tuned for an idle host.
        let deadline = Instant::now() + WAIT_HANG_GUARD;
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
        let status = wait_for_exit_status(&mut driver);

        assert_eq!(status, 7);
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
        let status = wait_for_exit_status(&mut driver);

        assert_eq!(status, 128 + 9);
    }

    #[test]
    fn killing_and_reaping_is_idempotent() {
        let mut driver = SpawnDriver::new(&shell("sleep 30"), &SpawnOptions::default())
            .expect("sleep starts");
        driver.reap_child().expect("the first reap succeeds");
        driver.reap_child().expect("a second reap is a no-op");
        assert_eq!(driver.exited().expect("the wait succeeds"), None);
    }

    #[test]
    fn an_empty_shell_command_is_rejected_as_badarg() {
        // The oracle rejects the empty name in its own `open_port/2` validation
        // (`open_port({spawn,""},[])` is `error:badarg`, verified against erts-17.1), before
        // any driver sees it. An empty command reaching `Command::new("/bin/sh")` would run
        // `sh -c ""` instead — a silently different program, not a failure.
        let spec = shell("");
        assert!(matches!(
            build_command(&spec, &SpawnOptions::default()),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            SpawnDriver::new(&spec, &SpawnOptions::default())
                .map(|_| ())
                .err(),
            Some(BeamError::BadArg)
        ));
        // Only the *empty* name is rejected: `{spawn, " "}` is accepted by the oracle.
        assert!(build_command(&shell(" "), &SpawnOptions::default())
            .expect("a blank command is accepted")
            .is_some());
    }

    #[test]
    fn an_empty_spawn_executable_path_is_rejected_as_badarg() {
        // `open_port({spawn_executable,""},[])` is `error:badarg` in the oracle too.
        let spec = SpawnSpec::SpawnExecutable {
            path: String::new(),
        };
        assert!(matches!(
            build_command(&spec, &SpawnOptions::default()),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            SpawnDriver::new(&spec, &SpawnOptions::default())
                .map(|_| ())
                .err(),
            Some(BeamError::BadArg)
        ));
    }

    #[test]
    fn the_stream_options_default_to_both_off() {
        let options = StreamOptions::default();
        assert!(!options.stderr_to_stdout, "the child's stderr is inherited");
        assert!(!options.eof, "no end-of-file notification is asked for");
        assert!(StreamOptions {
            stderr_to_stdout: true,
            eof: true,
        } != options);
    }

    #[test]
    fn the_eof_notification_is_reported_once_with_the_option() {
        let options = SpawnOptions::default();
        let mut driver = SpawnDriver::with_stream_options(
            &shell("echo hi"),
            &options,
            StreamOptions {
                stderr_to_stdout: false,
                eof: true,
            },
        )
        .expect("echo starts");
        assert!(!driver.eof(), "nothing has been read yet");
        assert!(
            !driver.take_eof().expect("the check succeeds"),
            "an end-of-file that has not happened yet is not reported"
        );
        drive_to_eof(&mut driver);
        assert!(
            driver.take_eof().expect("the check succeeds"),
            "the end-of-file is reported when the option asked for it"
        );
        assert!(
            !driver.take_eof().expect("the check succeeds"),
            "and exactly once"
        );
    }

    #[test]
    fn the_eof_notification_is_withheld_without_the_option() {
        // The latch still happens (`eof()` is true); only the *report* is withheld.
        let mut driver =
            SpawnDriver::new(&shell("echo hi"), &SpawnOptions::default()).expect("echo starts");
        drive_to_eof(&mut driver);
        assert!(driver.eof(), "the end-of-file is latched anyway");
        assert!(
            !driver.take_eof().expect("the check succeeds"),
            "without the `eof` option the port is never told"
        );
    }

    #[test]
    fn an_adopted_fd_reports_its_end_of_file_once_with_the_option() {
        let mut fds = [0 as RawFd; 2];
        // SAFETY: `fds` is a valid, writable 2-element array; on success `pipe(2)` fills both
        // slots with fresh descriptors owned by this test.
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(rc, 0, "pipe(2) succeeded");
        let mut driver = SpawnDriver::with_stream_options(
            &SpawnSpec::Fd { fd: fds[0] },
            &SpawnOptions::default(),
            StreamOptions {
                stderr_to_stdout: false,
                eof: true,
            },
        )
        .expect("the descriptor is adopted");
        // Closing the write end is the adopted descriptor's end of stream.
        // SAFETY: `fds[1]` is the write end of the pipe created above and is closed once.
        assert_eq!(unsafe { libc::close(fds[1]) }, 0);

        drive_to_eof(&mut driver);
        assert!(driver.take_eof().expect("the check succeeds"));
        assert!(!driver.take_eof().expect("the check succeeds"));
        // The adopted read end belongs to the Erlang side: the driver never closed it.
        // SAFETY: `fds[0]` was created by `pipe(2)` above and is closed exactly once here.
        unsafe {
            libc::close(fds[0]);
        }
    }
}
