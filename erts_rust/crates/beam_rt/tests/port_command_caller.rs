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

//! **A port command's reply belongs to the process that issued it** (contract A.4), end to end
//! and cross-checked against the live `erl` oracle.
//!
//! # The two rules this file pins
//!
//! 1. **The recipient is the operation's issuer, not the port's owner.** ERTS sends a completed
//!    command's `{inet_reply, Port, Status[, Tag]}` to `desc->caller` — the process that ran the
//!    `port_command/2` — and clears that field as it sends it
//!    (`inet_reply_finish`, `../../erts/emulator/drivers/common/inet_drv.c:2614-2637`, set by
//!    every `tcp_inet_command*`). A queued **asynchronous** operation carries the same fact per
//!    operation — `enq_async`'s `opp->caller = driver_caller(desc->port)`
//!    (`inet_drv.c:2427`), which `send_async_ok`/`send_async_error` address — so a passive
//!    `gen_tcp:recv/3`'s `{inet_async, Port, Id, Result}` belongs to the process that issued the
//!    read as well. Any process may command or read a port another process owns, so addressing
//!    the completion to the owner leaves the real issuer blocked in `prim_inet:send/4`'s
//!    `receive {inet_reply, S, Status, Mref}` — or in `prim_inet:recv0/3`'s `after`-less
//!    `receive {inet_async, S, Ref, Status}` — for ever.
//! 2. **A command issued inside a scheduler slice must park on a route that can be resumed.**
//!    The reply is a *message*, so the issuer's `receive` has to suspend the **process**. A
//!    callee entered on the nested `call_bif` route inside a slice can never be advanced (that
//!    context may not pump the scheduler), and its `receive` folds into the typed `blocked()`
//!    gap — which is why the per-function fun-image half of the call-site guard yields to the
//!    in-machine entry exactly there (`context::apply::fun_image_guard_yields_for_a_slice`,
//!    consumed by `context::execution::resolve_external_call`).
//!
//! # The matrix: what each row pins
//!
//! | Case | The rule it pins |
//! |---|---|
//! | [`a_spawned_sender_is_resumed_by_its_own_reply`] | a spawned `gen_tcp:send/2` returns `ok`: the callee enters **in-machine** (`prim_inet:send/3` issues `term_to_binary/2`, so the call-site guard's per-function arm yields to the in-machine entry where the context cannot pump), its `receive` suspends the process, and the reply resumes it |
//! | [`a_spawned_command_reply_carries_its_own_tag_and_never_reaches_the_owner`] | the `{inet_reply, S, ok, Tag}` carries the **issuer's** own `Mref` and the owner receives nothing |
//! | [`two_concurrent_senders_each_get_their_reply`] | a second, concurrent issuer on one port still gets its own reply |
//! | [`a_read_from_a_process_that_is_not_the_controlling_process`] | the read twin: a **non-owner**'s passive `gen_tcp:recv/3` is answered by its own `{inet_async, …}` while the owner stays alive |
//! | [`a_send_on_a_closed_socket_is_closed`], [`a_send_after_the_peer_closed_is_closed`] | a command's negative answer is a reply too — `{error,closed}` instead of a hang |
//! | [`an_untagged_command_gets_no_reply_and_the_port_dies_of_einval`] | a command carrying **no** caller tag produces **no** reply: the C's `init_caller` failure is `driver_failure_posix(…, EINVAL)` (`inet_drv.c:12635-12638`) |
//!
//! # What is compared
//!
//! Each probe runs under the built `beam_rt` binary **and** under the system `erl`, with the
//! **exit code** and the **stdout bytes** required to be equal, and the bytes then pinned
//! exactly, so the suite still discriminates without a toolchain. A probe is either one `-eval`
//! expression ([`support::differential_eval`]) or a compiled fixture on the `run` route (its
//! `run` action prints the target's own result after the probe's line). A missing `erl` is a
//! **loud** skip ([`support::skip_loudly`]), never a silent pass.
//!
//! Each probe additionally asserts the **essential invariant** rather than only the text: the
//! run exits `0` and its stderr never carries the `process blocked` verdict, i.e. the process
//! that issued the command or the read was *resumed* and not left parked.
//!
//! # Determinism (no volatile value is pinned, no probe can hang)
//!
//! Every socket is loopback-only (`{ip, {127,0,0,1}}`, port `0`) and the ephemeral port is
//! never pinned; every socket and port is closed before the probe answers. A wait that must let
//! the other side's socket operation arm first is a **bounded** `receive after 300 -> ok end`
//! (never a `timer:sleep`, never an unbounded wait), and every probe-side wait carries its own
//! bound, so a lost reply or a lost read shows up as a visible mismatch. [`support::PROBE_TIMEOUT`]
//! additionally kills a child that never answers.
//!
//! # Where the read-side cases live
//!
//! The read twin is pinned here ([`a_read_from_a_process_that_is_not_the_controlling_process`]):
//! a non-owner's passive `gen_tcp:recv/3` is answered because the port layer records **per
//! operation** who issued it (`PortMeta::async_callers`, taken when that id's `{inet_async, …}` is
//! rendered) and addresses the completion there.
//!
//! A read of data that was already buffered before the read operation was issued, with a
//! `timeout 0`, is [`../inet_async_caller.rs`](../inet_async_caller.rs)'s `b` case, and it answers
//! the oracle's `{ok, <<"hi">>}` on both the owner and a non-owner reader: the driver attempts the
//! receive **inside** its control call (the C's `tcp_recv(desc, n)`, `inet_drv.c:12310`), so the
//! bytes are in hand before any read interest is armed and nothing is left for a readiness event
//! to supply.

// The shared harness is compiled in whole; this suite uses its bounded runner, its toolchain
// discovery and its `-eval` differential.
#[allow(dead_code)]
mod support;

use support::EvalProbe;

// ---------------------------------------------------------------------------------------
// The probes and the pinned assertions
// ---------------------------------------------------------------------------------------

/// Runs one probe differentially ([`support::differential_eval`]) and asserts the pinned
/// observable: CLI exit `0`, the exact **stdout** bytes, and — the essential invariants — a
/// stderr that carries neither the pre-fix `process blocked` verdict nor the runtime's own
/// internal plumbing text (`term not found`, the `beam_process`
/// [`unknown_process`](beam_process) diagnostic that an untagged command's death used to leak
/// when the driver went on evaluating in a process that no longer existed).
///
/// The last assertions are what make a *hang* or an *internal string* visible even if a future
/// change made the two runtimes agree on some other text: a process left parked after issuing a
/// port command, and a route that reports its own bookkeeping instead of an Erlang-level answer,
/// are exactly the failures this file exists to catch.
fn pinned(what: &str, expr: &str, expected: &str) -> EvalProbe {
    let run = support::differential_eval("port_command_caller", what, expr);
    assert_eq!(
        run.code, 0,
        "{what}: beam_rt must exit 0 — got {} (stdout {:?}, stderr {:?})",
        run.code,
        run.stdout_text(),
        run.stderr_text()
    );
    assert_eq!(
        run.stdout_text(),
        expected,
        "{what}: stdout bytes differ — got {:?} (stderr {:?})",
        run.stdout_text(),
        run.stderr_text()
    );
    assert!(
        !run.stderr_text().contains("blocked"),
        "{what}: the issuing process must be resumed, not left parked — stderr {:?}",
        run.stderr_text()
    );
    assert!(
        !run.stderr_text().contains("term not found"),
        "{what}: no internal plumbing text may reach the user — stderr {:?}",
        run.stderr_text()
    );
    run
}

/// `gen_tcp:listen/2` on loopback plus an accepted/client pair — the prefix of every probe
/// below, so each case reads as its own delta. The probe supplies the opening `begin` and this
/// list continues its body.
///
/// `{active, false}` keeps the socket passive: a read is then the caller's own `gen_tcp:recv/3`
/// and a write is the caller's own `port_command/2` (`prim_inet:send/3`), which is what makes the
/// *issuer* of the command observable at all.
const SETUP: &str = "\
    {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]), \
    {ok,Port} = inet:port(L), \
    {ok,Cl} = gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]), \
    {ok,A} = gen_tcp:accept(L), ";

/// The matching trailer: close the client, the accepted socket and the listener.
const TEARDOWN: &str = "gen_tcp:close(Cl), gen_tcp:close(A), gen_tcp:close(L), ";

// ---------------------------------------------------------------------------------------
// (1)+(2): the spawned sender is resumed, and its own command's reply reaches it
// ---------------------------------------------------------------------------------------

/// A **spawned** process's `gen_tcp:send/2` returns `ok` and its bytes arrive, while the parent
/// (which owns the socket and issues the matching read) sees only its own data.
///
/// This is the shape that used to hang: `prim_inet:send/3` issues `term_to_binary/2`, so the
/// in-machine call-site guard refused it and the send ran on the nested route, where a scheduler
/// slice can never advance the scheduler — the bytes were written and the child was then killed
/// as `process blocked (no message available)`. Entered **in-machine** the child suspends on the
/// receive, the port round writes it, and the reply `{inet_reply, Cl, ok, Mref}` resumes it.
///
/// The child waits 300 ms before sending so the parent's `gen_tcp:recv/3` is **armed** when the
/// bytes arrive: this row pins the sender's own reply, while a read armed only *after* the payload
/// was buffered is a different shape — answered from the driver's receive inside the control call
/// (`inet_drv.c:12310`), pinned by [`../inet_async_caller.rs`](../inet_async_caller.rs)'s `b` case
/// and by [`../inet_nested.rs`](../inet_nested.rs)'s accepted-socket case.
#[test]
fn a_spawned_sender_is_resumed_by_its_own_reply() {
    let expr = format!(
        "begin {SETUP}\
         P = self(), \
         spawn(fun() -> receive after 300 -> ok end, \
                       P ! {{sent, gen_tcp:send(Cl, <<\"hi\">>)}} end), \
         Rc = gen_tcp:recv(A, 2, 3000), \
         Rs = receive {{sent,X}} -> X after 3000 -> no_reply end, \
         {TEARDOWN}\
         io:format(\"~p~n\",[{{Rc,Rs}}]) end"
    );
    pinned(
        "a spawned gen_tcp:send/2 is resumed by its own reply",
        &expr,
        "{{ok,<<\"hi\">>},ok}\n",
    );
}

// ---------------------------------------------------------------------------------------
// (1): the reply carries the issuer's tag and never reaches the owner
// ---------------------------------------------------------------------------------------

/// `erlang:port_command/2` from a spawned process is answered with `{inet_reply, Port, ok, Tag}`
/// carrying **that process's own** tag (`prim_inet:send/4`'s `Mref`), and the port's **owner**
/// receives no reply at all.
///
/// The tag is the decoded `term_to_binary(Mref, [local])` image the issuer wrote in the C
/// driver's `<<Size:16, TagBin/binary, Data/binary>>` command framing, so the `true` in the
/// answer is a real round trip of the *caller's* reference — an owner-addressed reply could never
/// reproduce it.
#[test]
fn a_spawned_command_reply_carries_its_own_tag_and_never_reaches_the_owner() {
    let expr = format!(
        "begin {SETUP}\
         P = self(), \
         spawn(fun() -> receive after 300 -> ok end, \
                       Mref = erlang:monitor(port, Cl), \
                       MrefBin = term_to_binary(Mref, [local]), \
                       Size = byte_size(MrefBin) band 16#FFFF, \
                       true = erlang:port_command(Cl, [<<Size:16, MrefBin/binary>>, <<\"x\">>]), \
                       R = receive {{inet_reply, Cl, S, T}} -> {{tagged, S, T =:= Mref}}; \
                                     {{inet_reply, Cl, S}} -> {{untagged, S}} \
                               after 3000 -> no_reply end, \
                       erlang:demonitor(Mref, [flush]), \
                       P ! {{child, R}} end), \
         Rc = gen_tcp:recv(A, 1, 3000), \
         Rs = receive {{child,X}} -> X after 3000 -> no_reply end, \
         Owner = receive {{inet_reply, Cl, _, _}} -> owner_got_reply \
                   after 300 -> no_owner_reply end, \
         {TEARDOWN}\
         io:format(\"~p~n\",[{{Rc,Rs,Owner}}]) end"
    );
    pinned(
        "a spawned port_command/2 reply carries its own tag and skips the owner",
        &expr,
        "{{ok,<<\"x\">>},{tagged,ok,true},no_owner_reply}\n",
    );
}

// ---------------------------------------------------------------------------------------
// (1): two concurrent issuers on one port
// ---------------------------------------------------------------------------------------

/// Two spawned senders on the **same** socket each get their own `ok`.
///
/// The port records one `desc->caller` per command (the C's single `desc->caller` field), so a
/// runtime that addressed the reply to the owner would answer neither of them. The receive side
/// is deliberately not observed — this row pins the pair of *replies*, and the read shapes have
/// their own suites ([`../inet_async_caller.rs`](../inet_async_caller.rs),
/// [`../inet_nested.rs`](../inet_nested.rs)).
#[test]
fn two_concurrent_senders_each_get_their_reply() {
    let expr = format!(
        "begin {SETUP}\
         P = self(), \
         spawn(fun() -> P ! {{s1, gen_tcp:send(Cl, <<\"a\">>)}} end), \
         spawn(fun() -> P ! {{s2, gen_tcp:send(Cl, <<\"b\">>)}} end), \
         C1 = receive {{s1,X1}} -> X1 after 3000 -> no_reply end, \
         C2 = receive {{s2,X2}} -> X2 after 3000 -> no_reply end, \
         {TEARDOWN}\
         io:format(\"~p~n\",[{{C1,C2}}]) end"
    );
    pinned(
        "two concurrent senders on one socket each get their reply",
        &expr,
        "{ok,ok}\n",
    );
}

// ---------------------------------------------------------------------------------------
// (1): a command's negative answers are replies too
// ---------------------------------------------------------------------------------------

/// A `gen_tcp:send/2` on a **closed** socket answers `{error,closed}` rather than hanging: the
/// driver still owes the issuer a reply, so a mis-addressed one would park the caller instead of
/// reporting the closed stream.
#[test]
fn a_send_on_a_closed_socket_is_closed() {
    let expr = format!(
        "begin {SETUP}\
         ok = gen_tcp:close(Cl), \
         R = gen_tcp:send(Cl, <<\"hi\">>), \
         gen_tcp:close(A), gen_tcp:close(L), \
         io:format(\"~p~n\",[R]) end"
    );
    pinned(
        "gen_tcp:send/2 on a closed socket answers {error,closed}",
        &expr,
        "{error,closed}\n",
    );
}

/// A send **after the peer closed** answers `{error,closed}`, and so does the read that observes
/// the close first.
///
/// The read is what makes this deterministic: it consumes the peer's FIN and settles the port's
/// state, so the following send is answered from a known-closed socket instead of racing the
/// kernel's RST (the first write after a bare `close` of the peer legitimately succeeds).
#[test]
fn a_send_after_the_peer_closed_is_closed() {
    let expr = format!(
        "begin {SETUP}\
         ok = gen_tcp:close(A), \
         Rc = gen_tcp:recv(Cl, 0, 3000), \
         R = gen_tcp:send(Cl, <<\"hi\">>), \
         gen_tcp:close(Cl), gen_tcp:close(L), \
         io:format(\"~p~n\",[{{Rc,R}}]) end"
    );
    pinned(
        "a send after the peer closed answers {error,closed}",
        &expr,
        "{{error,closed},{error,closed}}\n",
    );
}

// ---------------------------------------------------------------------------------------
// (2): no tag means no reply — and the driver's own failure
// ---------------------------------------------------------------------------------------

/// A command carrying **no caller tag** produces **no** `{inet_reply, …}`: the C driver's
/// `init_caller` fails on the untagged buffer and the port dies of `EINVAL`
/// (`driver_failure_posix`, `inet_drv.c:12635-12638`), so the owner's next act is the `'EXIT'`
/// its link owes it — never a reply.
///
/// The owner traps exits so the port's death is *observable* as the linked `'EXIT'` instead of
/// being a non-trapping link's own termination; the oracle answers the identical
/// `{true, no_reply, {exit_reason, einval}, undefined}`.
#[test]
fn an_untagged_command_gets_no_reply_and_the_port_dies_of_einval() {
    let expr = format!(
        "begin process_flag(trap_exit, true), {SETUP}\
         R = erlang:port_command(Cl, <<\"raw\">>), \
         Reply = receive {{inet_reply, Cl, S, T}} -> {{tagged, S, T}}; \
                          {{inet_reply, Cl, S}} -> {{untagged, S}} \
                    after 500 -> no_reply end, \
         Exit = receive {{'EXIT', Cl, Why}} -> {{exit_reason, Why}} after 300 -> no_exit end, \
         Info = erlang:port_info(Cl), \
         gen_tcp:close(A), gen_tcp:close(L), \
         io:format(\"~p~n\",[{{R,Reply,Exit,Info}}]) end"
    );
    pinned(
        "an untagged port_command/2 gets no reply and kills the port with einval",
        &expr,
        "{true,no_reply,{exit_reason,einval},undefined}\n",
    );
}

// ---------------------------------------------------------------------------------------
// (1) the read twin: a non-owner's passive read is answered by its own operation
// ---------------------------------------------------------------------------------------

/// A passive `gen_tcp:recv/3` issued by a process that is **not** the socket's controlling
/// process returns the peer's data — the read twin of the reply rule above.
///
/// A passive read is not a control-op answer: `prim_inet:recv0/3` receives an operation id back
/// and then `receive`s the driver's `{inet_async, Port, Id, Result}` with **no** `after`, and the
/// C addresses that completion to the operation's issuer (`enq_async`'s
/// `opp->caller = driver_caller(desc->port)`, `inet_drv.c:2427`). Addressed to the socket's owner
/// instead, the spawned reader below would never be woken — it would wait out its own 3 s driver
/// timeout and answer `no_answer` — so the pinned `recv={ok,<<"hi">>}` *is* the addressing rule.
///
/// The **owner stays alive** for the whole probe: it opens the pair, accepts, and issues the
/// write, so the reader's only distinguishing property is that it does not control the socket.
/// An owner that exits before the read is a different (and unpinnable) shape: the oracle answers
/// `{error, einval}` there — measured 3/3 on OTP 29.1.2 — because the socket's driver port dies
/// with its owner.
///
/// Driven on the **`run`** route, whose pass loop returns as soon as the target pid does
/// (`Runtime::pump_run_to_completion`), so a helper's bookkeeping can never turn the run into
/// `unsupported: run: process blocked` the way a parked child does on the `-eval` route. The
/// child is killed and every socket closed before the target returns, so nothing is left parked.
#[test]
fn a_read_from_a_process_that_is_not_the_controlling_process() {
    const SOURCE: &str = "\
-module(pc_recv).\n\
-export([go/0]).\n\
go() ->\n\
    {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]),\n\
    {ok,Port} = inet:port(L),\n\
    {ok,Cl} = gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),\n\
    {ok,A} = gen_tcp:accept(L),\n\
    P = self(),\n\
    Child = spawn(fun() -> P ! {r, gen_tcp:recv(Cl, 2, 3000)} end),\n\
    receive after 300 -> ok end,\n\
    ok = gen_tcp:send(A, <<\"hi\">>),\n\
    Ans = receive {r,X} -> X after 5000 -> no_answer end,\n\
    exit(Child, kill),\n\
    gen_tcp:close(Cl), gen_tcp:close(A), gen_tcp:close(L),\n\
    io:format(\"recv=~p~n\",[Ans]),\n\
    ok.\n";
    const EXPECTED: &str = "recv={ok,<<\"hi\">>}\n";
    let Some(fixture) = support::ErlangFixture::compile(
        "port_command_caller",
        "non_owner_recv",
        "pc_recv",
        SOURCE,
    ) else {
        return;
    };
    let Some(oracle) = support::resolve_oracle() else {
        support::skip_loudly("port_command_caller", "no `erl` oracle on $PATH");
        return;
    };
    let dir = fixture.dir().to_str().expect("the fixture dir is UTF-8");
    let beam = fixture.beam();
    let beam = beam.to_str().expect("the fixture path is UTF-8");
    let Some(oracle_run) = support::run_bounded(
        &oracle,
        &[
            "-noshell", "-pa", dir, "-s", "pc_recv", "go", "-s", "init", "stop",
        ],
    ) else {
        support::skip_loudly(
            "port_command_caller",
            "the oracle did not answer the non-owner read inside the probe timeout",
        );
        return;
    };
    assert_eq!(
        String::from_utf8_lossy(&oracle_run.stdout),
        EXPECTED,
        "the oracle's own answer moved — re-measure before trusting this row"
    );
    let Some(run) = support::run_runtime(&["-noshell", "run", beam, "pc_recv:go/0"]) else {
        support::skip_loudly(
            "port_command_caller",
            "the `run` action did not finish the non-owner read inside the probe timeout",
        );
        return;
    };
    assert_eq!(
        run.stdout,
        format!("{EXPECTED}ok\n"),
        "a non-owner's passive read must be answered with the peer's data — got {:?}, \
         stderr {:?}",
        run.stdout,
        run.stderr
    );
    assert_eq!(run.code, 0, "exit code — stderr {:?}", run.stderr);
    for forbidden in ["blocked", "unsupported", "term not found"] {
        assert!(
            !run.stderr.contains(forbidden),
            "the reader must be resumed, not parked or refused (`{forbidden}`) — stderr {:?}",
            run.stderr
        );
    }
}

// ---------------------------------------------------------------------------------------
// The untagged command: the port dies of `einval`, and the death is an Erlang-level answer
// ---------------------------------------------------------------------------------------
/// An **untagged** `erlang:port_command(Port, Data)` (no `<<Size:16, TagBin/binary>>` prefix) is
/// the C `init_caller` failure — `driver_failure_posix(desc, EINVAL)`
/// (`../../erts/emulator/drivers/common/inet_drv.c:12635-12638`) — so the port is killed with
/// reason `einval` and no `{inet_reply, …}` is ever sent.
///
/// The probe uses a **spawned, non-trapping** child which owns the socket, because the oracle's
/// own `-eval`/`-s` process traps exits (`init` sets `trap_exit=true`) and would survive the
/// death — the non-trapping matrix row is the spawned child, and the parent `monitor`s it so the
/// child's **exact** exit reason is observable, which is the crisp assertion this case pins:
/// `down=einval`, then no trailing message at all (`rest=timeout` — no reply, no `alive`, no
/// `{cmd, …}`, since the child dies at the command).
///
/// Two wrong answers this rules out, both of which reached the user before the fix: the internal
/// `term not found` (the driver kept evaluating in a process the port round had already removed)
/// and, on the `run` route, a hang ([`an_untagged_command_terminates_the_run_target`]).
#[test]
fn a_spawned_untagged_command_kills_the_child_with_the_ports_einval() {
    let expr = "begin \
         P = self(), \
         Child = spawn(fun() -> \
             {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]), \
             {ok,Port} = inet:port(L), \
             {ok,Cl} = gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]), \
             {ok,_A} = gen_tcp:accept(L), \
             true = erlang:port_command(Cl, <<\"raw\">>), \
             receive after 500 -> ok end, \
             P ! alive end), \
         MRef = erlang:monitor(process, Child), \
         Down = receive {'DOWN',MRef,process,_,W} -> W after 3000 -> nodown end, \
         Rest = receive alive -> alive; {inet_reply,_,_} -> reply \
                    after 800 -> timeout end, \
         io:format(\"down=~p rest=~p~n\",[Down,Rest]) end";
    pinned(
        "an untagged port_command/2 kills its own process with einval",
        expr,
        "down=einval rest=timeout\n",
    );
}

/// The `run` action's target is **not** a trapping process, so the same untagged command must
/// terminate it with the port's own reason — the CLI then renders the target's abnormal exit on
/// stderr (`beam_rt: error einval`) and exits `1`.
///
/// This is the regression pin for the second wrong answer: the death signal used to reach the
/// process table and vanish, so the run round kept waiting for a target that no longer existed
/// and the CLI ended at `RUN_DEADLINE` (60 s) with `run: pc:go/0 did not terminate within
/// 60000 ms`. The assertion is therefore *both* the wording and **the clock**: a bounded
/// completion far inside [`support::PROBE_TIMEOUT`] is the invariant, never a 60 s wait.
///
/// Not oracle-compared: the oracle's `-s`/`-run` action process traps exits, so it survives the
/// death (`cmd=true` then `alive`) — an own-state difference this crate documents. The
/// oracle-comparable half of the matrix is the spawned-child case above.
#[test]
fn an_untagged_command_terminates_the_run_target() {
    const SOURCE: &str = "\
-module(pc_run).\n\
-export([go/0]).\n\
go() ->\n\
    {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]),\n\
    {ok,Port} = inet:port(L),\n\
    {ok,Cl} = gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]),\n\
    {ok,_A} = gen_tcp:accept(L),\n\
    R = erlang:port_command(Cl, <<\"raw\">>),\n\
    io:format(\"cmd=~p~n\",[R]),\n\
    receive after 500 -> ok end,\n\
    io:format(\"alive~n\",[]),\n\
    ok.\n";
    let Some(fixture) = support::ErlangFixture::compile(
        "port_command_caller",
        "untagged_run_target",
        "pc_run",
        SOURCE,
    ) else {
        return;
    };
    let beam = fixture.beam();
    let beam = beam.to_str().expect("the fixture path is UTF-8");
    let started = std::time::Instant::now();
    let Some(run) = support::run_runtime(&["run", beam, "pc_run:go/0"]) else {
        support::skip_loudly(
            "port_command_caller",
            "the `run` action did not finish inside the probe timeout",
        );
        return;
    };
    let elapsed = started.elapsed();
    assert_eq!(
        run.stdout, "cmd=true\n",
        "the command itself is accepted (`true`), and the target must not reach the second \
         statement — stderr {:?}",
        run.stderr
    );
    assert_eq!(
        run.code, 1,
        "the target's abnormal exit is a runtime error, never a hang and never success — \
         stderr {:?}",
        run.stderr
    );
    assert!(
        run.stderr.contains("einval"),
        "the target's exit reason is the port's own `einval` — stderr {:?}",
        run.stderr
    );
    assert!(
        !run.stderr.contains("term not found") && !run.stderr.contains("blocked"),
        "no internal plumbing text may reach the user — stderr {:?}",
        run.stderr
    );
    assert!(
        elapsed < std::time::Duration::from_secs(15),
        "the run must report the death, not wait out the deadline — took {elapsed:?}"
    );
}
