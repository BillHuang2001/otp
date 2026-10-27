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

//! **Group-leader capture** — an `io` output call addressed to a plain device process must reach
//! that process as an `{io_request, From, ReplyAs, Request}` and must **not** write to stdout.
//!
//! # The defect this suite exists for
//!
//! Every `io:format/1,2,3`, `io:put_chars/1,2`, `io:nl/0,1`, `io:write/1,2` and `io:fwrite/1,2,3`
//! is an `io.erl` wrapper over `o_request/2`, whose device is `group_leader()`
//! (`lib/stdlib/src/io.erl`'s `default_output/0`). A floor that answers those keys itself,
//! straight onto the runtime's own output stream, ignores the caller's group leader entirely —
//! so a **capture** leader (exactly how `eunit` collects a test's output,
//! `lib/eunit/src/eunit_proc.erl`'s `new_group_leader/1`) never sees the request and the bytes
//! leak onto the run's stream.
//!
//! The routing rule has three parts and this suite pins the observable half of each
//! (`src/context/io_route.rs` is the predicate):
//!
//! 1. a leader that **is** one of the runtime's own streams (the root pid, `user`, the registered
//!    `standard_error`) keeps the direct-sink fast path;
//! 2. any **other** live leader is served by OTP's own `io.erl` body running in-machine, so its
//!    `receive` suspends the process (`RunOutcome::Wait`) and the device's `{io_reply, …}`
//!    resumes it — no re-run, hence no double send;
//! 3. device failures keep the oracle's answers: a **dead** leader is `error:terminated`, and an
//!    `{error, Reason}` reply goes through `conv_reason/1` (here `{error, boom}` → `badarg`).
//!
//! # What is compared
//!
//! The fixture ([`FIXTURE_ERL`]) is compiled at test time by the system `erlc`
//! ([`support::ErlangFixture`]) and driven as a **scheduled** process through the built `beam_rt`
//! (`run <fixture.beam> iocap:<fun>/0`), so the call really crosses the external-call seam. Each
//! probe's `~w` line is compared to the live oracle's rendering **and** pinned, so the suite
//! still discriminates with no toolchain. A missing `erl`/`erlc` is a **loud** skip.
//!
//! The capture process never reports text it received — it reports the request's **shape** (the
//! tag, the encoding, and a byte/element count) — so "the marker is absent from stdout" is a real
//! assertion about where the bytes went, not a tautology about how they were rendered.

#[allow(dead_code)]
mod support;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "io leader capture";

/// The marker a leaked byte carries. It appears **only** in the requests the fixture's device
/// process receives, never in that process's report, so finding it on stdout means the call
/// bypassed the protocol.
const MARKER: &str = "CAPTURE-LEAK";

/// The fixture: a device process, a group-leader redirect, and the three failure shapes.
///
/// `capture/0` redirects the leader to a plain process, runs one call of each output family (the
/// explicit-device arity included) and then asks the device what it saw; `stderr_leader/0` points
/// the leader at the registered `standard_error` device; `dead_leader/0` and `error_reply/0` are
/// the two failure arms. Every arm restores the leader before returning, so the answer itself is
/// never swallowed by a dead or failing device.
const FIXTURE_ERL: &str = r#"
-module(iocap).
-export([capture/0, stderr_leader/0, dead_leader/0, error_reply/0]).

capture() ->
    Device = start_device(),
    Old = group_leader(),
    group_leader(Device, self()),
    R1 = io:format("~s", ["CAPTURE-LEAK-A"]),
    R2 = io:put_chars("CAPTURE-LEAK-B"),
    R3 = io:nl(),
    R4 = io:write({a, 1}),
    R5 = io:fwrite("~p", [[]]),
    R6 = io:format(Device, "~s", ["CAPTURE-LEAK-C"]),
    group_leader(Old, self()),
    {[R1, R2, R3, R4, R5, R6], stop_device(Device)}.

stderr_leader() ->
    Old = group_leader(),
    case whereis(standard_error) of
        undefined ->
            no_stderr_device;
        Device ->
            group_leader(Device, self()),
            R = io:put_chars("STDERR-LEAK"),
            group_leader(Old, self()),
            R
    end.

dead_leader() ->
    Old = group_leader(),
    Device = spawn(fun() -> ok end),
    timer:sleep(100),
    group_leader(Device, self()),
    R = try io:put_chars("DEAD-LEAK") of
            Ok -> {returned, Ok}
        catch
            Class:Reason -> {Class, Reason}
        end,
    group_leader(Old, self()),
    R.

error_reply() ->
    Old = group_leader(),
    Device = spawn(fun() ->
        receive
            {io_request, From, ReplyAs, _Request} ->
                From ! {io_reply, ReplyAs, {error, boom}}
        end
    end),
    group_leader(Device, self()),
    R = try io:put_chars("ERR-LEAK") of
            Ok -> {returned, Ok}
        catch
            Class:Reason -> {Class, Reason}
        end,
    group_leader(Old, self()),
    R.

start_device() ->
    spawn(fun() -> device_loop([]) end).

stop_device(Device) ->
    Device ! {report, self()},
    receive {seen, Seen} -> Seen after 5000 -> timeout end.

device_loop(Acc) ->
    receive
        {io_request, From, ReplyAs, Request} ->
            From ! {io_reply, ReplyAs, ok},
            device_loop([shape(Request) | Acc]);
        {report, From} ->
            From ! {seen, lists:reverse(Acc)},
            device_loop(Acc);
        _Other ->
            device_loop(Acc)
    end.

shape({put_chars, Enc, Bin}) when is_binary(Bin) ->
    {put_chars, Enc, binary, byte_size(Bin)};
shape({put_chars, Enc, Mod, Fun, Args}) when is_atom(Mod), is_atom(Fun), is_list(Args) ->
    {put_chars, Enc, Mod, Fun, length(Args)};
shape({put_chars, Enc, Chars}) when is_list(Chars) ->
    {put_chars, Enc, list, length(Chars)};
shape(Other) ->
    {other, Other}.
"#;

/// The oracle's rendering of `iocap:capture/0`, measured on OTP 29.1.2 / erts-17.1.1.
///
/// The shapes are the ones `io.erl`'s `io_request/2` + `bc_req/3` build for a **local** device
/// (`net_kernel:dflag_unicode_io/1` is true in-node): `format`/`fwrite`/`write` become the
/// `{put_chars, unicode, M, F, A}` form, `put_chars` becomes a **binary**, and `nl` stays a
/// one-character **list** (it is built by `bc_req/3`, not by the list-converting clause).
const CAPTURE_ANSWER: &str = "{[ok,ok,ok,ok,ok,ok],\
     [{put_chars,unicode,io_lib,format,2},\
     {put_chars,unicode,binary,14},\
     {put_chars,unicode,list,1},\
     {put_chars,unicode,io_lib,write,1},\
     {put_chars,unicode,io_lib,format,2},\
     {put_chars,unicode,io_lib,format,2}]}";

// ---------------------------------------------------------------------------------------
// The two sides of one probe
// ---------------------------------------------------------------------------------------

/// Compiles [`FIXTURE_ERL`], or returns `None` after the loud skip
/// [`support::ErlangFixture::compile`] already printed.
fn fixture() -> Option<support::ErlangFixture> {
    support::ErlangFixture::compile(SUITE, "io_leader_capture", "iocap", FIXTURE_ERL)
}

/// The fixture's `.beam` as an ASCII path (`run`'s `ARG` token is a plain string).
fn beam_arg(fixture: &support::ErlangFixture) -> String {
    let path = fixture.beam();
    path.to_str()
        .unwrap_or_else(|| panic!("the fixture path {} is not UTF-8", path.display()))
        .to_string()
}

/// Runs `iocap:<function>/0` as a **scheduled** process through the built `beam_rt` and answers
/// its transcript, asserting a clean exit.
fn ours(fixture: &support::ErlangFixture, function: &str) -> support::Observed {
    let beam = beam_arg(fixture);
    let spec = format!("iocap:{function}/0");
    let observed = support::run_runtime(&["-noshell", "run", &beam, &spec])
        .unwrap_or_else(|| panic!("`run {spec}` could not be run to completion"));
    assert_eq!(
        observed.code, 0,
        "iocap:{function}/0 exited {}: {}",
        observed.code, observed.stderr
    );
    observed
}

/// The live oracle's `~w` rendering of `iocap:<function>/0`, or `None` after a loud skip.
fn oracle(fixture: &support::ErlangFixture, function: &str) -> Option<String> {
    let Some(oracle) = support::resolve_oracle() else {
        support::skip_loudly(SUITE, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
        return None;
    };
    support::oracle_eval(
        &oracle,
        Some(fixture.dir()),
        &format!("io:format(\"~w~n\",[iocap:{function}()]), halt(0)."),
    )
}

/// Asserts this runtime's answer for `iocap:<function>/0` is the live oracle's, when the oracle
/// can answer.
fn agree_with_oracle(fixture: &support::ErlangFixture, function: &str, observed: &support::Observed) {
    if let Some(expected) = oracle(fixture, function) {
        assert_eq!(
            observed.stdout.trim(),
            expected,
            "iocap:{function}/0 must render what the live oracle renders"
        );
    }
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// A capture leader receives the request and **nothing** reaches stdout.
///
/// This is the regression the whole routing rule exists for: with the output served
/// unconditionally by the floor, `io:format` wrote straight to the runtime's output stream, so
/// the marker bytes appeared in the transcript and the device process never saw a request. Both
/// halves are asserted — the marker is absent from stdout, and the device saw one request per
/// call, in OTP's own shapes — plus the oracle's rendering, so the shapes cannot drift.
#[test]
fn a_capture_leader_takes_the_request_and_nothing_reaches_stdout() {
    let Some(fixture) = fixture() else {
        return;
    };
    let observed = ours(&fixture, "capture");
    let stdout = &observed.stdout;
    assert!(
        !stdout.contains(MARKER),
        "the capture leader's bytes leaked onto stdout ({MARKER}, fixture in {}):\n{stdout}",
        fixture.dir().display()
    );
    assert_eq!(
        stdout.trim(),
        CAPTURE_ANSWER,
        "the device must see one request per call, in OTP's shapes"
    );
    // The first element is the six reply values: the device answers `{io_reply, ReplyAs, ok}` and
    // `io.erl` returns it unchanged.
    assert!(
        stdout.trim().starts_with("{[ok,ok,ok,ok,ok,ok],"),
        "every call must answer the device's own reply, got:\n{stdout}"
    );
    agree_with_oracle(&fixture, "capture", &observed);
}

/// A leader that **is** the `standard_error` device keeps the direct sink: the bytes land on
/// fd 2 and stdout carries only the probe's own answer.
///
/// The kernel registers `standard_error` on every boot-driven route, and this is that identical
/// process rather than merely a name: `src/context/io_route.rs::io_leader_needs_the_protocol`
/// compares the leader by **pid** against `whereis(standard_error)`, the same classification
/// `native::io`'s device model uses for an explicit `standard_error` argument.
#[test]
fn a_standard_error_leader_writes_to_fd_2_and_never_to_stdout() {
    let Some(fixture) = fixture() else {
        return;
    };
    let observed = ours(&fixture, "stderr_leader");
    assert_eq!(
        observed.stdout.trim(),
        "ok",
        "stdout carries only the probe's own answer:\n{}",
        observed.stdout
    );
    assert!(
        !observed.stdout.contains("STDERR-LEAK"),
        "the marker must not reach stdout:\n{}",
        observed.stdout
    );
    assert!(
        observed.stderr.contains("STDERR-LEAK"),
        "the marker must be on fd 2:\n{}",
        observed.stderr
    );
    agree_with_oracle(&fixture, "stderr_leader", &observed);
}

/// A **dead** group leader is the oracle's `error:terminated` — never a silent write.
///
/// `io.erl`'s `execute_request/3` sends the request (a no-op to a dead pid) and waits on a
/// monitor that fires at once, which is its `terminated` clause; `o_request/2` re-raises that
/// through `conv_reason/1`. The dead leader is therefore *protocol* work, not a stream, and
/// classifying it as "the runtime's own output" is the one wrong answer available here.
#[test]
fn a_dead_leader_is_error_terminated() {
    let Some(fixture) = fixture() else {
        return;
    };
    let observed = ours(&fixture, "dead_leader");
    assert_eq!(observed.stdout.trim(), "{error,terminated}");
    assert!(
        !observed.stdout.contains("DEAD-LEAK"),
        "a dead leader must not receive the bytes on stdout:\n{}",
        observed.stdout
    );
    agree_with_oracle(&fixture, "dead_leader", &observed);
}

/// A leader replying `{error, boom}` is the oracle's `badarg` (`conv_reason/1`'s catch-all), not
/// the device's own reason and not a silent `ok`.
#[test]
fn a_leader_replying_with_an_error_is_badarg() {
    let Some(fixture) = fixture() else {
        return;
    };
    let observed = ours(&fixture, "error_reply");
    assert_eq!(observed.stdout.trim(), "{error,badarg}");
    assert!(
        !observed.stdout.contains("ERR-LEAK"),
        "a failing device must not leak its bytes onto stdout:\n{}",
        observed.stdout
    );
    agree_with_oracle(&fixture, "error_reply", &observed);
}
