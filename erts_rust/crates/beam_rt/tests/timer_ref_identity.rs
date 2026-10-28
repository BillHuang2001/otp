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
//! **A reference's identity is its id, not the address of its term** — end to end, on the
//! `erlang:cancel_timer/1,2` / `erlang:read_timer/1,2` path and on the `inet` resolver that
//! depends on it.
//!
//! # The defect this pins
//!
//! `Timers` used to recognise a timer reference **only** by the term word it was scheduled with.
//! A reference's identity, though, is its `id` word (the workspace has ONE reference-id space,
//! `beam_common::next_ref_id`), so a reference that was *copied* — round-tripped through ETS, sent
//! in a message — is the same reference at a different address, and the word-keyed table missed
//! it. The timer BIFs then classified the copy as a foreign reference and raised `badarg`.
//!
//! That is not a hypothetical: `lib/kernel/src/inet_gethost_native.erl` mints
//! `TimerRef = erlang:start_timer(Timeout, self(), RID)`, stores it in an `ets` row (`:365-368`)
//! and, when the port program answers, cancels **the copy it reads back out of ETS**
//! (`:270-275`, `_ = erlang:cancel_timer(TimerRef, [{async,true},{info,false}])`). The `badarg`
//! killed the resolver worker, and because `inet_gethost_native_sup` is registered with
//! `{restart_type,temporary}` **nothing restarted it**: `whereis(inet_gethost_native_sup)` became
//! `undefined` and every later hostname lookup answered `{error,nxdomain}` for the life of the
//! node, with `=ERROR REPORT`/`=SUPERVISOR REPORT` noise on stdout.
//!
//! # What is compared
//!
//! The fixture ([`FIXTURE_ERL`]) is compiled at test time by the system `erlc` and run on the
//! `-s` route under **both** runtimes; the two stdouts must be byte-identical, and the table is
//! additionally pinned literally so the suite still discriminates with no toolchain. Every value
//! a healthy run answers is normalised to a **shape** (`remaining_ms`, `ok`, `false`, the address
//! itself), so the comparison never depends on wall-clock time or on a pid's serial.
//!
//! A missing `erl`/`erlc` is a **loud** skip, never a silent pass.

#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::Output;

/// The suite's name in the loud-skip banner.
const SUITE: &str = "timer reference identity";

/// The fixture: the ETS round trip of a **timer reference**, then the `inet` probes that stand on
/// it.
///
/// `value/1` renders a probe's answer as a shape, so an exception is a value in the transcript
/// (`{raise,badarg}`) rather than a crashed process — the whole point being that a `badarg` must
/// show up as a difference instead of as a report block on stdout.
const FIXTURE_ERL: &str = r#"
-module(timerref).
-export([main/0]).

main() ->
    timer_lines(),
    lookup_lines(),
    ok.

timer_lines() ->
    Tab = ets:new(timerref, [set]),
    Cancel = erlang:start_timer(60000, self(), cancel_me),
    ets:insert(Tab, {cancel, Cancel}),
    [{cancel, CancelCopy}] = ets:lookup(Tab, cancel),
    io:format("timer.copy_is_equal=~p~n", [Cancel =:= CancelCopy]),
    io:format("timer.cancel=~p~n", [value(fun() -> erlang:cancel_timer(CancelCopy) end)]),
    io:format("timer.cancel_again=~p~n", [value(fun() -> erlang:cancel_timer(Cancel) end)]),
    Read = erlang:start_timer(60000, self(), read_me),
    ets:insert(Tab, {read, Read}),
    [{read, ReadCopy}] = ets:lookup(Tab, read),
    io:format("timer.read=~p~n", [value(fun() -> erlang:read_timer(ReadCopy) end)]),
    io:format("timer.read_async=~p~n",
              [value(fun() -> erlang:read_timer(ReadCopy, [{async, true}]) end)]),
    io:format("timer.cancel_async=~p~n",
              [value(fun() -> erlang:cancel_timer(ReadCopy, [{async, true}, {info, false}]) end)]),
    io:format("timer.read_after_cancel=~p~n",
              [value(fun() -> erlang:read_timer(ReadCopy) end)]).

lookup_lines() ->
    io:format("getaddr.inet=~p~n", [value(fun() -> inet:getaddr("127.0.0.1", inet) end)]),
    io:format("getaddr.inet6=~p~n", [value(fun() -> inet:getaddr("::1", inet6) end)]),
    io:format("getaddr.unknown=~p~n",
              [value(fun() -> inet:getaddr("no-such-host-xyz", inet) end)]),
    io:format("supervisor.alive=~p~n", [is_pid(whereis(inet_gethost_native_sup))]),
    connect_lines().

connect_lines() ->
    {ok, Listen} = gen_tcp:listen(0, [{active, false}]),
    {ok, Port} = inet:port(Listen),
    ok = gen_tcp:close(Listen),
    io:format("connect.string=~p~n",
              [value(fun() -> gen_tcp:connect("127.0.0.1", Port, [], 2000) end)]),
    io:format("connect.tuple=~p~n",
              [value(fun() -> gen_tcp:connect({127,0,0,1}, Port, [], 2000) end)]).

value(Fun) ->
    try Fun() of
        Value -> shape(Value)
    catch
        error:Reason -> {raise, Reason}
    end.

shape(ok) -> ok;
shape(false) -> false;
shape(Value) when is_integer(Value), Value >= 0 -> remaining_ms;
shape(Value) -> Value.
"#;

/// The transcript both runtimes must print, pinned.
///
/// `timer.copy_is_equal=true` on **both** sides: a reference compares by its content, so a copy is
/// `=:=` the original even though it is a different word — the two runtimes agree, which is what
/// makes the word-keyed lookup the anomaly. `remaining_ms` stands for the remaining milliseconds
/// (a real number, so only its shape is comparable), and `connect.*` names a listening port that
/// the fixture has just closed, so the refusal is the oracle's own `econnrefused`.
const EXPECTED: &str = "\
timer.copy_is_equal=true
timer.cancel=remaining_ms
timer.cancel_again=false
timer.read=remaining_ms
timer.read_async=ok
timer.cancel_async=ok
timer.read_after_cancel=false
getaddr.inet={ok,{127,0,0,1}}
getaddr.inet6={ok,{0,0,0,0,0,0,0,1}}
getaddr.unknown={error,nxdomain}
supervisor.alive=true
connect.string={error,econnrefused}
connect.tuple={error,econnrefused}
";

/// Compiles [`FIXTURE_ERL`], or returns `None` after the loud skip.
fn fixture(label: &str) -> Option<support::ErlangFixture> {
    support::ErlangFixture::compile(SUITE, label, "timerref", FIXTURE_ERL)
}

/// Runs the fixture's `main/0` on the module's own `-s` route under `program`.
///
/// The oracle needs an explicit `-s init stop` to leave the node (`-noshell` alone runs for
/// ever), while this runtime's `run`/`-s` action exits with the program — so the terminator is
/// part of the *oracle* invocation only.
fn run_main(program: &Path, dir: &str, stop: bool) -> Output {
    let mut args = vec!["-noshell", "-pa", dir, "-s", "timerref", "main"];
    if stop {
        args.extend(["-s", "init", "stop"]);
    }
    support::run_bounded(program, &args).unwrap_or_else(|| {
        panic!(
            "`{}` could not run the timerref fixture to completion (killed at the {}s bound)",
            program.display(),
            support::PROBE_TIMEOUT.as_secs()
        )
    })
}

/// The built `beam_rt`, whose `-s` action exits on its own.
fn ours_main(dir: &str) -> Output {
    run_main(Path::new(env!("CARGO_BIN_EXE_beam_rt")), dir, false)
}

/// The live oracle, which must be told to stop.
fn oracle_main(oracle: &Path, dir: &str) -> Output {
    run_main(oracle, dir, true)
}

/// A report block or a `badarg` crash on stdout is the symptom this fix removes: the resolver
/// worker must not die, so neither may appear.
fn assert_no_crash_report(stdout: &str) {
    for needle in ["=ERROR REPORT", "=SUPERVISOR REPORT", "badarg"] {
        assert!(
            !stdout.contains(needle),
            "stdout carries `{needle}`, which means a process died where the oracle's did not:\n{stdout}"
        );
    }
}

/// The probe table: a copied timer reference is the same timer, and the `inet` resolver that
/// depends on it survives — compared byte-for-byte against the live oracle.
#[test]
fn the_ets_round_trip_probe_table_agrees_with_the_oracle() {
    let Some(fixture) = fixture("timer_ref_identity") else {
        return;
    };
    let dir = fixture.dir().to_str().expect("an ASCII scratch path").to_string();
    let ours = ours_main(&dir);
    let stdout = String::from_utf8_lossy(&ours.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&ours.stderr).into_owned();
    assert_eq!(ours.status.code(), Some(0), "the fixture exited non-zero: {stderr}");
    assert!(stderr.is_empty(), "a healthy run says nothing on stderr: {stderr}");
    assert_no_crash_report(&stdout);
    assert_eq!(stdout, EXPECTED, "the probe table this runtime answers");
    let Some(oracle) = support::resolve_oracle() else {
        support::skip_loudly(SUITE, "no `erl` on $PATH (and no executable $ERTS_ORACLE)");
        return;
    };
    let theirs = oracle_main(&oracle, &dir);
    let expected = String::from_utf8_lossy(&theirs.stdout).into_owned();
    assert_eq!(
        theirs.status.code(),
        Some(0),
        "the oracle fixture exited non-zero: {}",
        String::from_utf8_lossy(&theirs.stderr)
    );
    assert_eq!(
        stdout, expected,
        "the two runtimes print the same probe table (a copied timer reference is the same timer, \
         so the resolver survives and its answers match)"
    );
}

/// The property the defect broke, asserted on its own: **a failing lookup is silent and leaves
/// the resolver registered**, so a later lookup still works.
///
/// `inet_gethost_native_sup` is a `temporary` child of `kernel_safe_sup`, so a resolver worker
/// that dies is never restarted — the supervisor being a live pid after an `nxdomain` is the
/// observable that says the worker survived the `cancel_timer/2` on its ETS-stored reference.
#[test]
fn a_failing_lookup_is_silent_and_leaves_the_resolver_alive() {
    let Some(fixture) = fixture("timer_ref_resolver") else {
        return;
    };
    let dir = fixture.dir().to_str().expect("an ASCII scratch path").to_string();
    let observed = ours_main(&dir);
    let stdout = String::from_utf8_lossy(&observed.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&observed.stderr).into_owned();
    assert_eq!(
        observed.status.code(),
        Some(0),
        "the fixture exited non-zero: {stderr}"
    );
    assert!(stderr.is_empty(), "a failing lookup writes nothing on stderr: {stderr}");
    assert_no_crash_report(&stdout);
    assert!(
        stdout.contains("supervisor.alive=true"),
        "the resolver supervisor must still be registered after an nxdomain:\n{stdout}"
    );
    assert!(
        stdout.contains("getaddr.unknown={error,nxdomain}"),
        "an unresolvable name answers the oracle's `{{error,nxdomain}}`:\n{stdout}"
    );
}
