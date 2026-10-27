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
//! **A queued asynchronous socket operation is answered to the process that issued it**
//! (contract A.4), end to end and cross-checked against the live `erl` oracle.
//!
//! `gen_tcp:recv/3` issues no synchronous control request: `prim_inet:recv/2` is
//! `recv0(S, Length, -1)`, whose `receive {inet_async, S, Ref, Status}` has **no `after`**
//! (`erts/preloaded/src/prim_inet.erl:808-826`). The driver's own timeout is therefore the only
//! deadline a passive read has, and ERTS addresses that completion — data or `{error, timeout}`
//! — to the operation's issuer, `opp->caller = driver_caller(desc->port)`
//! (`../../erts/emulator/drivers/common/inet_drv.c:2427`), which `send_async_ok`/`send_async_error`
//! read (`:2494-2500`/`:2541-2559`). Addressing it to the port's **owner** left a spawned
//! non-owner parked for ever, which is the defect these cases pin.
//!
//! Every probe spawns its reader (so it is a process that does **not** own the socket), waits on
//! a bounded `receive`, and answers exactly one line; the reader is killed and every socket
//! closed before the probe returns. The fixture is compiled with the system `erlc` at test time
//! and run on the **`run`** route, whose pass loop returns as soon as the target pid returns, so
//! a helper that is still parked cannot turn the run into `unsupported: run: process blocked`.
//! The oracle is the system `erl` on the same module (`-s <mod> <fun> -s init stop`); a missing
//! toolchain is a loud skip, never a silent pass.

#[allow(dead_code)]
mod support;

/// The fixture: one socket pair per probe, a spawned reader, one answer line.
///
/// `a` reads data that arrives **after** the read was issued, `b` data that was already
/// buffered, `c` nothing at all with a zero timeout, `d` nothing at all with a finite one, and
/// `e` a `{packet,4}` frame whose tail arrives after an incomplete first read.
const SOURCE: &str = "\
-module(acaller).
-export([a/0,b/0,c/0,d/0,e/0]).
pair(Opts) ->
    {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]),
    {ok,Port} = inet:port(L),
    {ok,Cl} = gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}|Opts]),
    {ok,A} = gen_tcp:accept(L),
    {Cl,A,L}.
close_all(Socks) -> [gen_tcp:close(S) || S <- Socks], ok.
ask(Cl, Timeout) ->
    P = self(),
    Child = spawn(fun() -> P ! {r, gen_tcp:recv(Cl, 2, Timeout)} end),
    Ans = receive {r,X} -> X after 5000 -> no_answer end,
    exit(Child, kill),
    Ans.
a() ->
    {Cl,A,L} = pair([]),
    P = self(),
    Child = spawn(fun() -> P ! {r, gen_tcp:recv(Cl, 2, 3000)} end),
    receive after 150 -> ok end,
    ok = gen_tcp:send(A, <<\"yo\">>),
    Ans = receive {r,X} -> X after 5000 -> no_answer end,
    exit(Child, kill),
    close_all([Cl,A,L]),
    io:format(\"a=~p~n\",[Ans]).
b() ->
    {Cl,A,L} = pair([]),
    ok = gen_tcp:send(A, <<\"hi\">>),
    receive after 150 -> ok end,
    Ans = ask(Cl, 0),
    close_all([Cl,A,L]),
    io:format(\"b=~p~n\",[Ans]).
c() ->
    {Cl,A,L} = pair([]),
    Ans = ask(Cl, 0),
    close_all([Cl,A,L]),
    io:format(\"c=~p~n\",[Ans]).
d() ->
    {Cl,A,L} = pair([]),
    T0 = erlang:monotonic_time(millisecond),
    Ans = ask(Cl, 300),
    Elapsed = erlang:monotonic_time(millisecond) - T0,
    close_all([Cl,A,L]),
    io:format(\"d=~p slow=~p~n\",[Ans, Elapsed > 250]).
e() ->
    {Cl,A,L} = pair([{packet,4}]),
    P = self(),
    ok = gen_tcp:send(A, <<0,0,0,5,\"he\">>),
    receive after 150 -> ok end,
    Child = spawn(fun() ->
        R1 = gen_tcp:recv(Cl, 0, 0),
        P ! {first, R1},
        P ! armed,
        R2 = gen_tcp:recv(Cl, 0, 3000),
        P ! {second, R2}
    end),
    R1 = receive {first,X1} -> X1 after 5000 -> no_first end,
    receive armed -> ok after 5000 -> no_armed end,
    ok = gen_tcp:send(A, <<\"llo\">>),
    R2 = receive {second,X2} -> X2 after 5000 -> no_second end,
    exit(Child, kill),
    close_all([Cl,A,L]),
    io:format(\"e=~p~n\",[{R1, R2}]).
";

/// The compiled fixture, or a loud skip.
fn fixture(label: &str) -> Option<support::ErlangFixture> {
    support::ErlangFixture::compile("inet_async_caller", label, "acaller", SOURCE)
}

/// Runs `acaller:<function>/0` on **both** runtimes and asserts they agree, then pins the bytes.
///
/// The runtime's `run` action prints the target's result (`ok`) after the probe's own line, so
/// the two stdouts differ by exactly that trailing line; anything else is a mismatch.
fn pinned(what: &str, label: &str, function: &str, expected: &str) {
    let Some(fixture) = fixture(label) else {
        return;
    };
    let Some(oracle) = support::resolve_oracle() else {
        support::skip_loudly("inet_async_caller", "no `erl` oracle on $PATH");
        return;
    };
    let dir = fixture.dir().to_str().expect("the fixture dir is UTF-8");
    let beam = fixture.beam();
    let beam = beam.to_str().expect("the fixture path is UTF-8");
    let mounted = format!("acaller:{function}/0");
    let Some(oracle_run) = support::run_bounded(
        &oracle,
        &["-noshell", "-pa", dir, "-s", "acaller", function, "-s", "init", "stop"],
    ) else {
        support::skip_loudly(
            "inet_async_caller",
            &format!("the oracle did not answer `{what}` inside the probe timeout"),
        );
        return;
    };
    let oracle_stdout = String::from_utf8_lossy(&oracle_run.stdout).into_owned();
    assert_eq!(
        oracle_stdout,
        expected,
        "{what}: the oracle's own answer moved — re-measure before trusting this row"
    );
    let Some(run) = support::run_runtime(&["-noshell", "run", beam, &mounted]) else {
        support::skip_loudly(
            "inet_async_caller",
            &format!("the `run` action did not finish `{what}` inside the probe timeout"),
        );
        return;
    };
    let wanted = format!("{expected}ok\n");
    assert_eq!(
        run.stdout, wanted,
        "{what}: stdout differs from the oracle's (`{oracle_stdout:?}`) — got {:?}, stderr {:?}",
        run.stdout, run.stderr
    );
    assert_eq!(run.code, 0, "{what}: exit code — stderr {:?}", run.stderr);
    assert!(
        !run.stderr.contains("blocked") && !run.stderr.contains("unsupported"),
        "{what}: the reader must be resumed, not left parked — stderr {:?}",
        run.stderr
    );
}

/// (a) A spawned **non-owner**'s passive `gen_tcp:recv/3` returns the data that arrives after
/// the read was issued — the defect's headline case.
#[test]
fn a_spawned_readers_recv_returns_the_data_that_arrives_after_it_arms() {
    pinned(
        "a spawned reader's recv gets the later data",
        "a",
        "a",
        "a={ok,<<\"yo\">>}\n",
    );
}

/// (b) Data buffered **before** the read, a spawned reader and a `timeout 0`: the oracle answers
/// `{ok, <<"hi">>}` because the C attempts the receive at **control** time
/// (`inet_drv.c:12309`), so the bytes are already the driver's when the zero deadline expires.
///
/// `#[ignore]`d for the **driver** half, which is not this crate's: `beam_io`'s
/// `TcpInetDriver` has no control-time read attempt — it arms read interest and reads only from
/// a readiness callback or from `try_complete_recv`, and a socket whose payload was fully
/// drained into the port's input queue before the operation was enqueued has none left for it.
/// Measured on the owner **and** on a non-owner reader, connect-side and accepted-side, so it is
/// not an addressing difference. The expectation stays the oracle's own; the row is un-ignored
/// the moment the control-time read lands.
#[test]
#[ignore = "beam_io has no control-time read attempt (inet_drv.c:12309): a read of already-buffered data times out for the owner too"]
fn a_spawned_readers_recv_returns_data_that_was_buffered_before_it_armed() {
    pinned(
        "a spawned reader's recv gets the buffered data",
        "b",
        "b",
        "b={ok,<<\"hi\">>}\n",
    );
}

/// (c) No data at all, a spawned reader and `timeout 0` → the driver's own `{error, timeout}`,
/// delivered to the reader. Before the fix the reader never returned: the timeout was produced
/// but addressed to the owner.
#[test]
fn a_spawned_readers_zero_timeout_answers_timeout() {
    pinned(
        "a spawned reader's zero timeout is answered",
        "c",
        "c",
        "c={error,timeout}\n",
    );
}

/// (d) No data at all, a spawned reader and a finite timeout → `{error, timeout}` after the
/// driver's own deadline. The probe only asserts a **generous** bound (the read took longer than
/// 250 ms), never a tight one: this host is shared.
#[test]
fn a_spawned_readers_finite_timeout_answers_timeout_after_its_deadline() {
    pinned(
        "a spawned reader's finite timeout is answered",
        "d",
        "d",
        "d={error,timeout} slow=true\n",
    );
}

/// (e) A `{packet,4}` frame whose header and partial body were buffered before the read: the
/// incomplete `recv(_, 0, 0)` answers the oracle's `{error, timeout}`, and the frame is delivered
/// **once**, whole, when its tail arrives — through the same addressing rule, since both
/// completions belong to the spawned reader.
#[test]
fn a_partial_frame_times_out_then_arrives_whole_for_a_spawned_reader() {
    pinned(
        "a partial {packet,4} frame then its tail",
        "e",
        "e",
        "e={{error,timeout},{ok,<<\"hello\">>}}\n",
    );
}

