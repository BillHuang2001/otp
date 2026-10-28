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
//! **The staged-pristine-Cowboy harness** — everything `cowboy_serve.rs` and
//! `cowboy_static_serve.rs` do in common, and nothing either of them pins.
//!
//! Both suites exercise the same third-party stack the same way: the read-only cowboy checkout is
//! **staged** into a scratch directory (never written into), `make` builds it with `erlang.mk`
//! (which git-fetches and builds its own `cowlib`/`ranch` dependencies), cowboy's own
//! `examples/hello_world` sources are compiled **unpatched** by the system `erlc` next to the
//! suite's own harness module, and the server then runs as a real **scheduled process** on the
//! built `beam_rt` binary through the `run` action:
//!
//! ```text
//! beam_rt -noshell -pa <deps/ranch/ebin> -pa <deps/cowlib/ebin> -pa <stage>/ebin -pa <hw_ebin> \
//!         run <hw_ebin>/<harness>.beam <harness>:start/0
//! ```
//!
//! The requests are driven from the suite with a plain `std::net::TcpStream`, so nothing of the
//! server side is stubbed: `ranch`'s acceptors, `cowboy_clear`'s HTTP/1.1 protocol,
//! `cowboy_router`'s dispatch, the handler and the response writer all run on the Rust runtime.
//!
//! # What is here, and what stays in the suites
//!
//! * [`Stage::build`] — the checkout checks, the toolchain probe, the staged copy and the
//!   `erlang.mk` build. Every environment precondition is a **loud skip** ([`Skip`]), never a
//!   failure.
//! * [`Stage::compile`] — the unpatched example sources plus one [`Harness`] module, in a
//!   `hw_ebin` directory of its own.
//! * [`Stage::command`] — the serving command: the four `-pa` directories in their tested order,
//!   the `run` action and `$CW_PORT`.
//! * [`Serving`] — the child server: drained pipes, the bounded `CW_READY` wait, the raw-socket
//!   request helpers and a kill-and-reap `Drop`.
//! * [`Response`]/[`parse_response`] and the small plumbing ([`run_bounded`], [`free_port`],
//!   [`copy_tree`], [`skip`], [`describe_code`], [`tail`]).
//!
//! The assertions — which status line, which header, which body — belong to the suites; nothing
//! here pins a response.
//!
//! # The skip contract
//!
//! An environment that cannot support a run is a **loud skip**: a missing checkout, a missing
//! `erl`/`erlc`/`make`/`git`, a dependency fetch that cannot reach the network, a port that
//! cannot be bound, a harness that does not compile, or a server that never prints `CW_READY`.
//! Each prints one `skipping <suite>: <reason>` line (plus the crate's standard skip banner) and
//! returns. Nothing else is skipped: once `CW_READY` is printed, every assertion is a hard
//! requirement.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::support;

/// The read-only cowboy checkout the suites stage from. It is **read**, never written.
pub const CHECKOUT: &str = "/home/bill/Source/cowboy";

/// The entries copied out of [`CHECKOUT`] into the scratch stage. `ebin` is prebuilt but `make`
/// regenerates it, and this checkout has **no** `include/` directory, so none is required.
pub const STAGED_ENTRIES: &[&str] = &[
    "src",
    "ebin",
    "examples",
    "Makefile",
    "erlang.mk",
    "plugins.mk",
    "rebar.config",
];

/// The entries that must be present for the staged build to be possible at all; the others are
/// copied when they exist.
pub const REQUIRED_ENTRIES: &[&str] = &["src", "examples", "Makefile", "erlang.mk"];

/// `make`'s bound: it git-fetches `cowlib` and `ranch` and compiles both plus cowboy.
pub const MAKE_TIMEOUT: Duration = Duration::from_secs(900);

/// One `erlc` invocation's bound.
pub const COMPILE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the server may take to print `CW_READY` (a boot plus `ranch`'s listener start on a
/// loaded host).
pub const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// One HTTP exchange's socket timeout.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// The poll interval of every bounded wait here.
pub const POLL: Duration = Duration::from_millis(10);

// ---------------------------------------------------------------------------------------
// The skip contract
// ---------------------------------------------------------------------------------------

/// Prints the one skip line and the crate's standard banner, and lets the test pass.
pub fn skip(banner: &str, reason: &str) {
    skip_note(banner, reason);
    support::skip_loudly(banner, reason);
}

/// Prints only the one `skipping <banner>: <reason>` line — for a path where the banner has
/// already been printed.
pub fn skip_note(banner: &str, reason: &str) {
    eprintln!("skipping {banner}: {reason}");
}

/// A [`Stage::build`] precondition this environment cannot satisfy. Both halves are **loud
/// skips**; they differ only in which lines the caller still owes, because one helper (the
/// scratch-directory one) prints the banner itself.
pub enum Skip {
    /// Nothing has been printed yet: the caller owes both the one-line note and the banner.
    Unreported(String),
    /// The full banner has already been printed: the caller owes only the one-line note.
    BannerOnly(String),
}

impl Skip {
    /// Prints whatever is still owed and lets the test pass as a skip.
    pub fn report(&self, banner: &str) {
        match self {
            Skip::Unreported(reason) => skip(banner, reason),
            Skip::BannerOnly(reason) => skip_note(banner, reason),
        }
    }
}

// ---------------------------------------------------------------------------------------
// The staged, built checkout
// ---------------------------------------------------------------------------------------

/// A staged, built cowboy checkout: the scratch stage, the resolved system `erlc` and the three
/// `ebin` directories `make` produced.
pub struct Stage {
    /// The scratch stage — held here so it outlives the server and is removed on drop.
    stage: support::Scratch,
    /// The system `erlc` cowboy's own unpatched example sources are compiled with.
    erlc: PathBuf,
    /// `<stage>/deps/cowlib/ebin`.
    cowlib_ebin: PathBuf,
    /// `<stage>/deps/ranch/ebin`.
    ranch_ebin: PathBuf,
    /// `<stage>/ebin` — cowboy itself.
    cowboy_ebin: PathBuf,
}

impl Stage {
    /// Stages [`CHECKOUT`] into a fresh scratch directory and builds it with `erlang.mk` (which
    /// git-fetches and builds `cowlib` and `ranch`).
    ///
    /// Every `Err` is a **loud-skip** reason — a missing checkout, a missing
    /// `erl`/`erlc`/`make`/`git`, a dependency fetch that cannot reach the network, or a `make`
    /// that produced no `ebin` directory. A failure *after* staging is the suites' business, not
    /// this one's.
    pub fn build(banner: &str) -> Result<Stage, Skip> {
        let checkout = Path::new(CHECKOUT);
        if !checkout.is_dir() {
            return Err(Skip::Unreported(format!(
                "the read-only cowboy checkout {CHECKOUT} is absent"
            )));
        }
        let missing: Vec<&str> = REQUIRED_ENTRIES
            .iter()
            .copied()
            .filter(|entry| !checkout.join(entry).exists())
            .collect();
        if !missing.is_empty() {
            return Err(Skip::Unreported(format!(
                "{CHECKOUT} does not look like a cowboy checkout (missing {})",
                missing.join(", ")
            )));
        }
        let Some(oracle) = support::resolve_oracle() else {
            return Err(Skip::Unreported(
                "no `erl` on $PATH (and no executable $ERTS_ORACLE)".to_string(),
            ));
        };
        let Some(erlc) = support::resolve_erlc(&oracle) else {
            return Err(Skip::Unreported(
                "no `erlc` beside the oracle and none on $PATH".to_string(),
            ));
        };
        let (Some(make), Some(git)) = (
            support::find_on_path("make"),
            support::find_on_path("git"),
        ) else {
            return Err(Skip::Unreported(
                "`make` and `git` are both needed (erlang.mk git-fetches cowlib and ranch)"
                    .to_string(),
            ));
        };
        let Some(stage) = support::Scratch::new(banner, "cowboy_stage") else {
            // `Scratch::new` printed the banner for this one; only the note is still owed.
            return Err(Skip::BannerOnly(
                "cannot create a scratch directory under $TMPDIR".to_string(),
            ));
        };

        // 1. Stage the checkout: it is never written to.
        for entry in STAGED_ENTRIES {
            let from = checkout.join(entry);
            if !from.exists() {
                // A checkout without the optional entries still builds; say so rather than hide it.
                eprintln!("note: {banner}: {CHECKOUT}/{entry} does not exist and is not staged");
                continue;
            }
            let to = stage.dir().join(entry);
            if let Err(error) = copy_tree(&from, &to) {
                return Err(Skip::Unreported(format!(
                    "cannot stage {from:?} → {to:?}: {error}"
                )));
            }
        }

        // 2. Build it with erlang.mk (which fetches cowlib and ranch).
        let mut make_command = Command::new(&make);
        make_command.arg("-C").arg(stage.dir());
        // The staged tree is not a git repository; erlang.mk only needs `git` to fetch deps.
        let built = match run_bounded(make_command, MAKE_TIMEOUT) {
            Ok(built) => built,
            Err(error) => {
                return Err(Skip::Unreported(format!(
                    "cannot run `make` in the staged copy: {error}"
                )))
            }
        };
        if built.timed_out {
            return Err(Skip::Unreported(format!(
                "`make` in the staged copy outlived the {}s bound",
                MAKE_TIMEOUT.as_secs()
            )));
        }
        if built.code != Some(0) {
            return Err(Skip::Unreported(format!(
                "`make` in the staged copy failed ({}; the dependency fetch needs the network)\n--- tail ---\n{}",
                describe_code(built.code),
                tail(&built.stderr, 20)
            )));
        }
        let cowlib_ebin = stage.dir().join("deps/cowlib/ebin");
        let ranch_ebin = stage.dir().join("deps/ranch/ebin");
        let cowboy_ebin = stage.dir().join("ebin");
        for dir in [&cowlib_ebin, &ranch_ebin, &cowboy_ebin] {
            if !dir.is_dir() {
                return Err(Skip::Unreported(format!(
                    "`make` produced no {} (git: {})",
                    dir.display(),
                    git.display()
                )));
            }
        }
        Ok(Stage {
            stage,
            erlc,
            cowlib_ebin,
            ranch_ebin,
            cowboy_ebin,
        })
    }

    /// Compiles cowboy's own **unpatched** `examples/hello_world/src/*.erl` plus `harness` into a
    /// new `hw_ebin` directory, and answers that directory with the harness `.beam`.
    ///
    /// An `Err` here is a **loud-skip** reason too: a staged tree that cannot be read, a source
    /// set that is empty, or an `erlc` that cannot run or rejects the sources (which on a machine
    /// that staged fine means the toolchain, not this repository).
    pub fn compile(&self, harness: &Harness) -> Result<Served, String> {
        let hw_ebin = self.stage.dir().join("hw_ebin");
        std::fs::create_dir_all(&hw_ebin)
            .map_err(|error| format!("cannot create {}: {error}", hw_ebin.display()))?;
        let harness_source = hw_ebin.join(format!("{}.erl", harness.module));
        std::fs::write(&harness_source, harness.source)
            .map_err(|error| format!("cannot write {}: {error}", harness_source.display()))?;

        // Cowboy's own handler sources, compiled exactly as they are published.
        let example_src = self.stage.dir().join("examples/hello_world/src");
        let mut sources: Vec<PathBuf> = Vec::new();
        match std::fs::read_dir(&example_src) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "erl") => {
                            sources.push(entry.path());
                        }
                        Ok(_) => {}
                        Err(error) => {
                            return Err(format!("cannot read {}: {error}", example_src.display()))
                        }
                    }
                }
            }
            Err(error) => return Err(format!("cannot read {}: {error}", example_src.display())),
        }
        sources.sort();
        if sources.is_empty() {
            return Err(format!("{} holds no `.erl` sources", example_src.display()));
        }

        let mut erlc_command = Command::new(&self.erlc);
        erlc_command.arg("-o").arg(&hw_ebin);
        erlc_command.args(&sources);
        erlc_command.arg(&harness_source);
        let compiled = match run_bounded(erlc_command, COMPILE_TIMEOUT) {
            Ok(compiled) => compiled,
            Err(error) => {
                return Err(format!("cannot run `{}`: {error}", self.erlc.display()))
            }
        };
        if compiled.timed_out || compiled.code != Some(0) {
            return Err(format!(
                "the system `erlc` could not compile cowboy's hello_world example ({}; {} source(s))\n--- tail ---\n{}",
                describe_code(compiled.code),
                sources.len(),
                tail(&compiled.stderr, 20)
            ));
        }
        // Every compiled source must have produced its own image, the harness included.
        for source in &sources {
            let module = source
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
                .unwrap_or_default();
            let beam = hw_ebin.join(format!("{module}.beam"));
            if !beam.is_file() {
                return Err(format!("`erlc` produced no {}", beam.display()));
            }
        }
        let beam = hw_ebin.join(format!("{}.beam", harness.module));
        if !beam.is_file() {
            return Err(format!("`erlc` produced no {}", beam.display()));
        }
        Ok(Served {
            ebin: hw_ebin,
            beam,
            entry: harness.entry.to_string(),
        })
    }

    /// The serving command: the four `-pa` directories in their tested order, the `run` action on
    /// the harness, and `$CW_PORT`. A suite that needs more sets it on the returned command.
    pub fn command(&self, served: &Served, port: u16) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_beam_rt"));
        for dir in [
            &self.ranch_ebin,
            &self.cowlib_ebin,
            &self.cowboy_ebin,
            &served.ebin,
        ] {
            command.arg("-pa").arg(dir);
        }
        command
            .arg("run")
            .arg(&served.beam)
            .arg(&served.entry)
            .env("CW_PORT", port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }
}

/// The suite's own harness module: its name, its source, and the `M:F/A` the `run` action enters.
///
/// A harness patches nothing — it starts cowboy's application (which is what makes `ranch`'s
/// listener supervisor available), compiles the suite's dispatch table, starts a clear-text
/// listener on `$CW_PORT`, announces itself with `CW_READY` and then blocks for ever.
pub struct Harness {
    /// The module name, without `.erl`.
    pub module: &'static str,
    /// The source, written into `hw_ebin/<module>.erl` and compiled there.
    pub source: &'static str,
    /// The export the `run` action enters, as `module:function/arity`.
    pub entry: &'static str,
}

/// A compiled harness: the directory holding it (the last `-pa`), its `.beam`, and the entry point.
pub struct Served {
    /// `hw_ebin` — the harness's own directory, handed to the server as the last `-pa`.
    pub ebin: PathBuf,
    /// The harness `.beam` the `run` action loads.
    pub beam: PathBuf,
    /// The `M:F/A` the `run` action enters.
    pub entry: String,
}

// ---------------------------------------------------------------------------------------
// The served response, parsed the way an HTTP/1.1 client reads it
// ---------------------------------------------------------------------------------------

/// One HTTP/1.1 response read from a socket: the raw bytes (for diagnostics), the status line and
/// the headers, plus everything after the header block as `body`.
pub struct Response {
    /// Everything the server sent, verbatim — what every failure message prints.
    pub raw: String,
    /// The status line, without its `\r\n`.
    pub status_line: String,
    /// The header fields, name lowercased.
    pub headers: Vec<(String, String)>,
    /// The bytes after the header block (for a `HEAD` answer: empty).
    pub body: Vec<u8>,
}

impl Response {
    /// A header's value by its **lowercased** name.
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone())
    }
}

/// Splits raw bytes into a [`Response`]: the header block ends at the first blank line.
pub fn parse_response(bytes: &[u8]) -> Response {
    let text = String::from_utf8_lossy(bytes).into_owned();
    let split = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| (at, at + 4));
    let (head, body) = match split {
        Some((head_end, body_start)) => (&bytes[..head_end], bytes[body_start..].to_vec()),
        None => (bytes, Vec::new()),
    };
    let head = String::from_utf8_lossy(head).into_owned();
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Response {
        raw: text,
        status_line,
        headers,
        body,
    }
}

// ---------------------------------------------------------------------------------------
// The serving runtime child — killed and reaped on every exit path
// ---------------------------------------------------------------------------------------

/// A `beam_rt` child serving a suite's harness on its own ephemeral port.
///
/// Its stdout is drained line by line into a channel (`CW_READY` is awaited as it appears) and
/// kept in a transcript; stderr is drained in the background so the child can never block on a
/// full pipe. `Drop` kills and reaps it, so a failed assertion (or an early `return` from a skip)
/// never leaves a listener behind for the next run.
pub struct Serving {
    /// The child process.
    child: Child,
    /// The port the harness was told to listen on.
    port: u16,
    /// The child's stdout, one line at a time.
    lines: Receiver<String>,
    /// The child's stderr, one line at a time (drained on demand, so the pipe never fills).
    err_lines: Receiver<String>,
    /// Every stdout line so far (failure diagnostics).
    pub transcript: Vec<String>,
    /// Every stderr line so far (failure diagnostics).
    stderr: Vec<String>,
}

impl Serving {
    /// Starts a child for `command` (already configured by [`Stage::command`]), with both pipes
    /// drained by background threads.
    pub fn spawn(mut command: Command, port: u16) -> std::io::Result<Serving> {
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().expect("the child's stdout is piped");
        let stderr = child.stderr.take().expect("the child's stderr is piped");
        let (tx, lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let (err_tx, err_lines) = mpsc::channel();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines() {
                let Ok(line) = line else { break };
                if err_tx.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Serving {
            child,
            port,
            lines,
            err_lines,
            transcript: Vec::new(),
            stderr: Vec::new(),
        })
    }

    /// Waits (bounded) for the harness's `CW_READY` line, discarding anything before it.
    ///
    /// Returns `Err(reason)` — a **skip** reason, never a failure — when the child exits first or
    /// the bound expires, so an environment that cannot host the server does not fail the suite.
    pub fn wait_ready(&mut self, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!(
                    "the serving runtime did not print CW_READY within {}s\n--- stdout ---\n{}\n--- stderr ---\n{}",
                    timeout.as_secs(),
                    self.transcript.join("\n"),
                    self.stderr_text()
                ));
            }
            match self.lines.recv_timeout(remaining) {
                Ok(line) => {
                    let ready = line.trim() == "CW_READY";
                    self.transcript.push(line);
                    if ready {
                        return Ok(());
                    }
                }
                Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                    let status = self.child.try_wait().ok().flatten();
                    return Err(format!(
                        "the serving runtime {} before it printed CW_READY\n--- stdout ---\n{}\n--- stderr ---\n{}",
                        match status {
                            Some(status) => format!("exited ({status})"),
                            None => "closed its stdout".to_string(),
                        },
                        self.transcript.join("\n"),
                        self.stderr_text()
                    ));
                }
            }
        }
    }

    /// Everything the child has written to stderr so far.
    pub fn stderr_text(&mut self) -> String {
        while let Ok(line) = self.err_lines.try_recv() {
            self.stderr.push(line);
        }
        self.stderr.join("\n")
    }

    /// Whether the serving child is still running (a serve loop never exits by itself).
    pub fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends `request` on a fresh connection and reads the answer to EOF.
    ///
    /// The read is bounded by [`HTTP_TIMEOUT`], so a response that never completes is a visible
    /// failure instead of a hang.
    pub fn exchange(&mut self, request: &str) -> Response {
        let mut stream = self.connect();
        stream
            .write_all(request.as_bytes())
            .unwrap_or_else(|error| panic!("cannot write the request: {error}\n{request:?}"));
        stream
            .flush()
            .unwrap_or_else(|error| panic!("cannot flush the request: {error}"));
        let mut bytes = Vec::new();
        stream.read_to_end(&mut bytes).unwrap_or_else(|error| {
            panic!("cannot read the response ({error}; {} byte(s) so far)\n{request:?}", bytes.len())
        });
        parse_response(&bytes)
    }

    /// Sends `request` `times` times over **one** connection, reading each answer from the
    /// `content-length` the server declared (so the connection really is reused, not re-dialled).
    ///
    /// Returns one `"<status> <body>"` summary per answer.
    pub fn exchange_keep_alive(&mut self, request: &str, times: usize) -> Vec<String> {
        let mut stream = self.connect();
        let mut reader = BufReader::new(stream.try_clone().expect("the socket is cloneable"));
        let mut answers = Vec::new();
        for _ in 0..times {
            stream
                .write_all(request.as_bytes())
                .unwrap_or_else(|error| panic!("cannot write a keep-alive request: {error}"));
            stream
                .flush()
                .unwrap_or_else(|error| panic!("cannot flush a keep-alive request: {error}"));
            let mut status_line = String::new();
            let read = reader
                .read_line(&mut status_line)
                .unwrap_or_else(|error| panic!("cannot read a keep-alive status line: {error}"));
            assert!(
                read > 0,
                "the connection was closed before answer {}; answers so far: {answers:?}",
                answers.len() + 1
            );
            let mut headers = Vec::new();
            loop {
                let mut line = String::new();
                let read = reader
                    .read_line(&mut line)
                    .unwrap_or_else(|error| panic!("cannot read a keep-alive header: {error}"));
                if read == 0 {
                    break;
                }
                let line = line.trim_end_matches(['\r', '\n']).to_string();
                if line.is_empty() {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
                }
            }
            let length: usize = headers
                .iter()
                .find(|(name, _)| name == "content-length")
                .map(|(_, value)| value.parse().expect("content-length is a number"))
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            reader
                .read_exact(&mut body)
                .unwrap_or_else(|error| panic!("cannot read a keep-alive body: {error}"));
            let code = status_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string();
            answers.push(format!("{code} {}", String::from_utf8_lossy(&body)));
        }
        answers
    }

    /// A connection to the serving child, bounded by [`HTTP_TIMEOUT`] in both directions.
    pub fn connect(&self) -> TcpStream {
        let address = format!("127.0.0.1:{}", self.port);
        let stream = TcpStream::connect_timeout(
            &address.parse().expect("the loopback address parses"),
            HTTP_TIMEOUT,
        )
        .unwrap_or_else(|error| panic!("cannot connect to {address}: {error}"));
        stream
            .set_read_timeout(Some(HTTP_TIMEOUT))
            .expect("a read timeout is settable");
        stream
            .set_write_timeout(Some(HTTP_TIMEOUT))
            .expect("a write timeout is settable");
        stream
    }

    /// Kills the child (`KILL`, never a signal it could ignore) and reaps it, so no listener
    /// outlives the test.
    pub fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = self.stderr_text();
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------
// Plumbing: a bounded child run, an ephemeral port, a tree copy and a bounded diagnostic
// ---------------------------------------------------------------------------------------

/// One bounded child run: its exit code (`None` when killed by a signal), its streams and whether
/// the bound expired.
pub struct Bounded {
    /// The exit code, `None` for a signal-killed child.
    pub code: Option<i32>,
    /// Everything the child wrote to stderr, verbatim.
    pub stderr: String,
    /// Whether the bound expired (the child was then killed and reaped).
    pub timed_out: bool,
}

/// Runs `command` with `/dev/null` stdin and both streams drained by **threads** (so the child can
/// never block on a full pipe), bounded by `timeout` — after which it is killed and reaped.
pub fn run_bounded(mut command: Command, timeout: Duration) -> std::io::Result<Bounded> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let mut stdout = child.stdout.take().expect("the child's stdout is piped");
    let mut stderr = child.stderr.take().expect("the child's stderr is piped");
    let out_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stdout.read_to_string(&mut text);
        text
    });
    let err_reader = thread::spawn(move || {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text);
        text
    });
    let deadline = Instant::now() + timeout;
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            timed_out = true;
            break None;
        }
        thread::sleep(POLL);
    };
    let stderr = err_reader.join().unwrap_or_default();
    let _stdout = out_reader.join().unwrap_or_default();
    Ok(Bounded {
        code: status.and_then(|status| status.code()),
        stderr,
        timed_out,
    })
}

/// Copies `from` to `to` recursively — directories, files and (resolved) symlinks.
///
/// A special file is reported rather than silently dropped; none is needed by the staged build.
pub fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else if metadata.is_file() {
        std::fs::copy(from, to).map(|_| ())
    } else if metadata.file_type().is_symlink() {
        // Resolve it: the staged tree must be self-contained.
        let target = std::fs::canonicalize(from)?;
        copy_tree(&target, to)
    } else {
        eprintln!("note: skipping the special file {}", from.display());
        Ok(())
    }
}

/// A loopback port nothing is listening on: bound on port `0`, read back, then released.
pub fn free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// `exit code N` / `killed by a signal`.
pub fn describe_code(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// The last `lines` lines of `text`, for a bounded diagnostic.
pub fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}
