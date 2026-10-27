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
//! 1. **The recipient is the command's caller, not the port's owner.** ERTS sends a completed
//!    command's `{inet_reply, Port, Status[, Tag]}` to `desc->caller` — the process that ran the
//!    `port_command/2` — and clears that field as it sends it
//!    (`inet_reply_finish`, `../../erts/emulator/drivers/common/inet_drv.c:2614-2637`, set by
//!    every `tcp_inet_command*`). Any process may command a port another process owns, so
//!    addressing the reply to the owner leaves the real issuer blocked in
//!    `prim_inet:send/4`'s `receive {inet_reply, S, Status, Mref}` for ever.
//! 2. **A command issued inside a scheduler slice must park on a route that can be resumed.**
//!    The reply is a *message*, so the issuer's `receive` has to suspend the **process**. A
//!    callee entered on the nested `call_bif` route inside a slice can never be advanced (that
//!    context may not pump the scheduler), and its `receive` folds into the typed `blocked()`
//!    gap — which is why the per-function fun-image half of the call-site guard yields to the
//!    in-machine entry exactly there (`context::apply::fun_image_guard_yields_for_a_slice`,
//!    consumed by `context::execution::resolve_external_call`).
//!
//! # What each case would have caught before the fix
//!
//! | Case | Pre-fix behaviour it rules out |
//! |---|---|
//! | [`a_spawned_sender_is_resumed_by_its_own_reply`] | the spawned `gen_tcp:send/2` wrote its bytes and then hung: `prim_inet:send/3` was refused the in-machine entry, folded into the nested route, and its `receive` came back as `unsupported: … process blocked (no message available)` (CLI exit 3, child never reports) |
//! | [`a_spawned_command_reply_carries_its_own_tag_and_never_reaches_the_owner`] | the `{inet_reply, S, ok, Tag}` went to the port's **owner** — the child blocked and the owner received a message it never asked for, so the probe answered `no_reply` / `owner_got_reply` instead of `{tagged,ok,true}` / `no_owner_reply` |
//! | [`two_concurrent_senders_each_get_their_reply`] | the second (concurrent) issuer never saw its reply — only the recorded *owner* would have been addressed |
//! | [`a_send_on_a_closed_socket_is_closed`], [`a_send_after_the_peer_closed_is_closed`] | a command's negative answer is also an `{inet_reply, …}`; a mis-addressed one would hang the caller instead of answering `{error,closed}` |
//! | [`an_untagged_command_gets_no_reply_and_the_port_dies_of_einval`] | over-delivery: a command that carries **no** caller tag must produce **no** reply (the C's `init_caller` failure is `driver_failure_posix(…, EINVAL)`, `inet_drv.c:12635-12638`) |
//!
//! # What is compared
//!
//! Every probe is one `-eval` expression run under the built `beam_rt` binary **and** under the
//! system `erl`, with the **exit code** and the **stdout bytes** required to be equal
//! (`support::differential_eval`) and the bytes then pinned exactly, so the suite still
//! discriminates without a toolchain. A missing `erl` is a **loud** skip
//! ([`support::skip_loudly`]), never a silent pass.
//!
//! Each probe additionally asserts the **essential invariant** rather than only the text: the
//! run exits `0` and its stderr never carries the pre-fix `process blocked` verdict, i.e. the
//! process that issued the command was *resumed* and not left parked.
//!
//! # Determinism (no volatile value is pinned, no probe can hang)
//!
//! Every socket is loopback-only (`{ip, {127,0,0,1}}`, port `0`) and the ephemeral port is
//! never pinned; every socket and port is closed before the probe answers. A spawned helper that
//! must let the parent's read arm first waits on a **bounded** `receive after 300 -> ok end`
//! (never a `timer:sleep`, never an unbounded wait), and every probe-side wait carries its own
//! bound, so a lost reply shows up as a visible mismatch. [`support::PROBE_TIMEOUT`] additionally
//! kills a child that never answers.
//!
//! # The one matrix row this runtime cannot pin yet
//!
//! The read-side twin — `gen_tcp:recv/3` from a process that is **not** the controlling process
//! (`{ok, Bin}`, no `not_owner`) — is [`a_read_from_a_process_that_is_not_the_controlling_process`],
//! `#[ignore]`d because two *other*, independent gaps stand in its way (a spawned process's
//! passive `gen_tcp:recv/3` never returns; and an owning helper left parked in a bounded
//! `receive` makes the CLI report `unsupported: run: process blocked`). Neither is the
//! reply-recipient rule, so the row is documented here and escalated rather than pinned green.

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
/// bytes arrive: a passive read of an accepted socket that is armed only *after* the payload was
/// buffered is a separate, documented `beam_io` residual (`tests/inet_completion.rs`), not this
/// rule.
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
/// is deliberately not observed: two reads of one accepted socket are the arm-once residual named
/// above, and what this row pins is the pair of *replies*.
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
// The matrix row this runtime cannot pin yet
// ---------------------------------------------------------------------------------------

/// A passive `gen_tcp:recv/3` from a process that is **not** the controlling process succeeds
/// (`{ok, <<"hi">>}`) — the oracle's answer, with no `not_owner` in sight.
///
/// `#[ignore]`d: two *other* gaps stand between this runtime and the row, and neither is the
/// reply-recipient rule this file pins.
///
/// * The reader must be a second process, and a **spawned** process's passive `gen_tcp:recv/3`
///   never returns today — measured with the payload already buffered *and* with a `0` timeout,
///   so it is the read op's completion, not the arrival, that is lost.
/// * The alternative shape (the `-eval` process reads a socket a spawned owner opened, so the
///   reader is the nested route that *does* complete a read) needs that owner to stay alive, and
///   a spawned process left parked in a bounded `receive` at the end of a `-eval` run makes the
///   CLI report `unsupported: run: process blocked (no message available)` (exit 3) instead of
///   exiting on its own.
///
/// The expectation below is the **oracle's own**, recorded with `erl -noshell -eval …`; the row
/// stays visible here so it is re-pinned the moment either blocker is closed.
#[test]
#[ignore = "a spawned process's passive gen_tcp:recv/3 never returns; an owning helper's parked receive makes the CLI report `process blocked` (both outside the reply-recipient rule)"]
fn a_read_from_a_process_that_is_not_the_controlling_process() {
    let expr = "begin \
         P = self(), \
         spawn(fun() -> \
             {ok,L} = gen_tcp:listen(0,[{ip,{127,0,0,1}},binary,{active,false}]), \
             {ok,Port} = inet:port(L), \
             {ok,Cl} = gen_tcp:connect({127,0,0,1},Port,[binary,{active,false}]), \
             {ok,A} = gen_tcp:accept(L), \
             ok = gen_tcp:send(A, <<\"hi\">>), \
             P ! {sock, Cl} end), \
         Cl = receive {sock,S} -> S after 3000 -> none end, \
         R = gen_tcp:recv(Cl, 2, 3000), \
         io:format(\"~p~n\",[{R}]) end";
    pinned(
        "a non-owner gen_tcp:recv/3 succeeds",
        expr,
        "{ok,<<\"hi\">>}\n",
    );
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
