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

//! The shared **two-VM harness**: a private `epmd`, a per-test scratch directory and
//! session-isolated VM children (**contract A.5**).
//!
//! Two suites drive a real node pair with it — [`tests/dist_two_vm.rs`](../dist_two_vm.rs)
//! (the link itself: `net_adm:ping/1` ⇒ `pong`, one message each way, `nodedown` in both
//! directions) and [`tests/dist_payload_matrix.rs`](../dist_payload_matrix.rs) (payloads far
//! larger than a receiver's default nursery, delivered through the heap-fragment read path).
//! Everything they share lives here; the suites keep only their fixtures, their peer
//! expressions and their tests.
//!
//! # Isolation (why `setsid`, and why a *private* `epmd`)
//!
//! An Erlang VM signals its **process group** (`SIGUSR1` for the break handler, and
//! `erl_child_setup` does the same on the way down), so a VM started as a plain child of the
//! test process can hit the test binary — and any sibling test — with its own signals.
//! Every child here (the private `epmd`, the `erl` peer, the `beam_rt` runtime) is therefore
//! started as `setsid -w <program> …`: its own **session**, so its own process group, whose
//! id is its own pid. Drop sends `TERM` to the whole group, waits a bounded grace, escalates
//! to `KILL`, and reaps — so no `beam_rt`, `erl` or `epmd` is ever left behind.
//! (`setsid` execs the program directly, so the child we hold *is* the program; the group id
//! is read back from `/proc/<pid>/stat` and only trusted when it really is `<pid>` — see
//! [`ChildVm::pgid`].)
//!
//! A private `epmd` on a private port (`ERL_EPMD_PORT=<port> epmd -daemon -port <port>`,
//! with its cwd inside the test's own scratch dir) keeps the run out of any other node's way
//! — the developer's own `epmd`, and any other test's — and lets a pair collide with
//! nothing. A daemonized `epmd` detaches into its own session, so the group kill cannot
//! reach it: [`PrivateEpmd::stop`] finds it by its own argv (`-port <our port>`) and signals
//! that pid, then proves the port is closed.
//!
//! # Skip policy
//!
//! Every precondition is a **loud skip** ([`super::skip_loudly`], under the calling suite's
//! `banner`): a missing `erl`/`erlc`/`epmd`/`setsid`/`kill` (discovered by **filesystem
//! only**, never by running the tool — see [`super::find_on_path`]), a private `epmd` that
//! does not come up, or a runtime that never prints its `ready` line inside the bound. A
//! missing toolchain is never a silent pass, and a *missing committed fixture* cannot happen
//! here: every fixture is compiled at test time with the system `erlc` into the scratch
//! directory.

use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::{find_on_path, require_oracle, skip_loudly};

/// The **real** executable under test (the test target's build guarantees it is current).
pub const BINARY: &str = env!("CARGO_BIN_EXE_beam_rt");

/// How long the private `epmd` may take to accept its first connection.
pub const EPMD_START_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the runtime may take to print its `READY` line (it drives the whole boot script
/// first, then loads and starts the fixture).
pub const READY_TIMEOUT: Duration = Duration::from_secs(30);
/// How long any single expected stdout line may take to appear once its predecessor has.
///
/// Deliberately **longer** than every deadline the peer's expressions carry (a 100 × 200 ms
/// ping budget, a 20 s nodedown wait), so a peer that gives up prints its own `PINGFAIL` /
/// `NODEDOWN_TIMEOUT` line into the transcript *before* this wait expires — a red test then
/// names the peer's verdict instead of only "no line".
pub const LINE_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a VM may take to exit once the interaction has completed.
pub const EXIT_TIMEOUT: Duration = Duration::from_secs(60);
/// The grace a `TERM`ed process group gets before it is `KILL`ed.
pub const KILL_GRACE: Duration = Duration::from_secs(3);
/// The poll interval for every bounded wait in this module.
pub const POLL: Duration = Duration::from_millis(20);
/// How long the killed `epmd` gets to release its port.
pub const EPMD_STOP_TIMEOUT: Duration = Duration::from_secs(3);

// ---------------------------------------------------------------------------------------
// The harness: preconditions, toolchain discovery, the private `epmd` and the fixture
// ---------------------------------------------------------------------------------------

/// Everything one test needs before it can start a VM pair: the discovered tools, the
/// test's own scratch directory, the derived host name and the private `epmd`.
///
/// Declared in drop order: the `epmd` daemon is stopped **before** the scratch directory
/// (its cwd) is removed.
pub struct Harness {
    /// The suite name a loud skip names (the caller's own banner).
    pub banner: &'static str,
    /// The system `erl` — the peer (resolved by [`super::resolve_oracle`]'s rules).
    pub oracle: PathBuf,
    /// The system `erlc` — compiles the fixture at test time.
    pub erlc: PathBuf,
    /// `setsid`, which puts every child in its own session (see the module docs).
    pub setsid: PathBuf,
    /// `kill`, which delivers the group `TERM`/`KILL` a `Child::kill` cannot.
    pub kill: PathBuf,
    /// The short hostname **both** nodes live under, derived exactly as `beam_rt`'s
    /// `dist::local_hostname` derives it (`/proc/sys/kernel/hostname`, then `$HOSTNAME`,
    /// then `localhost`) — never by shelling out to `hostname`.
    pub host: String,
    /// The private `epmd` this run registered with (dropped → stopped).
    pub epmd_daemon: PrivateEpmd,
    /// This test's own directory under `$TMPDIR` (the fixture's output directory, the
    /// children's cwd, and the `-pa` the runtime is given).
    pub scratch: Scratch,
}

impl Harness {
    /// Discovers the toolchain, makes the scratch directory and starts the private `epmd`,
    /// or `None` after a **loud skip** naming exactly what is missing.
    ///
    /// `banner` is the suite name every skip reports, `label` names the scratch directory.
    pub fn new(banner: &'static str, label: &str) -> Option<Harness> {
        let Some(oracle) = require_oracle(banner) else {
            return None;
        };
        let erlc = tool(banner, "erlc")?;
        let epmd_program = tool(banner, "epmd")?;
        let setsid = tool(banner, "setsid")?;
        let kill = tool(banner, "kill")?;
        let scratch = Scratch::new(banner, label)?;
        let host = local_hostname();
        let epmd_daemon = PrivateEpmd::start(banner, &epmd_program, &setsid, &kill, scratch.path())?;
        Some(Harness {
            banner,
            oracle,
            erlc,
            setsid,
            kill,
            host,
            epmd_daemon,
            scratch,
        })
    }

    /// Compiles `source` into `<scratch>/<module>.beam` with the system `erlc`, or `None`
    /// after a loud skip when the compiler fails.
    pub fn compile_fixture(&self, module: &str, source: &str) -> Option<PathBuf> {
        let source_path = self.scratch.path().join(format!("{module}.erl"));
        if let Err(error) = std::fs::write(&source_path, source) {
            skip_loudly(
                self.banner,
                &format!(
                    "cannot write the fixture source {}: {error}",
                    source_path.display()
                ),
            );
            return None;
        }
        let output = Command::new(&self.erlc)
            .arg("-o")
            .arg(self.scratch.path())
            .arg(&source_path)
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                skip_loudly(
                    self.banner,
                    &format!(
                        "`{}` exited with {}: {}",
                        self.erlc.display(),
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                );
                return None;
            }
            Err(error) => {
                skip_loudly(
                    self.banner,
                    &format!("cannot run `{}`: {error}", self.erlc.display()),
                );
                return None;
            }
        }
        let beam = self.scratch.path().join(format!("{module}.beam"));
        if !beam.is_file() {
            skip_loudly(
                self.banner,
                &format!("`erlc` produced no {}", beam.display()),
            );
            return None;
        }
        Some(beam)
    }

    /// The environment every child gets: the **private** `epmd` port, so neither VM can
    /// touch (or be seen by) any other node on the machine.
    pub fn child_env(&self) -> Vec<(&'static str, String)> {
        vec![("ERL_EPMD_PORT", self.epmd_daemon.port.to_string())]
    }

    /// Reports — never asserts — whether the runtime's short name really appeared in *this*
    /// run's private `epmd`, and whether the machine's default `epmd` knows it instead.
    ///
    /// This is the one precondition the peer's `pong` cannot distinguish from the others: a
    /// runtime that ignores `ERL_EPMD_PORT` registers on 4369 and is invisible to the peer,
    /// which looks exactly like "the runtime does not serve `net_kernel`". Both facts end up
    /// in the failure message, so a red test is attributable rather than mysterious.
    pub fn report_registration(&self, name: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(port) = beam_dist::epmd::lookup_port_at(&self.epmd_daemon.addr, name.as_bytes())
            {
                eprintln!(
                    "note: the private `epmd` on {} resolved {name} to listener port {port}",
                    self.epmd_daemon.addr
                );
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            thread::sleep(POLL);
        }
        let elsewhere = beam_dist::epmd::lookup_port_at(
            beam_dist::EPMD_DEFAULT_ADDR,
            name.as_bytes(),
        );
        eprintln!(
            "warning: {name} never appeared in the private `epmd` on {} within 15s — is \
             ERL_EPMD_PORT honoured for the runtime's ALIVE2 registration? (default `epmd` on \
             {} says {:?})",
            self.epmd_daemon.addr,
            beam_dist::EPMD_DEFAULT_ADDR,
            elsewhere
        );
    }

    /// Starts the runtime under test as a `-sname` node on the `run` route.
    ///
    /// `peer_node` (when given) is the peer's full `node@host` atom, passed to the fixture
    /// as a **double-quoted** `run` argument — a char list, so no atom-charset question
    /// reaches the command line.
    pub fn spawn_runtime(
        &self,
        name: &str,
        cookie: &str,
        fixture: &Path,
        spec: &str,
        peer_node: Option<&str>,
    ) -> std::io::Result<ChildVm> {
        let mut extra: Vec<String> = Vec::new();
        if let Some(peer) = peer_node {
            extra.push(format!("\"{peer}\""));
        }
        self.spawn_runtime_with(name, cookie, fixture, spec, &extra)
    }

    /// As [`Harness::spawn_runtime`], but with the fixture's `run` arguments given verbatim
    /// (the caller applies the `run` argument grammar: a bare atom, `42`, or `"a string"`).
    pub fn spawn_runtime_with(
        &self,
        name: &str,
        cookie: &str,
        fixture: &Path,
        spec: &str,
        extra: &[String],
    ) -> std::io::Result<ChildVm> {
        let mut args: Vec<String> = vec![
            "-noshell".into(),
            "-sname".into(),
            name.to_string(),
            "-setcookie".into(),
            cookie.to_string(),
            "-pa".into(),
            self.scratch.path().display().to_string(),
            "run".into(),
            fixture.display().to_string(),
            spec.to_string(),
        ];
        args.extend(extra.iter().cloned());
        ChildVm::spawn(
            "runtime",
            &self.setsid,
            &self.kill,
            Path::new(BINARY),
            &args,
            &self.child_env(),
            Some(self.scratch.path()),
        )
    }

    /// Starts the runtime under test as a `-sname` node on the **`-eval`** route.
    ///
    /// This is the route `tools/differential`'s distribution corpus uses
    /// (`-noshell -sname <name> -eval <script>`), so a suite that reproduces one of its
    /// shapes drives it exactly as the gate does.
    pub fn spawn_runtime_eval(
        &self,
        name: &str,
        cookie: &str,
        eval: &str,
    ) -> std::io::Result<ChildVm> {
        let args: Vec<String> = vec![
            "-noshell".into(),
            "-sname".into(),
            name.to_string(),
            "-setcookie".into(),
            cookie.to_string(),
            "-eval".into(),
            eval.to_string(),
        ];
        ChildVm::spawn(
            "runtime-eval",
            &self.setsid,
            &self.kill,
            Path::new(BINARY),
            &args,
            &self.child_env(),
            Some(self.scratch.path()),
        )
    }

    /// Starts the system `erl` peer with `eval` as its `-eval` expression, `@TARGET@`
    /// replaced by `target` (the runtime's full `node@host` atom).
    pub fn spawn_peer(
        &self,
        name: &str,
        cookie: &str,
        eval: &str,
        target: &str,
    ) -> std::io::Result<ChildVm> {
        let eval = eval.replace("@TARGET@", target);
        let args: Vec<String> = vec![
            "-sname".into(),
            name.to_string(),
            "-setcookie".into(),
            cookie.to_string(),
            "-noshell".into(),
            "-eval".into(),
            eval,
        ];
        ChildVm::spawn(
            "peer",
            &self.setsid,
            &self.kill,
            &self.oracle,
            &args,
            &self.child_env(),
            Some(self.scratch.path()),
        )
    }
}

/// `name` on `$PATH` (**filesystem only** — the tool is never run to detect it), or a loud
/// skip naming it.
pub fn tool(banner: &str, name: &str) -> Option<PathBuf> {
    match find_on_path(name) {
        Some(path) => Some(path),
        None => {
            skip_loudly(
                banner,
                &format!("no `{name}` on $PATH (this suite drives a real two-VM pair)"),
            );
            None
        }
    }
}

/// The short hostname Erlang itself derives for a `-sname` node, derived the way
/// [`beam_rt`]'s `dist::local_hostname` derives it: `/proc/sys/kernel/hostname`, then
/// `$HOSTNAME`, then the literal `localhost`. Deliberately **not** `hostname(1)` — the
/// harness never shells out to discover a fact the runtime reads from a file.
pub fn local_hostname() -> String {
    if let Ok(contents) = std::fs::read_to_string("/proc/sys/kernel/hostname") {
        let host = contents.trim();
        if !host.is_empty() {
            return host.to_string();
        }
    }
    match std::env::var("HOSTNAME") {
        Ok(host) if !host.is_empty() => host,
        _ => "localhost".to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// Unique names
// ---------------------------------------------------------------------------------------

/// A per-call counter, so two tests (which cargo may run concurrently) never pick the same
/// node names or cookie.
static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

/// A process-unique, **lowercase-alphanumeric** node-name fragment: `-sname` lowercases and
/// validates the name, and a leftover node from an earlier run must never collide.
pub fn unique(tag: char) -> String {
    format!(
        "twovm{tag}{}{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    )
}

/// A process-unique cookie (never the developer's `~/.erlang.cookie`: every node is given
/// `-setcookie` explicitly).
pub fn unique_cookie() -> String {
    format!(
        "ck{}{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    )
}

// ---------------------------------------------------------------------------------------
// The test's own scratch directory
// ---------------------------------------------------------------------------------------

/// A private directory under `$TMPDIR`, unique per call (label + pid + sequence), removed on
/// drop.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    /// Creates `$TMPDIR/beam_rt_two_vm_<label>_<pid>_<n>`, or `None` after a loud skip.
    pub fn new(banner: &str, label: &str) -> Option<Scratch> {
        let dir = std::env::temp_dir().join(format!(
            "beam_rt_two_vm_{label}_{}_{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::SeqCst)
        ));
        match std::fs::create_dir_all(&dir) {
            Ok(()) => Some(Scratch { dir }),
            Err(error) => {
                skip_loudly(
                    banner,
                    &format!(
                        "cannot create the scratch directory {}: {error}",
                        dir.display()
                    ),
                );
                None
            }
        }
    }

    /// The directory itself (the fixture's output directory and the children's cwd).
    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// ---------------------------------------------------------------------------------------
// The private `epmd`
// ---------------------------------------------------------------------------------------

/// A private `epmd` daemon on a private port, stopped on drop.
///
/// The port comes from an ephemeral bind that is released immediately, so it collides with
/// no other test (nor with a developer's own `epmd` on 4369), and every child is handed
/// `ERL_EPMD_PORT=<port>` so both VMs register with, and resolve through, *this* daemon.
pub struct PrivateEpmd {
    /// The port the daemon listens on.
    pub port: u16,
    /// `127.0.0.1:<port>` — the address every probe and lookup uses.
    pub addr: String,
    /// The resolved `kill`, used to reclaim the detached daemon.
    pub kill: PathBuf,
}

impl PrivateEpmd {
    /// Starts `epmd -daemon -port <free port>` under `setsid -w` with `ERL_EPMD_PORT` set
    /// and its cwd inside `scratch`, then waits (bounded) for it to accept a connection.
    ///
    /// `None` after a loud skip when no free port is available, the daemon fails to start,
    /// or it never accepts a connection.
    pub fn start(
        banner: &str,
        epmd: &Path,
        setsid: &Path,
        kill: &Path,
        scratch: &Path,
    ) -> Option<PrivateEpmd> {
        let Some(port) = free_port() else {
            skip_loudly(banner, "no free loopback port for the private `epmd`");
            return None;
        };
        let addr = format!("127.0.0.1:{port}");
        let output = Command::new(setsid)
            .arg("-w")
            .arg(epmd)
            .arg("-daemon")
            .arg("-port")
            .arg(port.to_string())
            .env("ERL_EPMD_PORT", port.to_string())
            .current_dir(scratch)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output();
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                skip_loudly(
                    banner,
                    &format!(
                        "`{} -daemon -port {port}` exited with {}: {}",
                        epmd.display(),
                        output.status,
                        String::from_utf8_lossy(&output.stderr).trim()
                    ),
                );
                return None;
            }
            Err(error) => {
                skip_loudly(
                    banner,
                    &format!("cannot run `{}`: {error}", epmd.display()),
                );
                return None;
            }
        }
        let daemon = PrivateEpmd {
            port,
            addr,
            kill: kill.to_path_buf(),
        };
        // `epmd -daemon` returns as soon as the daemon parent has forked, so readiness is
        // "it accepts a connection" rather than "the command returned".
        let deadline = Instant::now() + EPMD_START_TIMEOUT;
        while Instant::now() < deadline {
            if TcpStream::connect(&daemon.addr).is_ok() {
                return Some(daemon);
            }
            thread::sleep(POLL);
        }
        skip_loudly(
            banner,
            &format!(
                "the private `epmd` never accepted a connection on {} within {EPMD_START_TIMEOUT:?}",
                daemon.addr
            ),
        );
        // A daemon that came up too late (or on a port we cannot reach) must still not be
        // left running.
        daemon.stop();
        None
    }

    /// Stops the daemon and proves its port is closed.
    ///
    /// A daemonized `epmd` detaches into its own session, so the group kill that reclaims
    /// the VMs cannot reach it — and OTP's daemon refuses `KILL_REQ`/`STOP_REQ` from an
    /// ordinary client (measured: `epmd -stop`/`-kill` answer `local epmd responded with
    /// <>` unless it was started with `-relaxed_command_check`). So the daemon is found by
    /// its **own argv** ([`epmd_pid_on_port`], which only our private `-port <port>` matches)
    /// and signalled by pid.
    pub fn stop(&self) {
        // Best effort: have the daemon drop every registration first.
        if let Ok(mut client) = beam_dist::epmd::connect(&self.addr) {
            let _ = client.kill();
        }
        let Some(pid) = epmd_pid_on_port(self.port) else {
            if port_closed(&self.addr) {
                return;
            }
            eprintln!(
                "warning: `epmd` on {} accepts connections but no daemon with `-port {}` was \
                 found in /proc to stop",
                self.addr, self.port
            );
            return;
        };
        signal(&self.kill, &["-TERM", &pid.to_string()]);
        if wait_for_port_closed(&self.addr, EPMD_STOP_TIMEOUT) {
            return;
        }
        signal(&self.kill, &["-KILL", &pid.to_string()]);
        if wait_for_port_closed(&self.addr, EPMD_STOP_TIMEOUT) {
            return;
        }
        eprintln!(
            "warning: the private `epmd` (pid {pid}) still holds {} after TERM and KILL",
            self.addr
        );
    }
}

impl Drop for PrivateEpmd {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A free loopback port: bound as `:0`, read back, and released at once.
pub fn free_port() -> Option<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").ok()?;
    let port = listener.local_addr().ok()?.port();
    drop(listener);
    Some(port)
}

/// Whether nothing accepts connections on `addr` any more.
fn port_closed(addr: &str) -> bool {
    TcpStream::connect(addr).is_err()
}

/// Polls [`port_closed`] for up to `timeout`.
fn wait_for_port_closed(addr: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if port_closed(addr) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
}

/// The pid of the `epmd` daemon started with `-port <port>`, found by reading
/// `/proc/<pid>/cmdline` — a plain filesystem scan, so only *this* run's private daemon (the
/// only one whose argv carries the port) can match.
pub fn epmd_pid_on_port(port: u16) -> Option<u32> {
    let wanted = port.to_string();
    let entries = std::fs::read_dir("/proc").ok()?;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|byte| *byte == 0)
            .filter(|arg| !arg.is_empty())
            .map(|arg| String::from_utf8_lossy(arg).into_owned())
            .collect();
        let Some(program) = args.first() else {
            continue;
        };
        let base = program.rsplit('/').next().unwrap_or(program);
        if base != "epmd" {
            continue;
        }
        if args
            .windows(2)
            .any(|pair| pair[0] == "-port" && pair[1] == wanted)
        {
            return Some(pid);
        }
    }
    None
}

/// Runs `<kill> <args…>` with the **resolved** `kill` binary (never a shell builtin).
pub fn signal(kill: &Path, args: &[&str]) {
    let _ = Command::new(kill)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------------------
// A VM child in its own session
// ---------------------------------------------------------------------------------------

/// One VM (`beam_rt` or the system `erl`) running as `setsid -w <program> …`: its own
/// session, hence its own process group, so its `SIGUSR1`/`SIGTERM` traffic can never reach
/// the test binary or a sibling test.
///
/// The child's stdout is drained line by line into a channel (so a `READY`/`PONG`/`GOT` line
/// can be awaited as it appears) *and* kept in a transcript for failure messages; stderr is
/// collected the same way. Drop kills the **group** (`TERM`, a bounded grace, then `KILL`)
/// and reaps, so nothing survives a failed assertion.
pub struct ChildVm {
    /// The exact command line, so a failure names what was run.
    pub command: String,
    /// The `setsid -w …` process — with `setsid`'s direct exec, the VM itself.
    child: Child,
    /// The process group to signal: this child's pid when `/proc/<pid>/stat` says it really
    /// leads its own group (`setsid` execs directly, so it does), else `None`.
    pgid: Option<u32>,
    /// The resolved `kill` used for the group signals.
    kill: PathBuf,
    /// The child's stdout, one line at a time.
    lines: Receiver<String>,
    /// Every stdout line so far (failure diagnostics).
    transcript: Arc<Mutex<Vec<String>>>,
    /// Every stderr line so far (failure diagnostics).
    stderr: Arc<Mutex<String>>,
}

impl ChildVm {
    /// Spawns `setsid -w <program> <args…>` with `env` applied and `cwd` set, and starts the
    /// two reader threads.
    pub fn spawn(
        label: &str,
        setsid: &Path,
        kill: &Path,
        program: &Path,
        args: &[String],
        env: &[(&str, String)],
        cwd: Option<&Path>,
    ) -> std::io::Result<ChildVm> {
        let mut command = Command::new(setsid);
        command.arg("-w").arg(program);
        command.args(args);
        for (key, value) in env {
            command.env(key, value);
        }
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let command_line = format!(
            "setsid -w {} {}",
            program.display(),
            args.iter()
                .map(|arg| if arg.contains(' ') || arg.contains('\n') {
                    format!("{arg:?}")
                } else {
                    arg.clone()
                })
                .collect::<Vec<_>>()
                .join(" ")
        );
        let mut child = command.spawn()?;
        let pid = child.id();
        // `setsid` calls `setsid(2)` and execs the program, so the child *is* the session and
        // group leader; read the group back from /proc rather than assuming it (a `setsid`
        // that forked would leave `pgid == pid` false, and then only the process itself is
        // signalled — never an unrelated group).
        let pgid = wait_for_group_leader(pid);
        if pgid.is_none() {
            eprintln!(
                "warning: {label}: `setsid -w` did not leave pid {pid} as its own group leader; \
                 the group signal will be skipped"
            );
        }
        let stdout = child.stdout.take().expect("the child's stdout is piped");
        let stderr_pipe = child.stderr.take().expect("the child's stderr is piped");

        let transcript = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&transcript);
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if let Ok(mut buffer) = sink.lock() {
                    buffer.push(line.clone());
                }
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        thread::spawn(move || {
            for line in BufReader::new(stderr_pipe).lines() {
                let Ok(line) = line else { break };
                match sink.lock() {
                    Ok(mut buffer) => {
                        buffer.push_str(&line);
                        buffer.push('\n');
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(ChildVm {
            command: command_line,
            child,
            pgid,
            kill: kill.to_path_buf(),
            lines,
            transcript,
            stderr,
        })
    }

    /// Waits (bounded) for a stdout line satisfying `pred`, discarding earlier lines.
    ///
    /// Returns `None` on the timeout, or when the child's stdout reached EOF first.
    pub fn wait_for<F: Fn(&str) -> bool>(&self, pred: F, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) if pred(&line) => return Some(line),
                Ok(_) => {}
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => return None,
            }
        }
    }

    /// Every stdout line so far (failure diagnostics).
    pub fn transcript(&self) -> Vec<String> {
        self.transcript
            .lock()
            .map(|buffer| buffer.clone())
            .unwrap_or_default()
    }

    /// The stderr collected so far (failure diagnostics).
    pub fn stderr_text(&self) -> String {
        self.stderr.lock().map(|text| text.clone()).unwrap_or_default()
    }

    /// Polls for the child's exit, bounded by `timeout`; the child is **reaped** when it has
    /// exited.
    pub fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => return Some(status),
                Ok(None) => {}
                Err(_) => return None,
            }
            if Instant::now() >= deadline {
                return None;
            }
            thread::sleep(POLL);
        }
    }

    /// Signals `signal` to this child's **process group** (a no-op when the group could not
    /// be established; see [`ChildVm::pgid`]).
    pub fn signal_group(&self, signal: &str) {
        let Some(pgid) = self.pgid else { return };
        let _ = Command::new(&self.kill)
            .arg(signal)
            .arg("--")
            .arg(format!("-{pgid}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    /// Terminates the whole group: `TERM`, a bounded grace, then `KILL`.
    pub fn kill_group(&mut self) {
        if self.wait_exit(Duration::ZERO).is_some() {
            return;
        }
        self.signal_group("-TERM");
        if self.wait_exit(KILL_GRACE).is_some() {
            return;
        }
        self.signal_group("-KILL");
        let _ = self.wait_exit(KILL_GRACE);
    }
}

impl Drop for ChildVm {
    fn drop(&mut self) {
        self.kill_group();
        // Belt and braces: a child whose group could not be established is killed directly,
        // and a child that already exited is reaped here.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls `/proc/<pid>/stat` briefly for a process that really leads its own group, and
/// answers that group id.
///
/// `setsid` execs the program it was given, so the child *is* the session leader and
/// `pgrp == pid`; the poll exists only to let that change land after the spawn.
fn wait_for_group_leader(pid: u32) -> Option<u32> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(pgid) = leader_pgid(pid) {
            return Some(pgid);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(5));
    }
}

/// `pid`'s process group id when it leads that group, read from `/proc/<pid>/stat`
/// (field 5, `pgrp`, after the parenthesised command that may itself contain spaces).
fn leader_pgid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let close = stat.rfind(')')?;
    let mut fields = stat[close + 1..].split_whitespace();
    let _state = fields.next()?;
    let _ppid = fields.next()?;
    let pgrp: u32 = fields.next()?.parse().ok()?;
    (pgrp == pid).then_some(pgrp)
}
