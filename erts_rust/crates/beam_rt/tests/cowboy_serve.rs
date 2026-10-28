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

//! **A pristine (unpatched) Cowboy HTTP/1.1 server serves a real request on this runtime**, end to
//! end and reproducibly.
//!
//! # What this file pins
//!
//! The whole third-party stack is used exactly as it is published: the read-only cowboy checkout is
//! **staged** into a scratch directory (never written into), `make` builds it with `erlang.mk`
//! (which git-fetches and builds its own `cowlib`/`ranch` dependencies), cowboy's own
//! `examples/hello_world` handler is compiled **unpatched** by the system `erlc`, and the only
//! extra module is the test's own harness ([`HARNESS`]) — test code, not a patch of the project.
//!
//! The server then runs as a real **scheduled process** on the built `beam_rt` binary through the
//! `run` action (the `-eval` route cannot host a blocking `receive` loop):
//!
//! ```text
//! beam_rt -noshell -pa <deps/ranch/ebin> -pa <deps/cowlib/ebin> -pa <stage>/ebin -pa <hw_ebin> \
//!         run <hw_ebin>/cw_serve.beam cw_serve:start/0
//! ```
//!
//! and the requests are driven from this crate with a plain `std::net::TcpStream`, so nothing of
//! the server side is stubbed: `ranch`'s acceptors, `cowboy_clear`'s HTTP/1.1 protocol,
//! `cowboy_router`'s dispatch, the handler, and the response writer all run on the Rust runtime.
//!
//! | Request | The pinned answer |
//! |---|---|
//! | `GET /` | `HTTP/1.1 200 OK`, `content-length: 12`, `content-type: text/plain`, `server: Cowboy`, body `Hello world!` |
//! | `HEAD /` | `HTTP/1.1 200 OK` with the same `content-length: 12` and **no** body |
//! | `GET /nope` | `HTTP/1.1 404 Not Found` with `content-length: 0` and no body |
//! | two `GET /` on **one** connection | two `200` answers, i.e. the connection is reused (keep-alive) |
//! | `GET /` again on a fresh connection | still `200` — the server survived the earlier requests |
//!
//! # Running it
//!
//! The staging + dependency fetch + compile takes tens of seconds, so the test is `#[ignore]`d:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --test cowboy_serve -- --ignored --nocapture
//! ```
//!
//! # Determinism, and the skip contract
//!
//! * The port is **never assumed**: a `TcpListener` is bound on `127.0.0.1:0`, its ephemeral port is
//!   read and the listener dropped, so a concurrent test (or a listener of another agent on this
//!   host) cannot collide with this one, and the same port is handed to the server through
//!   `CW_PORT`.
//! * The spawned runtime is killed and reaped on **every** exit path, a failed assertion included
//!   ([`Serving`]'s `Drop`), so a re-run never inherits a stale listener.
//! * Every socket read carries a timeout and every wait is deadline-bounded, so a lost response is
//!   a visible failure instead of a hang.
//! * **An environment that cannot support the run is a loud skip, never a failure**: a missing
//!   checkout, a missing `erl`/`erlc`/`make`/`git`, a dependency fetch that cannot reach the
//!   network, a port that cannot be bound, a harness that does not compile, or a server that never
//!   prints `CW_READY`. Each prints one `skipping cowboy_serve: <reason>` line (plus the crate's
//!   standard skip banner) and returns, so the suite is green on a machine without the network,
//!   without a toolchain or without the foreign checkout. Nothing else is skipped: once `CW_READY`
//!   is printed, every assertion below is a hard requirement.

// The shared harness is compiled in whole; this suite uses its scratch directory, its toolchain
// discovery and its loud-skip banner.
#[allow(dead_code)]
mod support;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// The suite's name in every diagnostic and skip banner.
const BANNER: &str = "cowboy_serve";

/// The read-only cowboy checkout this test stages from. It is **read**, never written.
const CHECKOUT: &str = "/home/bill/Source/cowboy";

/// The entries copied out of [`CHECKOUT`] into the scratch stage. `ebin` is prebuilt but `make`
/// regenerates it, and this checkout has **no** `include/` directory, so none is required.
const STAGED_ENTRIES: &[&str] = &[
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
const REQUIRED_ENTRIES: &[&str] = &["src", "examples", "Makefile", "erlang.mk"];

/// `make`'s bound: it git-fetches `cowlib` and `ranch` and compiles both plus cowboy.
const MAKE_TIMEOUT: Duration = Duration::from_secs(900);

/// One `erlc` invocation's bound.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(300);

/// How long the server may take to print `CW_READY` (a boot plus `ranch`'s listener start on a
/// loaded host).
const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// One HTTP exchange's socket timeout.
const HTTP_TIMEOUT: Duration = Duration::from_secs(60);

/// The poll interval of every bounded wait here.
const POLL: Duration = Duration::from_millis(10);

/// The test's own harness module: start cowboy's application (which is what makes `ranch`'s
/// listener supervisor available), compile the one-route dispatch table, start a clear-text
/// listener on `$CW_PORT`, announce itself with `CW_READY`, then block for ever. It patches
/// nothing — `hello_world`'s own `toppage_h` is the handler, unmodified.
const HARNESS: &str = r#"
-module(cw_serve).
-export([start/0]).

start() ->
    {ok, _} = application:ensure_all_started(cowboy),
    D = cowboy_router:compile([{'_', [{"/", toppage_h, []}]}]),
    {ok, _Pid} = cowboy:start_clear(my_listener, [{port, port()}], #{env => #{dispatch => D}}),
    io:format("CW_READY~n"),
    receive never -> ok end.

%% The port is chosen by the test, so two runs (or two agents on this host) never collide.
port() ->
    case os:getenv("CW_PORT") of
        false -> 8080;
        Value -> list_to_integer(Value)
    end.
"#;

// ---------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------

/// **A pristine Cowboy HTTP/1.1 server serves a real request on this runtime.**
///
/// Every step is loud: a missing prerequisite prints `skipping cowboy_serve: <reason>` and returns
/// (the test then passes as a skip), and only a server that reached `CW_READY` is held to the
/// pinned responses in this file's module docs.
#[test]
#[ignore = "stages + git-fetches + compiles a pristine cowboy checkout (tens of seconds); run with -- --ignored --nocapture"]
fn a_pristine_cowboy_serves_a_real_http_request() {
    // ---- the environment ---------------------------------------------------------------
    let checkout = Path::new(CHECKOUT);
    if !checkout.is_dir() {
        return skip(&format!("the read-only cowboy checkout {CHECKOUT} is absent"));
    }
    let missing: Vec<&str> = REQUIRED_ENTRIES
        .iter()
        .copied()
        .filter(|entry| !checkout.join(entry).exists())
        .collect();
    if !missing.is_empty() {
        return skip(&format!(
            "{CHECKOUT} does not look like a cowboy checkout (missing {})",
            missing.join(", ")
        ));
    }
    let Some(oracle) = support::resolve_oracle() else {
        return skip("no `erl` on $PATH (and no executable $ERTS_ORACLE)");
    };
    let Some(erlc) = support::resolve_erlc(&oracle) else {
        return skip("no `erlc` beside the oracle and none on $PATH");
    };
    let (Some(make), Some(git)) = (support::find_on_path("make"), support::find_on_path("git"))
    else {
        return skip("`make` and `git` are both needed (erlang.mk git-fetches cowlib and ranch)");
    };

    let Some(scratch) = support::Scratch::new(BANNER, "cowboy_stage") else {
        // `Scratch::new` has already printed the banner for this one.
        return skip_note("cannot create a scratch directory under $TMPDIR");
    };

    // ---- 1. stage the checkout (it is never written to) --------------------------------
    let stage = scratch.dir();
    for entry in STAGED_ENTRIES {
        let from = checkout.join(entry);
        if !from.exists() {
            // A checkout without the optional entries still builds; say so rather than hide it.
            eprintln!("note: {BANNER}: {CHECKOUT}/{entry} does not exist and is not staged");
            continue;
        }
        let to = stage.join(entry);
        if let Err(error) = copy_tree(&from, &to) {
            return skip(&format!("cannot stage {from:?} → {to:?}: {error}"));
        }
    }

    // ---- 2. build it with erlang.mk (which fetches cowlib and ranch) -------------------
    let mut make_command = Command::new(&make);
    make_command.arg("-C").arg(stage);
    // The staged tree is not a git repository; erlang.mk only needs `git` to fetch deps.
    let built = match run_bounded(make_command, MAKE_TIMEOUT) {
        Ok(built) => built,
        Err(error) => return skip(&format!("cannot run `make` in the staged copy: {error}")),
    };
    if built.timed_out {
        return skip(&format!(
            "`make` in the staged copy outlived the {}s bound",
            MAKE_TIMEOUT.as_secs()
        ));
    }
    if built.code != Some(0) {
        return skip(&format!(
            "`make` in the staged copy failed ({}; the dependency fetch needs the network)\n--- tail ---\n{}",
            describe_code(built.code),
            tail(&built.stderr, 20)
        ));
    }
    let cowlib_ebin = stage.join("deps/cowlib/ebin");
    let ranch_ebin = stage.join("deps/ranch/ebin");
    let cowboy_ebin = stage.join("ebin");
    for dir in [&cowlib_ebin, &ranch_ebin, &cowboy_ebin] {
        if !dir.is_dir() {
            return skip(&format!("`make` produced no {} (git: {})", dir.display(), git.display()));
        }
    }

    // ---- 3. compile cowboy's own example handler plus this test's harness --------------
    let hw_ebin = stage.join("hw_ebin");
    if let Err(error) = std::fs::create_dir_all(&hw_ebin) {
        return skip(&format!("cannot create {}: {error}", hw_ebin.display()));
    }
    let harness_source = hw_ebin.join("cw_serve.erl");
    if let Err(error) = std::fs::write(&harness_source, HARNESS) {
        return skip(&format!("cannot write {}: {error}", harness_source.display()));
    }
    let example_src = stage.join("examples/hello_world/src");
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
                        return skip(&format!("cannot read {}: {error}", example_src.display()))
                    }
                }
            }
        }
        Err(error) => return skip(&format!("cannot read {}: {error}", example_src.display())),
    }
    sources.sort();
    if sources.is_empty() {
        return skip(&format!("{} holds no `.erl` sources", example_src.display()));
    }
    let mut erlc_command = Command::new(&erlc);
    erlc_command.arg("-o").arg(&hw_ebin);
    erlc_command.args(&sources);
    erlc_command.arg(&harness_source);
    let compiled = match run_bounded(erlc_command, COMPILE_TIMEOUT) {
        Ok(compiled) => compiled,
        Err(error) => return skip(&format!("cannot run `{}`: {error}", erlc.display())),
    };
    if compiled.timed_out || compiled.code != Some(0) {
        return skip(&format!(
            "the system `erlc` could not compile cowboy's hello_world example ({}; {} source(s))\n--- tail ---\n{}",
            describe_code(compiled.code),
            sources.len(),
            tail(&compiled.stderr, 20)
        ));
    }
    let harness_beam = hw_ebin.join("cw_serve.beam");
    for beam in [&harness_beam, &hw_ebin.join("toppage_h.beam")] {
        if !beam.is_file() {
            return skip(&format!("`erlc` produced no {}", beam.display()));
        }
    }

    // ---- 4. a port nobody else holds ---------------------------------------------------
    let Ok(port) = free_port() else {
        return skip("cannot bind a loopback port for the listener");
    };

    // ---- 5. serve it on this runtime ---------------------------------------------------
    let mut command = Command::new(env!("CARGO_BIN_EXE_beam_rt"));
    for dir in [&ranch_ebin, &cowlib_ebin, &cowboy_ebin, &hw_ebin] {
        command.arg("-pa").arg(dir);
    }
    command
        .arg("run")
        .arg(&harness_beam)
        .arg("cw_serve:start/0")
        .env("CW_PORT", port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut serving = match Serving::spawn(command, port) {
        Ok(serving) => serving,
        Err(error) => return skip(&format!("cannot spawn beam_rt: {error}")),
    };
    if let Err(why) = serving.wait_ready(READY_TIMEOUT) {
        return skip(&why);
    }
    println!("{BANNER}: a pristine cowboy is serving on 127.0.0.1:{port}");

    // ---- 6. the requests ---------------------------------------------------------------
    // A fresh connection: the status line, the headers and the body, read to EOF.
    let get = serving.exchange("GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(
        get.status_line, "HTTP/1.1 200 OK",
        "GET / must answer the pinned status line; full response:\n{}",
        get.raw
    );
    assert_eq!(
        get.header("content-length").as_deref(),
        Some("12"),
        "GET / content-length; full response:\n{}",
        get.raw
    );
    assert_eq!(
        get.header("content-type").as_deref(),
        Some("text/plain"),
        "GET / content-type; full response:\n{}",
        get.raw
    );
    assert_eq!(
        get.header("server").as_deref(),
        Some("Cowboy"),
        "GET / server; full response:\n{}",
        get.raw
    );
    assert_eq!(
        get.body, b"Hello world!",
        "GET / body; full response:\n{}",
        get.raw
    );

    // HEAD carries the entity's headers and no body at all.
    let head = serving.exchange("HEAD / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(
        head.status_line, "HTTP/1.1 200 OK",
        "HEAD / status line; full response:\n{}",
        head.raw
    );
    assert_eq!(
        head.header("content-length").as_deref(),
        Some("12"),
        "HEAD / content-length; full response:\n{}",
        head.raw
    );
    assert!(
        head.body.is_empty(),
        "HEAD / must carry no body, got {:?}; full response:\n{}",
        String::from_utf8_lossy(&head.body),
        head.raw
    );

    // An unrouted path is the router's own 404, not a dropped connection.
    let missing = serving.exchange("GET /nope HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(
        missing.status_line, "HTTP/1.1 404 Not Found",
        "GET /nope status line; full response:\n{}",
        missing.raw
    );
    assert_eq!(
        missing.header("content-length").as_deref(),
        Some("0"),
        "GET /nope content-length; full response:\n{}",
        missing.raw
    );
    assert!(
        missing.body.is_empty(),
        "GET /nope must carry no body, got {:?}; full response:\n{}",
        String::from_utf8_lossy(&missing.body),
        missing.raw
    );

    // Two requests on ONE connection: the connection is reused (HTTP/1.1 keep-alive), so the
    // second answer arrives without a new handshake.
    let kept = serving.exchange_keep_alive("GET / HTTP/1.1\r\nHost: x\r\n\r\n", 2);
    assert_eq!(
        kept.len(),
        2,
        "two keep-alive requests must be answered on one connection, got {}; transcript:\n{}",
        kept.len(),
        kept.join("\n---\n")
    );
    assert_eq!(kept[0], "200 Hello world!");
    assert_eq!(kept[1], "200 Hello world!");

    // A fresh connection still works: repeated requests keep the server serving.
    let again = serving.exchange("GET / HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(
        again.status_line, "HTTP/1.1 200 OK",
        "a later GET / must still answer 200; full response:\n{}",
        again.raw
    );
    assert_eq!(again.body, b"Hello world!", "a later GET /'s body");

    // The server is still running: five requests did not take the listener (or the runtime) down.
    let alive = serving.is_running();
    let stderr = serving.stderr_text();
    assert!(
        alive,
        "the serving runtime exited while serving; its stdout:\n{}\n--- stderr ---\n{stderr}",
        serving.transcript.join("\n")
    );

    // The server must not have reported anything of its own while serving.
    assert!(
        stderr.trim().is_empty(),
        "the serving runtime wrote to stderr:\n{stderr}"
    );

    println!(
        "{BANNER}: GET / ⇒ {} ({} bytes {:?}), HEAD / ⇒ {}, GET /nope ⇒ {}, keep-alive ⇒ 2 × {}, \
         then GET / ⇒ {}",
        get.status_line,
        get.body.len(),
        String::from_utf8_lossy(&get.body),
        head.status_line,
        missing.status_line,
        kept[0],
        again.status_line
    );
}

// ---------------------------------------------------------------------------------------
// The served response, parsed the way an HTTP/1.1 client reads it
// ---------------------------------------------------------------------------------------

/// One HTTP/1.1 response read from a socket: the raw bytes (for diagnostics), the status line and
/// the headers, plus everything after the header block as `body`.
struct Response {
    /// Everything the server sent, verbatim — what every failure message prints.
    raw: String,
    /// The status line, without its `\r\n`.
    status_line: String,
    /// The header fields, name lowercased.
    headers: Vec<(String, String)>,
    /// The bytes after the header block (for a `HEAD` answer: empty).
    body: Vec<u8>,
}

impl Response {
    /// A header's value by its **lowercased** name.
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value.clone())
    }
}

/// Splits raw bytes into a [`Response`]: the header block ends at the first blank line.
fn parse_response(bytes: &[u8]) -> Response {
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

/// The `beam_rt` child serving [`HARNESS`] on its own ephemeral port.
///
/// Its stdout is drained line by line into a channel (`CW_READY` is awaited as it appears) and
/// kept in a transcript; stderr is drained in the background so the child can never block on a
/// full pipe. `Drop` kills and reaps it, so a failed assertion (or an early `return` from a skip)
/// never leaves a listener behind for the next run.
struct Serving {
    /// The child process.
    child: Child,
    /// The port the harness was told to listen on.
    port: u16,
    /// The child's stdout, one line at a time.
    lines: Receiver<String>,
    /// The child's stderr, one line at a time (drained on demand, so the pipe never fills).
    err_lines: Receiver<String>,
    /// Every stdout line so far (failure diagnostics).
    transcript: Vec<String>,
    /// Every stderr line so far (failure diagnostics).
    stderr: Vec<String>,
}

impl Serving {
    /// Starts a child for `command` (already configured with its `-pa` list, the `run` action and
    /// `CW_PORT`), with both pipes drained by background threads.
    fn spawn(mut command: Command, port: u16) -> std::io::Result<Serving> {
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
        // The stderr reader thread hands every line over a channel the struct drains on demand.
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
    fn wait_ready(&mut self, timeout: Duration) -> Result<(), String> {
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
    fn stderr_text(&mut self) -> String {
        while let Ok(line) = self.err_lines.try_recv() {
            self.stderr.push(line);
        }
        self.stderr.join("\n")
    }

    /// Whether the serving child is still running (a serve loop never exits by itself).
    fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Sends `request` on a fresh connection and reads the answer to EOF.
    fn exchange(&mut self, request: &str) -> Response {
        let mut stream = self.connect();
        stream
            .write_all(request.as_bytes())
            .unwrap_or_else(|error| panic!("cannot write the request: {error}\n{request:?}"));
        stream
            .flush()
            .unwrap_or_else(|error| panic!("cannot flush the request: {error}"));
        let mut bytes = Vec::new();
        stream
            .read_to_end(&mut bytes)
            .unwrap_or_else(|error| panic!("cannot read the response: {error}"));
        parse_response(&bytes)
    }

    /// Sends `request` `times` times over **one** connection, reading each answer from the
    /// `content-length` the server declared (so the connection really is reused, not re-dialled).
    ///
    /// Returns one `"<status> <body>"` summary per answer.
    fn exchange_keep_alive(&mut self, request: &str, times: usize) -> Vec<String> {
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
    fn connect(&self) -> TcpStream {
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
    fn stop(&mut self) {
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
// Plumbing: the staged copy, a bounded child run, an ephemeral port, and the skip line
// ---------------------------------------------------------------------------------------

/// Prints the one skip line and the crate's standard banner, and lets the test pass.
fn skip(reason: &str) {
    skip_note(reason);
    support::skip_loudly(BANNER, reason);
}

/// Prints only the one `skipping cowboy_serve: <reason>` line — for a path where the banner has
/// already been printed.
fn skip_note(reason: &str) {
    eprintln!("skipping {BANNER}: {reason}");
}

/// Copies `from` to `to` recursively — directories, files and (resolved) symlinks.
///
/// A special file is reported rather than silently dropped; none is needed by the staged build.
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
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
        eprintln!("note: {BANNER}: skipping the special file {}", from.display());
        Ok(())
    }
}

/// One bounded child run: its exit code (`None` when killed by a signal), its streams and whether
/// the bound expired.
struct Bounded {
    /// The exit code, `None` for a signal-killed child.
    code: Option<i32>,
    /// Everything the child wrote to stderr, verbatim.
    stderr: String,
    /// Whether the bound expired (the child was then killed and reaped).
    timed_out: bool,
}

/// Runs `command` with `/dev/null` stdin and both streams drained by **threads** (so the child can
/// never block on a full pipe), bounded by `timeout` — after which it is killed and reaped.
fn run_bounded(mut command: Command, timeout: Duration) -> std::io::Result<Bounded> {
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

/// A loopback port nothing is listening on: bound on port `0`, read back, then released.
fn free_port() -> std::io::Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

/// `exit code N` / `killed by a signal`.
fn describe_code(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// The last `lines` lines of `text`, for a bounded diagnostic.
fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}
