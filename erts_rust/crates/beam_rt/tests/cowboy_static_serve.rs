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

//! **A pristine `cowboy_static` file route delivers the file body on this runtime** — the
//! regression witness for the `sendfile` path.
//!
//! # The capability this pins
//!
//! `cowboy_static` answers a file request with a `{sendfile, Offset, Bytes, Path}` body
//! (`cowboy_static.erl`'s `get_file/2`), so the whole response rides `prim_file:sendfile/8`
//! rather than an ordinary binary body. Until that NIF stub was served by the native floor, the
//! request produced a **complete 200 header block with the true `content-length` and then no
//! body at all** — the connection stayed open and the client waited for ever. That is why this
//! suite pins the **body bytes** and not just the status line: a header-only answer must fail
//! here, never pass.
//!
//! The route is the published handler with the published `{dir, Dir}` form; the directory is a
//! scratch directory of this test's own, holding one 26-byte file, and it is handed to the
//! harness through `$CW_DIR` — the same env-var scheme `$CW_PORT` uses, so a concurrent run (or
//! another agent on this host) cannot collide with this one.
//!
//! ```text
//! D = cowboy_router:compile([{'_', [{"/static/[...]", cowboy_static, {dir, Dir}}]}]),
//! ```
//!
//! | Request | The pinned answer |
//! |---|---|
//! | `GET /static/hello.txt` | `HTTP/1.1 200 OK`, `content-length: 26`, `content-type: application/octet-stream`, `accept-ranges: bytes`, `allow: HEAD, GET, OPTIONS`, an `etag: "<digits>"` **shape**, body `static-payload-1234567890\n` |
//! | the same path with `Range: bytes=7-13` | `HTTP/1.1 206 Partial Content`, `content-range: bytes 7-13/26`, `content-length: 7`, body `payload` |
//! | `GET /static/nope.txt` | `HTTP/1.1 404 Not Found`, `content-length: 0`, no body |
//!
//! The `etag` is `integer_to_binary(erlang:phash2({Size, Mtime}, 16#ffffffff))`
//! (`cowboy_static.erl`'s `generate_default_etag/2`), so it is **shape-only** here: a literal
//! would pin one file's mtime, which no fixture controls.
//!
//! # Running it
//!
//! The staging + dependency fetch + compile takes tens of seconds, so the test is `#[ignore]`d:
//!
//! ```text
//! erts_rust/scripts/cargo.sh test --manifest-path erts_rust/Cargo.toml -p beam_rt \
//!     --test cowboy_static_serve -- --ignored --nocapture
//! ```
//!
//! # Determinism, and the skip contract
//!
//! Identical to `cowboy_serve.rs`, because the machinery is the same `support/cowboy.rs`: an
//! ephemeral port nobody else holds, a kill-and-reap `Drop` on every exit path, a socket timeout
//! on every read, and a **loud skip** (never a failure) when the checkout, the toolchain, the
//! network or the server is not available. Once `CW_READY` is printed, every assertion is a hard
//! requirement — a missing body included.

// The staged-pristine-Cowboy harness both this suite and `cowboy_serve.rs` drive.
#[path = "support/cowboy.rs"]
#[allow(dead_code)]
mod cowboy;
// The shared harness is compiled in whole; this suite uses its scratch directory and its
// loud-skip helpers through `cowboy`.
#[allow(dead_code)]
mod support;

/// The suite's name in every diagnostic and skip banner.
const BANNER: &str = "cowboy_static_serve";

/// The served file's exact contents: **26** bytes, the newline included and nothing after it.
const STATIC_BODY: &str = "static-payload-1234567890\n";

/// The served file's name inside the scratch directory.
const STATIC_NAME: &str = "hello.txt";

/// The test's own harness module: start cowboy's application, compile the one static-file route
/// from `$CW_DIR`, start a clear-text listener on `$CW_PORT`, announce itself with `CW_READY`,
/// then block for ever. `cowboy_static` itself is cowboy's own, unmodified, handler.
///
/// A missing `$CW_DIR` is fatal at start rather than a silent serve-from-`"."`: the listener then
/// never reaches `CW_READY`, and the failure names itself in the transcript.
const HARNESS: &str = r#"
-module(cw_static_serve).
-export([start/0]).

start() ->
    {ok, _} = application:ensure_all_started(cowboy),
    Dir = dir(),
    D = cowboy_router:compile([{'_', [{"/static/[...]", cowboy_static, {dir, Dir}}]}]),
    {ok, _Pid} = cowboy:start_clear(static_listener, [{port, port()}], #{env => #{dispatch => D}}),
    io:format("CW_READY~n"),
    receive never -> ok end.

%% The directory and the port are chosen by the test, so no run collides with another.
dir() ->
    case os:getenv("CW_DIR") of
        false -> exit(cw_dir_unset);
        Value -> Value
    end.

port() ->
    case os:getenv("CW_PORT") of
        false -> 8080;
        Value -> list_to_integer(Value)
    end.
"#;

// ---------------------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------------------

/// **A pristine `cowboy_static` route serves a file body, a range and a 404 on this runtime.**
///
/// Every step is loud: a missing prerequisite prints `skipping cowboy_static_serve: <reason>` and
/// returns (the test then passes as a skip), and only a server that reached `CW_READY` is held to
/// the pinned responses in this file's module docs.
#[test]
#[ignore = "stages + git-fetches + compiles a pristine cowboy checkout (tens of seconds); run with -- --ignored --nocapture"]
fn a_pristine_cowboy_static_serves_a_file_body() {
    // ---- the served file, in a scratch directory of its own -----------------------------
    let Some(served_dir) = support::Scratch::new(BANNER, "cowboy_static_dir") else {
        // `Scratch::new` printed the banner for this one; only the note is still owed.
        return cowboy::skip_note(BANNER, "cannot create a scratch directory under $TMPDIR");
    };
    let static_path = served_dir.dir().join(STATIC_NAME);
    if let Err(error) = std::fs::write(&static_path, STATIC_BODY) {
        return cowboy::skip(BANNER, &format!("cannot write {}: {error}", static_path.display()));
    }
    // The pinned `content-length`/`content-range` figures below are this string's own length; a
    // fixture that drifted would otherwise rewrite them silently.
    assert_eq!(
        STATIC_BODY.len(),
        26,
        "the served file's body must stay 26 bytes, got {:?} ({} byte(s))",
        STATIC_BODY,
        STATIC_BODY.len()
    );

    // ---- the environment: stage the checkout and build it (a loud skip at every gate) ----
    let stage = match cowboy::Stage::build(BANNER) {
        Ok(stage) => stage,
        Err(skip) => return skip.report(BANNER),
    };

    // ---- compile cowboy's own example sources plus this test's harness ------------------
    let served = match stage.compile(&cowboy::Harness {
        module: "cw_static_serve",
        source: HARNESS,
        entry: "cw_static_serve:start/0",
    }) {
        Ok(served) => served,
        Err(reason) => return cowboy::skip(BANNER, &reason),
    };

    // ---- a port nobody else holds -------------------------------------------------------
    let Ok(port) = cowboy::free_port() else {
        return cowboy::skip(BANNER, "cannot bind a loopback port for the listener");
    };

    // ---- serve it on this runtime -------------------------------------------------------
    let mut command = stage.command(&served, port);
    command.env("CW_DIR", served_dir.dir());
    let mut serving = match cowboy::Serving::spawn(command, port) {
        Ok(serving) => serving,
        Err(error) => return cowboy::skip(BANNER, &format!("cannot spawn beam_rt: {error}")),
    };
    if let Err(why) = serving.wait_ready(cowboy::READY_TIMEOUT) {
        return cowboy::skip(BANNER, &why);
    }
    println!(
        "{BANNER}: a pristine cowboy_static is serving {} on 127.0.0.1:{port}",
        static_path.display()
    );

    // ---- 1. the whole file, body included ----------------------------------------------
    // The body is the regression: this answer used to be the headers alone, and the connection
    // stayed open with nothing after them. A read that never finishes fails on
    // `cowboy::HTTP_TIMEOUT`; a read that finishes short fails the byte comparison.
    let whole = serving.exchange(&format!(
        "GET /static/{STATIC_NAME} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    ));
    assert_eq!(
        whole.status_line, "HTTP/1.1 200 OK",
        "GET /static/{STATIC_NAME} status line; full response:\n{}",
        whole.raw
    );
    assert_eq!(
        whole.header("content-length").as_deref(),
        Some("26"),
        "GET /static/{STATIC_NAME} content-length; full response:\n{}",
        whole.raw
    );
    assert_eq!(
        whole.header("content-type").as_deref(),
        Some("application/octet-stream"),
        "GET /static/{STATIC_NAME} content-type (`.txt` has no `cow_mimetypes:web/1` entry, so the \
         published fallback applies); full response:\n{}",
        whole.raw
    );
    assert_eq!(
        whole.header("accept-ranges").as_deref(),
        Some("bytes"),
        "GET /static/{STATIC_NAME} accept-ranges; full response:\n{}",
        whole.raw
    );
    assert_eq!(
        whole.header("allow").as_deref(),
        Some("HEAD, GET, OPTIONS"),
        "GET /static/{STATIC_NAME} allow; full response:\n{}",
        whole.raw
    );
    // The etag is `phash2({Size, Mtime})`, i.e. a quoted run of digits — never a literal.
    let etag = whole.header("etag").unwrap_or_else(|| {
        panic!(
            "GET /static/{STATIC_NAME} must carry an etag; full response:\n{}",
            whole.raw
        )
    });
    let digits = etag
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or("");
    assert!(
        !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()),
        "GET /static/{STATIC_NAME} etag must be a quoted digit string, got {etag:?}; \
         full response:\n{}",
        whole.raw
    );
    // **The regression assertion**: the file's bytes really arrive, in order and in full.
    assert_eq!(
        whole.body,
        STATIC_BODY.as_bytes(),
        "GET /static/{STATIC_NAME} must deliver the file body ({} byte(s) expected, {} received); \
         full response:\n{}",
        STATIC_BODY.len(),
        whole.body.len(),
        whole.raw
    );

    // ---- 2. a byte range: 206 with its own length and content-range ---------------------
    // Bytes 7..=13 of `static-payload-1234567890\n` are `payload`.
    let range = serving.exchange(&format!(
        "GET /static/{STATIC_NAME} HTTP/1.1\r\nHost: x\r\nRange: bytes=7-13\r\nConnection: close\r\n\r\n"
    ));
    assert_eq!(
        range.status_line, "HTTP/1.1 206 Partial Content",
        "the ranged GET must answer 206; full response:\n{}",
        range.raw
    );
    assert_eq!(
        range.header("content-range").as_deref(),
        Some("bytes 7-13/26"),
        "the ranged GET's content-range; full response:\n{}",
        range.raw
    );
    assert_eq!(
        range.header("content-length").as_deref(),
        Some("7"),
        "the ranged GET's content-length; full response:\n{}",
        range.raw
    );
    assert_eq!(
        range.body, b"payload",
        "the ranged GET's body; full response:\n{}",
        range.raw
    );

    // ---- 3. a missing file inside a live route is a 404, not a dropped connection --------
    let missing = serving.exchange("GET /static/nope.txt HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
    assert_eq!(
        missing.status_line, "HTTP/1.1 404 Not Found",
        "GET /static/nope.txt status line; full response:\n{}",
        missing.raw
    );
    assert_eq!(
        missing.header("content-length").as_deref(),
        Some("0"),
        "GET /static/nope.txt content-length; full response:\n{}",
        missing.raw
    );
    assert!(
        missing.body.is_empty(),
        "GET /static/nope.txt must carry no body, got {:?}; full response:\n{}",
        String::from_utf8_lossy(&missing.body),
        missing.raw
    );

    // The server is still running, and it reported nothing of its own while serving.
    let alive = serving.is_running();
    let stderr = serving.stderr_text();
    assert!(
        alive,
        "the serving runtime exited while serving; its stdout:\n{}\n--- stderr ---\n{stderr}",
        serving.transcript.join("\n")
    );
    assert!(
        stderr.trim().is_empty(),
        "the serving runtime wrote to stderr:\n{stderr}"
    );

    println!(
        "{BANNER}: GET /static/{STATIC_NAME} ⇒ {} (etag {etag}, {} bytes {:?}), \
         Range: bytes=7-13 ⇒ {} ({:?} of {:?}), GET /static/nope.txt ⇒ {}",
        whole.status_line,
        whole.body.len(),
        String::from_utf8_lossy(&whole.body),
        range.status_line,
        String::from_utf8_lossy(&range.body),
        range.header("content-range").unwrap_or_default(),
        missing.status_line
    );
}
