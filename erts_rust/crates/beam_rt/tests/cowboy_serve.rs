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
//! The sibling `cowboy_static_serve.rs` drives `cowboy_static`'s file route the same way; the
//! staging, compile, serve and skip machinery both suites use is `support/cowboy.rs`, so the two
//! cannot drift apart.
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
//!   (`cowboy::Serving`'s `Drop`), so a re-run never inherits a stale listener.
//! * Every socket read carries a timeout and every wait is deadline-bounded, so a lost response is
//!   a visible failure instead of a hang.
//! * **An environment that cannot support the run is a loud skip, never a failure**: a missing
//!   checkout, a missing `erl`/`erlc`/`make`/`git`, a dependency fetch that cannot reach the
//!   network, a port that cannot be bound, a harness that does not compile, or a server that never
//!   prints `CW_READY`. Each prints one `skipping cowboy_serve: <reason>` line (plus the crate's
//!   standard skip banner) and returns, so the suite is green on a machine without the network,
//!   without a toolchain or without the foreign checkout. Nothing else is skipped: once `CW_READY`
//!   is printed, every assertion below is a hard requirement.

// The staged-pristine-Cowboy harness both this suite and `cowboy_static_serve.rs` drive.
#[path = "support/cowboy.rs"]
#[allow(dead_code)]
mod cowboy;
// The shared harness is compiled in whole; this suite uses its scratch directory, its toolchain
// discovery and its loud-skip banner.
#[allow(dead_code)]
mod support;

/// The suite's name in every diagnostic and skip banner.
const BANNER: &str = "cowboy_serve";

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
    // ---- the environment: stage the checkout and build it (a loud skip at every gate) ----
    let stage = match cowboy::Stage::build(BANNER) {
        Ok(stage) => stage,
        Err(skip) => return skip.report(BANNER),
    };

    // ---- compile cowboy's own example handler plus this test's harness ------------------
    let served = match stage.compile(&cowboy::Harness {
        module: "cw_serve",
        source: HARNESS,
        entry: "cw_serve:start/0",
    }) {
        Ok(served) => served,
        Err(reason) => return cowboy::skip(BANNER, &reason),
    };

    // ---- a port nobody else holds -------------------------------------------------------
    let Ok(port) = cowboy::free_port() else {
        return cowboy::skip(BANNER, "cannot bind a loopback port for the listener");
    };

    // ---- serve it on this runtime -------------------------------------------------------
    let mut serving = match cowboy::Serving::spawn(stage.command(&served, port), port) {
        Ok(serving) => serving,
        Err(error) => return cowboy::skip(BANNER, &format!("cannot spawn beam_rt: {error}")),
    };
    if let Err(why) = serving.wait_ready(cowboy::READY_TIMEOUT) {
        return cowboy::skip(BANNER, &why);
    }
    println!("{BANNER}: a pristine cowboy is serving on 127.0.0.1:{port}");

    // ---- the requests -------------------------------------------------------------------
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
