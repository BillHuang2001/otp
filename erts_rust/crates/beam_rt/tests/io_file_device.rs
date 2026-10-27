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

//! **File devices** — the `io` output family addressed to the pid `file:open/2` handed out.
//!
//! # What a file device is, and which route serves it
//!
//! `file:open/2` answers a **pid** (a `file_io_server` process), and every `io` output call on it
//! is the `{io_request, From, ReplyAs, Request}` protocol of `lib/stdlib/src/io.erl` — not a
//! stream write. `src/native/file_server.rs` serves such a request synchronously
//! (`Runtime::serve_io_request`), which is what makes `io:format(Fd, …)` work without a scheduler
//! round trip; that is the shape `common_test`'s `ct_run:install/2` (`ct_run.erl:848`,
//! `io:format(Fd, "~tp.\n", [Opt])`) needs.
//!
//! The one bound on it is the *route*: an `io` output call can only complete a protocol round trip
//! where the caller may pump the scheduler, so these probes run on the `-eval` route — the same
//! route `-s`/`-run` and a pumping `apply` take. Inside a scheduler slice the `io` BIF cannot
//! block, so the call there is the documented typed `unsupported` gap rather than a wrong write;
//! [`a_file_device_on_a_route_that_cannot_pump_is_a_typed_gap`] pins that boundary so it cannot
//! silently decay into a leak.
//!
//! # What is compared
//!
//! The fixture ([`FIXTURE_ERL`]) is compiled at test time by the system `erlc`. Each probe writes
//! through one fixture file and then reads **the bytes back**, so the assertion is about what
//! reached the file rather than about a return value: the same fixture runs under the live oracle
//! with a second path and the two files must be byte-identical. The ASCII case is additionally
//! pinned literally, so the suite still discriminates with no toolchain.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass.

#[allow(dead_code)]
mod support;

use std::fs;
use std::path::Path;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "io file device";

/// The fixture: the output families on a file device, plus the two failure shapes.
///
/// `write_all/1` opens `Path` for writing and drives `io:put_chars/2`, `io:format/3`, `io:nl/1`,
/// `io:write/2` and `io:fwrite/3` on it, asserting each `ok` (the file server's own reply), then
/// closes it. `write_unicode/1` writes three characters outside ASCII, so the device's own
/// encoding (a `[write]` file is a **latin1** device) has to be applied to the request's
/// `unicode` payload. `dead_device/0` is a pid that is not a live process and `closed_device/1` a
/// device the caller already closed — both are the oracle's `error:terminated`.
const FIXTURE_ERL: &str = r#"
-module(fiodev).
-export([write_all/1, write_unicode/1, dead_device/0, closed_device/1]).

write_all(Path) ->
    {ok, F} = file:open(Path, [write]),
    ok = io:put_chars(F, "alpha"),
    ok = io:format(F, "~nbeta ~p~n", [{1, 2}]),
    ok = io:nl(F),
    ok = io:write(F, {gamma, [1, 2, 3]}),
    ok = io:fwrite(F, "delta ~s", ["eps"]),
    ok = file:close(F),
    ok.

write_unicode(Path) ->
    {ok, F} = file:open(Path, [write]),
    ok = io:put_chars(F, [229, 228, 246]),
    ok = io:nl(F),
    ok = io:put_chars(F, "plain"),
    ok = file:close(F),
    ok.

dead_device() ->
    Device = spawn(fun() -> ok end),
    timer:sleep(100),
    try io:put_chars(Device, "x") of
        Ok -> {returned, Ok}
    catch
        Class:Reason -> {Class, Reason}
    end.

closed_device(Path) ->
    {ok, F} = file:open(Path, [write]),
    ok = file:close(F),
    try io:put_chars(F, "after-close") of
        Ok -> {returned, Ok}
    catch
        Class:Reason -> {Class, Reason}
    end.
"#;

/// The bytes [`FIXTURE_ERL`]'s `write_all/1` must produce, pinned.
///
/// `~n` is one newline, `~p` renders `{1,2}`, and `io:write/2` appends `io_lib:write/1`'s `~w`
/// **without** a newline of its own — so the last two writes concatenate.
const WRITE_ALL_BYTES: &str = "alpha\nbeta {1,2}\n\n{gamma,[1,2,3]}delta eps";

/// The bytes `write_unicode/1` must produce on a `[write]` (latin1) device: the request payload is
/// `unicode`, so the device re-encodes `åäö` **back** to latin1 (`e5 e4 f6`) — not the UTF-8
/// `c3 a5 c3 a4 c3 b6`.
const WRITE_UNICODE_BYTES: [u8; 9] = [0xe5, 0xe4, 0xf6, b'\n', b'p', b'l', b'a', b'i', b'n'];

// ---------------------------------------------------------------------------------------
// The two sides of one probe
// ---------------------------------------------------------------------------------------

/// Compiles [`FIXTURE_ERL`], or returns `None` after the loud skip.
fn fixture() -> Option<support::ErlangFixture> {
    support::ErlangFixture::compile(SUITE, "io_file_device", "fiodev", FIXTURE_ERL)
}

/// `fiodev:<function>(<path>)` — the probe call shape for the path-taking exports.
fn call1(function: &str, path: &Path) -> String {
    format!("fiodev:{function}({path:?})")
}

/// `fiodev:<function>()` — the probe call shape for the arity-0 exports.
fn call0(function: &str) -> String {
    format!("fiodev:{function}()")
}

/// The `-eval` expression body that renders `call`'s answer with `~w` (no terminator: the
/// two routes end the expression differently).
fn eval_expr(call: &str) -> String {
    format!("io:format(\"~w~n\",[{call}])")
}

/// Runs `call` through the built `beam_rt` on the `-eval` route — the route where the call may
/// pump the scheduler — and answers its trimmed transcript.
fn ours(fixture: &support::ErlangFixture, call: &str) -> String {
    let dir = fixture.dir().to_str().expect("an ASCII scratch path");
    let expr = format!("{}.", eval_expr(call));
    let observed = support::run_runtime(&["-noshell", "-pa", dir, "-eval", &expr])
        .unwrap_or_else(|| panic!("`{call}` could not run to completion"));
    assert_eq!(
        observed.code, 0,
        "`{call}` exited {}: {}",
        observed.code, observed.stderr
    );
    assert!(
        !observed.stderr.contains("unsupported"),
        "a file device is served synchronously; this is not a capability gap: {}",
        observed.stderr
    );
    observed.stdout.trim().to_string()
}

/// Runs the same probe under the live oracle, or returns `None` after the loud skip.
fn oracle(fixture: &support::ErlangFixture, call: &str) -> Option<String> {
    let Some(oracle) = support::resolve_oracle() else {
        support::skip_loudly(SUITE, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
        return None;
    };
    let expr = format!("{}, halt(0).", eval_expr(call));
    support::oracle_eval(&oracle, Some(fixture.dir()), &expr)
}

/// The bytes `path` holds, or a panic naming the probe.
fn read_bytes(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every `io` output call on a `file:open/2` device writes the oracle's bytes.
///
/// The probe drives `io:put_chars/2`, `io:format/3` (the explicit-device arity), `io:nl/1`,
/// `io:write/2` and `io:fwrite/3`, each of which must answer the file server's `ok`; then the
/// **file** is read back and compared — first with the pinned bytes, and then (with the toolchain
/// present) with what the same fixture wrote under the live oracle.
#[test]
fn the_output_family_writes_the_oracles_bytes_to_a_file_device() {
    let Some(fixture) = fixture() else {
        return;
    };
    let ours_path = fixture.path("ours_write_all.txt");
    let answer = ours(&fixture, &call1("write_all", &ours_path));
    assert_eq!(answer, "ok", "every device request must answer `ok`");
    let bytes = read_bytes(&ours_path);
    assert_eq!(
        String::from_utf8_lossy(&bytes),
        WRITE_ALL_BYTES,
        "the bytes `write_all/1` put in the file"
    );
    let oracle_path = fixture.path("oracle_write_all.txt");
    if let Some(expected) = oracle(&fixture, &call1("write_all", &oracle_path)) {
        assert_eq!(expected, "ok", "the oracle answers `ok` too");
        assert_eq!(
            bytes,
            read_bytes(&oracle_path),
            "the two runtimes must write byte-identical files"
        );
    }
}

/// A `latin1` file device re-encodes a `unicode` request payload — the bytes on disk are latin1.
///
/// This is the one place the request's encoding and the device's encoding differ, so it is where a
/// runtime that wrote the payload verbatim (UTF-8) would diverge. The device is `[write]`, whose
/// `getopts` is `[{binary,false},{encoding,latin1}]` (`tests/scheduled_io_device.rs` pins that),
/// so `io:put_chars/2`'s UTF-8-encoded payload is converted **back** by the device.
#[test]
fn a_latin1_file_device_re_encodes_a_unicode_request() {
    let Some(fixture) = fixture() else {
        return;
    };
    let ours_path = fixture.path("ours_unicode.txt");
    assert_eq!(ours(&fixture, &call1("write_unicode", &ours_path)), "ok");
    let bytes = read_bytes(&ours_path);
    assert_eq!(
        bytes, WRITE_UNICODE_BYTES,
        "a latin1 device writes latin1 bytes, not the request's UTF-8: {:?}",
        String::from_utf8_lossy(&bytes)
    );
    let oracle_path = fixture.path("oracle_unicode.txt");
    if let Some(expected) = oracle(&fixture, &call1("write_unicode", &oracle_path)) {
        assert_eq!(expected, "ok", "the oracle answers `ok` too");
        assert_eq!(
            bytes,
            read_bytes(&oracle_path),
            "the two runtimes must write byte-identical files"
        );
    }
}

/// A **dead** device pid and an already **closed** device are both the oracle's `error:terminated`
/// — the `io.erl` `terminated` clause, never a silent write and never a file created behind the
/// caller's back.
#[test]
fn a_dead_or_closed_file_device_is_error_terminated() {
    let Some(fixture) = fixture() else {
        return;
    };
    let dead = ours(&fixture, &call0("dead_device"));
    assert_eq!(dead, "{error,terminated}", "a dead device pid");
    if let Some(expected) = oracle(&fixture, &call0("dead_device")) {
        assert_eq!(expected, dead, "the oracle's answer for a dead device pid");
    }

    let closed_path = fixture.path("ours_closed.txt");
    let closed = ours(&fixture, &call1("closed_device", &closed_path));
    assert_eq!(closed, "{error,terminated}", "a closed file device");
    assert_eq!(
        read_bytes(&closed_path),
        Vec::<u8>::new(),
        "the write after `file:close/1` must not have reached the file"
    );
    let oracle_closed = fixture.path("oracle_closed.txt");
    if let Some(expected) = oracle(&fixture, &call1("closed_device", &oracle_closed)) {
        assert_eq!(expected, closed, "the oracle's answer for a closed device");
    }
}

/// The documented **boundary**: on a route that cannot pump the scheduler, an `io` output call on
/// a live device is the typed `unsupported` capability gap (CLI exit 3) — not a wrong write, and
/// not a byte on stdout.
///
/// The `run` action executes the fixture as a **scheduled** process, and a scheduler slice may not
/// block in the `{io_request, …}` round trip, so the floor declines and the call surfaces the gap.
/// This is the residual the routing rule is honest about, and pinning it here means a future
/// change that starts *writing* on this route fails loudly instead of leaking into the transcript.
#[test]
fn a_file_device_on_a_route_that_cannot_pump_is_a_typed_gap() {
    let Some(fixture) = fixture() else {
        return;
    };
    let beam = fixture.beam();
    let beam = beam.to_str().expect("an ASCII fixture path");
    let target = fixture.path("never_written.txt");
    let target = target.to_str().expect("an ASCII scratch path");
    let observed = support::run_runtime(&["-noshell", "run", beam, "fiodev:write_all/1", target])
        .expect("the `run` probe completes");
    assert_eq!(
        observed.code, 3,
        "a slice's io round trip is the typed capability gap (exit 3), got {}: {}\n{}",
        observed.code,
        observed.stdout,
        observed.stderr
    );
    assert!(
        observed.stderr.contains("unsupported"),
        "the diagnostic must name the capability gap: {}",
        observed.stderr
    );
    assert!(
        observed.stdout.trim().is_empty(),
        "nothing may be written to the stream: {}",
        observed.stdout
    );
}
