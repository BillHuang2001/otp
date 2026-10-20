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

//! `erlang:send/2` (`erlang:'!'/2`) end to end: the `beam_bifs::SendHook` seam, wired.
//!
//! `beam_bifs` registers `erlang:send/2` **and** `erlang:'!'/2` as one `BifFn`
//! (`../beam_bifs/src/proc.rs`) whose whole body is `ctx.sender()`: a plain Rust BIF cannot
//! reach a mailbox, so with no hook installed the BIF raises the capability gap
//! `unsupported_bif` rather than dropping the message silently (workspace `CONTEXT.md` §3).
//! `beam_rt` installs the missing provider: [`SendBridge`] in
//! `src/context/bridges.rs`, selected by
//! [`RuntimeContext::call_registered`](beam_rt::RuntimeContext)'s `uses_send_hook` predicate,
//! which delegates to the runtime's **existing** send path
//! ([`RuntimeContext::send_message`](beam_rt::RuntimeContext)) — the very path the compiled
//! `send` instruction and the `-eval` `!` operator use.
//!
//! The `Dest ! Msg` **operator** does not reach the BIF at all: the compiler emits the `send`
//! instruction, which `beam_engine` serves through its `send_message` seam directly (that is
//! what `tests/mailbox.rs` and `tests/scheduler.rs` pin). The BIF call is what
//! `erlang:send(Pid, Msg)` compiles to (`call_ext erlang:send/2`) and what a dynamically
//! applied `erlang:'!'/2` dispatches.
//!
//! Two routes are therefore pinned here, because a **dropped** message has to behave the same
//! on both: the compiled `erlang:send/2` call and the operator's `send` instruction. The oracle
//! drops a message for a pid with no process and reports success on both
//! (`erts/emulator/beam/bif.c:2470-2480`: the lookup misses and the send reports success
//! without delivering), so `DeadPid ! Msg` evaluates to `Msg` and never raises — from compiled
//! code (`a_compiled_bang_to_a_dead_or_unknown_pid_succeeds_silently`), from `-eval`
//! (`an_eval_bang_to_a_dead_pid_agrees_with_the_live_oracle`) and leaving no residue in the
//! sender (`a_bang_to_a_dead_pid_leaves_the_sender_alive_with_an_empty_mailbox`).
//!
//! Both routes end at the runtime's send seam
//! ([`RuntimeContext::send_message`](beam_rt::RuntimeContext)), which folds the destination miss
//! into that success. A compiled `erlang:send/2` whose destination is a pid (or a reference)
//! additionally takes `beam_engine`'s own `erlang:send/2` fast path
//! (`../beam_engine/src/interpreter/dispatch.rs::resolve_call_ext_send`), which delivers through
//! the seam and folds the same miss itself — so that half is a check that the two agree, while
//! the `send` **instruction** is the route that pins the seam's fold. `SendBridge` (the
//! `beam_bifs::SendHook` provider) is reached by a `send` the fast path declines — a registered
//! name, a port, a `{Name, Node}` pair — and by a `-eval`/`apply` `erlang:send/2`, and it folds
//! nothing of its own, so the policy has one definition.//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`)
//!
//! ```none
//! 1> erlang:send(self(), hello), receive X -> X end.          %% hello
//! 2> erlang:send(self(), hello).                              %% hello (the message)
//! 3> erlang:apply(erlang, '!', [self(), hi]), receive X -> X end.  %% hi
//! 4> register(myname, self()), erlang:send(myname, hi), receive X -> X end.  %% hi
//! 5> erlang:send(42, x).        %% error:badarg   (also foo (unregistered), "ab", {a,b,c})
//! 6> P = spawn(...), P ! ok, erlang:send(DeadP, x).           %% x — success, dropped
//! 7> erlang:send(Port, Data).                                 %% Data — the port write path
//! 8> erlang:send({a, b}, x).                                  %% x — distributed send
//! 9> A = erlang:alias([reply]), erlang:send(A, hi), receive X -> X end.  %% hi — the alias owner
//! ```
//!
//! The compiled-code expectations (5 above, plus the dead pid) were checked against the
//! system `erl` with the same fixture this file compiles:
//!
//! ```none
//! $ erlc +deterministic -o <dir> sp.erl
//! $ erl -noshell -pa <dir> -eval 'io:format("~p~n",[sp:self_send()])'     %% hello
//! $ erl -noshell -pa <dir> -eval 'io:format("~p~n",[sp:bif_send(self(), hello)])'  %% hello
//! $ erl -noshell -pa <dir> -eval 'io:format("~p~n",[try sp:bif_send(42, x) catch C:E -> {C,E} end])'
//! {error,badarg}
//! $ erl -noshell -pa <dir> -eval 'D = spawn(sp, done, []), timer:sleep(50),
//!                                  io:format("~p ~p~n",[sp:bif_send(D, x), is_process_alive(D)])'
//! x false
//! $ erl -noshell -pa <dir> -eval 'D = spawn(sp, done, []), timer:sleep(50),
//!                                  io:format("~p ~p~n",[sp:bang(D, x), is_process_alive(D)])'
//! x false
//! ```
//!
//! Destination 8 is **accepted by the oracle** but is a Milestone-1 capability gap in this
//! runtime (a `{Name, Node}` send needs distribution, contract A.5); it surfaces as the honest
//! `unsupported: …` signal (exit 3), never as a fabricated value or a wrong `error::badarg`.
//! Destination 7 is served: a port destination runs the C's `erts_port_command` decomposition, so
//! `erlang:send(Port, Data)` answers the message term (writing only for the `{Pid,{command,Data}}`
//! and `{Pid,close}` shapes) — the oracle rows live in `tests/port_busy.rs`. The rest of the
//! fixture cases above cover the remaining destinations.
// The shared harness is compiled in whole, but this suite uses only its toolchain discovery
// and oracle probe (the dead-destination `!` case), so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::PathBuf;
use std::process::{Command, Output};

use beam_rt::Runtime;
use beam_term::{make_small, signed_value};

/// The committed receive fixture (`../beam_engine/tests/fixtures/recv.beam`), reused by
/// relative path — the receiver half of the cross-process case.
const RECV_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_engine/tests/fixtures/recv.beam"
);

/// The fixture source: the `erlang:send/2` **BIF** call and the `Dest ! Msg` **operator**,
/// compiled (never committed).
///
/// `bif_send/2` is the point of the fixture for the BIF half: the compiler lowers
/// `erlang:send(Dest, Msg)` to `call_ext erlang:send/2`, so running it in compiled code exercises
/// the engine's `erlang:send/2` fast path
/// (`../beam_engine/src/interpreter/dispatch.rs::resolve_call_ext_send`), which delivers through
/// [`RuntimeContext::send_message`](beam_rt::RuntimeContext) for a pid/reference destination and
/// folds the same dead-destination miss the seam does. (`erlang:'!'(Dest, Msg)`, by contrast, is
/// lowered to the `send` **instruction** and never reaches the BIF — which is why the `!`-BIF
/// case below is driven through `erlang:apply/3` instead.)///
/// `bang/2` is the point of the fixture for the **operator** half, and it is the one that
/// matters for a dropped message: `Dest ! Msg` is the `send` instruction, dispatched through
/// `ExecutionContext::send_message_with_off_heap`, a **different** seam from the BIF's
/// `SendHook` — so the dead-destination rule has to be pinned on it separately.
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/send.rs` (never committed).
-module(sp).
-export([self_send/0, bif_send/2, bang/2, bang_then_wait/2, done/0]).

%% `erlang:send/2` to this process's own mailbox, taken straight back out of the receive.
self_send() ->
    erlang:send(self(), hello),
    receive X -> X end.

%% The BIF form of a cross-process send, returning the message term (the oracle's value).
bif_send(Dest, Msg) ->
    erlang:send(Dest, Msg).

%% The **operator** form of a cross-process send. The compiler lowers `Dest ! Msg` to the
%% `send` **instruction**, so this never reaches the `erlang:send/2` BIF: it is the fixture
%% that exercises the runtime's `ExecutionContext::send_message` seam.
bang(Dest, Msg) ->
    Dest ! Msg.

%% The same `send` instruction, then a blocking `receive`: the process stays in the table
%% (parked) instead of terminating, so a test can ask whether a **dropped** message left
%% anything behind in the sender's own mailbox (it must not — the message is not
%% re-delivered to the sender).
bang_then_wait(Dest, Msg) ->
    _ = (Dest ! Msg),
    receive X -> X end.

%% A process that terminates immediately — the "dead pid" the oracle drops a message for.
done() ->
    ok.
"#;

/// A compiled fixture in its own scratch directory.
struct Fixture {
    /// The scratch directory holding `sp.erl`/`sp.beam`.
    dir: PathBuf,
    /// The compiled `sp.beam`.
    beam: PathBuf,
}

impl Fixture {
    /// Removes the scratch directory (`erlc` output is not source).
    fn cleanup(self) {
        match std::fs::remove_dir_all(&self.dir) {
            Ok(()) => {}
            // The directory is ours and disposable; a failure is reported, not hidden.
            Err(error) => println!("note: could not remove {}: {error}", self.dir.display()),
        }
    }
}

/// Compiles the fixture, or `None` when the system toolchain is unavailable (the caller then
/// returns early — **loudly**).
fn build_fixture(label: &str) -> Option<Fixture> {
    let dir = std::env::temp_dir().join(format!("beam_rt_send_{label}_{}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        println!("SKIP send: cannot create {}: {error}", dir.display());
        return None;
    }
    let source = dir.join("sp.erl");
    if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
        println!("SKIP send: cannot write {}: {error}", source.display());
        return None;
    }
    let output = match Command::new("erlc")
        .arg("+deterministic")
        .arg("-o")
        .arg(&dir)
        .arg(&source)
        .output()
    {
        Ok(output) => output,
        Err(error) => {
            println!("SKIP send: cannot run `erlc`: {error}");
            return None;
        }
    };
    if !output.status.success() {
        println!(
            "SKIP send: `erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let beam = dir.join("sp.beam");
    if !beam.is_file() {
        println!("SKIP send: `erlc` produced no {}", beam.display());
        return None;
    }
    Some(Fixture { dir, beam })
}

/// A runtime with `fixture` loaded explicitly (so `spawn_process`/`run_function` resolve the
/// module from the code server's cache, exactly as a `-pa` directory would).
fn runtime_with(fixture: &str) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.load_module(fixture).expect("the committed fixture loads");
    rt
}

/// Runs the built `beam_rt` binary with `args`.
fn beam_rt(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_beam_rt"))
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("spawning beam_rt {args:?} failed: {error}"))
}

/// Runs `expr` through the executable's `-eval` and returns `(stdout, stderr)`.
fn eval(expr: &str) -> (String, String) {
    let output = beam_rt(&["-noshell", "-eval", expr]);
    assert!(
        output.status.success(),
        "`{expr}` exited with {:?}\n--- stderr ---\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// `erlang:send(self(), hello), receive X -> X end` ⇒ `hello`.
///
/// The BIF delivers through the [`SendHook`](beam_bifs::SendHook) into the sender's own
/// mailbox, and the following `receive` scans it back out — the whole point of the wiring.
#[test]
fn eval_send_to_self_delivers_and_the_receive_takes_it() {
    let (stdout, stderr) = eval(
        "io:format(\"~p~n\",[begin erlang:send(self(), hello), receive X -> X end end]).",
    );
    assert_eq!(stdout, "hello\n");
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

/// The BIF's **return value** is the message term itself (oracle: `erlang:send(self(), hello)`
/// ⇒ `hello`), not `ok` and not the destination.
#[test]
fn eval_send_returns_the_message_term() {
    let (stdout, _) = eval("io:format(\"~p~n\",[erlang:send(self(), hello)]).");
    assert_eq!(stdout, "hello\n");
}

/// `erlang:'!'/2` is the same `BifFn` under a second name and must behave identically.
///
/// `erlang:'!'(D, M)` written literally is compiled to the `send` **instruction**, so the BIF
/// itself is reached through `erlang:apply/3` — oracle: `{hi,hi}` (the return value *and* the
/// mailbox).
#[test]
fn eval_bang_bif_delivers_and_returns_the_message() {
    let (stdout, _) = eval(
        "io:format(\"~p~n\",[begin R = erlang:apply(erlang, '!', [self(), hi]), \
         M = receive X -> X end, {R, M} end]).",
    );
    assert_eq!(stdout, "{hi,hi}\n");
}

/// A **registered name** destination resolves through the process table's registry and the
/// message is delivered to the registered process (oracle: `hi`).
#[test]
fn eval_send_to_a_registered_name_delivers() {
    let (stdout, _) = eval(
        "begin register(myname, self()), erlang:send(myname, hi), \
         io:format(\"~p~n\",[receive X -> X end]) end.",
    );
    assert_eq!(stdout, "hi\n");
}

/// A destination that is neither a pid nor a **registered** name is the oracle's
/// `error:badarg` — catchably, so a `try` sees it (oracle: `{error,badarg}` for both `42` and
/// the unregistered atom `foo`).
#[test]
fn eval_send_to_a_non_destination_or_unregistered_name_is_badarg() {
    let (integer, _) = eval(
        "io:format(\"~p~n\",[try erlang:send(42, x) catch C:E -> {C, E} end]).",
    );
    assert_eq!(integer, "{error,badarg}\n");

    let (atom, _) = eval(
        "io:format(\"~p~n\",[try erlang:send(no_such_name, x) catch C:E -> {C, E} end]).",
    );
    assert_eq!(atom, "{error,badarg}\n");
}

/// A **reference alias** destination: `erlang:send(Alias, Msg)` is served by the same
/// [`SendBridge`] seam, but the destination is neither a pid nor a name — it resolves through
/// the runtime's alias lookup (`src/context/alias.rs`, the C's `send_altact_message` arm,
/// `bif.c:2478-2492`) to the alias **owner**, and a `reply` (reply-once) alias is consumed by
/// the message that went through it.
///
/// Oracle: `{hi,hi,false}` — the BIF returns the message, the owner takes `hi`, and the
/// `reply_demonitor` alias is gone. (`tests/alias.rs` pins the same case from compiled code.)
#[test]
fn eval_send_to_a_reference_alias_reaches_its_owner_and_consumes_it() {
    let (stdout, stderr) = eval(
        "io:format(\"~p~n\",[begin A = erlang:alias([reply]), R = erlang:send(A, hi), \
         M = receive X -> X end, {R, M, erlang:unalias(A)} end]).",
    );
    assert_eq!(stdout, "{hi,hi,false}\n");
    assert!(stderr.is_empty(), "stderr: {stderr}");
}

/// **Compiled code**: `erlang:send/2` inside a `.beam` really goes through the send seam.
///
/// `sp:self_send/0` compiles to `call_ext erlang:send/2` (verified with the system
/// `beam_disasm`), so the runtime must dispatch it through `call_registered`'s send arm; the
/// oracle returns `hello` for it.
#[test]
fn compiled_send_bif_delivers_to_the_process_s_own_mailbox() {
    let Some(fixture) = build_fixture("compiled") else {
        return;
    };
    let beam = fixture.beam.to_string_lossy().into_owned();
    let mut rt = runtime_with(&beam);

    let result = rt
        .run_function(&beam, "self_send", 0, &[])
        .expect("sp:self_send/0");

    assert_eq!(rt.atom_name_of(result), Some("hello"), "oracle: hello");
    fixture.cleanup();
}

/// **Cross-process, through the scheduler**: a spawned process delivers to a *blocked*
/// receiver with `erlang:send/2`, and the receiver resumes and takes the message.
///
/// Oracle: `R = spawn(recv, wait, []), spawn(sp, bif_send, [R, 42])` ⇒ the receiver returns
/// `42` and the sender returns `42` (the message — the BIF's value). One slice first, so the
/// receiver is provably `Waiting` on an empty mailbox *before* the sender runs: the wake is
/// the send, not a message that was already queued.
#[test]
fn a_compiled_bif_send_wakes_a_blocked_receiver() {
    let Some(fixture) = build_fixture("wake") else {
        return;
    };
    let beam = fixture.beam.to_string_lossy().into_owned();
    let mut rt = runtime_with(RECV_BEAM);
    rt.load_module(&beam).expect("the fixture loads");

    let receiver = rt
        .spawn_process("recv", "wait", vec![])
        .expect("spawn recv:wait/0");
    let sender = rt
        .spawn_process("sp", "bif_send", vec![receiver.to_eterm(), make_small(42)])
        .expect("spawn sp:bif_send/2");

    assert_eq!(rt.run_one_slice().expect("one slice"), 1, "the receiver runs");
    let blocked = rt
        .processes()
        .get(receiver)
        .expect("a blocked process stays in the table");
    assert!(blocked.mailbox().is_empty(), "nothing has been sent yet");

    rt.run_scheduler().expect("the pass drains the queue");

    assert_eq!(
        signed_value(rt.process_result(receiver).expect("the receiver returned")),
        42,
        "the receiver was resumed by the BIF send"
    );
    assert_eq!(
        signed_value(rt.process_result(sender).expect("the sender returned")),
        42,
        "the BIF returns the message term"
    );
    assert!(rt.process_exit(sender).is_none(), "a normal return is not an error");
    fixture.cleanup();
}

/// A send to a pid **no process exists for** succeeds silently, delivering nothing.
///
/// The C looks the pid up and reports success when the lookup finds nothing
/// (`bif.c:2478-2480`), which is why the oracle answers `x` for a dead pid. The fixture sends
/// to a pid whose process has provably terminated (it is gone from the table) and to a pid
/// that was never allocated at all.
#[test]
fn a_compiled_bif_send_to_a_dead_or_unknown_pid_succeeds_silently() {
    let Some(fixture) = build_fixture("dead") else {
        return;
    };
    let beam = fixture.beam.to_string_lossy().into_owned();
    let mut rt = runtime_with(&beam);

    // A process that runs to completion: once it is gone from the table its pid is dead.
    let doomed = rt
        .spawn_process("sp", "done", vec![])
        .expect("spawn sp:done/0");
    rt.run_scheduler().expect("the pass drains the queue");
    assert!(
        rt.processes().get(doomed).is_none(),
        "the process terminated, so its pid is dead"
    );

    for (label, target) in [
        ("a terminated pid", doomed.to_eterm()),
        (
            "a never-allocated pid",
            beam_process::ProcessId::new(999_999, 0).to_eterm(),
        ),
    ] {
        let sender = rt
            .spawn_process("sp", "bif_send", vec![target, make_small(7)])
            .expect("spawn sp:bif_send/2");
        rt.run_scheduler().expect("the pass drains the queue");

        assert_eq!(
            signed_value(
                rt.process_result(sender)
                    .unwrap_or_else(|| panic!("the sender returned for {label}"))
            ),
            7,
            "sending to {label} returns the message, as the oracle does"
        );
        assert!(
            rt.process_exit(sender).is_none(),
            "sending to {label} must not raise"
        );
    }
    fixture.cleanup();
}

/// The `Dest ! Msg` **operator** (the `send` instruction) to a pid **no process exists for**
/// also succeeds silently, evaluating to the message.
///
/// This is a *different seam* from the BIF case above: `!` compiles to the `send`
/// instruction, which `ExecutionContext::send_message_with_off_heap` serves, so the
/// destination-miss rule has to hold there too. The C looks the pid up and reports success
/// when the lookup finds nothing (`bif.c:2470-2480`) — `erts_send_message` returns a failure
/// only for a *bad argument*, and a pid with no process is not one — which is why the oracle
/// answers `x` for a dead pid and never raises.
///
/// Oracle (system `erl`, the same fixture):
/// `D = spawn(sp, done, []), timer:sleep(50), sp:bang(D, x)` ⇒ `x`.
#[test]
fn a_compiled_bang_to_a_dead_or_unknown_pid_succeeds_silently() {
    let Some(fixture) = build_fixture("bang") else {
        return;
    };
    let beam = fixture.beam.to_string_lossy().into_owned();
    let mut rt = runtime_with(&beam);

    // A process that runs to completion: once it is gone from the table its pid is dead.
    let doomed = rt
        .spawn_process("sp", "done", vec![])
        .expect("spawn sp:done/0");
    rt.run_scheduler().expect("the pass drains the queue");
    assert!(
        rt.processes().get(doomed).is_none(),
        "the process terminated, so its pid is dead"
    );

    for (label, target) in [
        ("a terminated pid", doomed.to_eterm()),
        (
            "a never-allocated pid",
            beam_process::ProcessId::new(999_999, 0).to_eterm(),
        ),
    ] {
        let sender = rt
            .spawn_process("sp", "bang", vec![target, make_small(7)])
            .expect("spawn sp:bang/2");
        rt.run_scheduler().expect("the pass drains the queue");

        let exit_reason = rt
            .process_exit(sender)
            .map(|reason| rt.atom_name_of(reason).unwrap_or("<unnamed atom>").to_string());

        assert_eq!(
            rt.process_result(sender).map(signed_value),
            Some(7),
            "`{label} ! 7` evaluates to the message, as the oracle does \
             (the sender exited with {exit_reason:?})"
        );
        assert!(
            rt.process_exit(sender).is_none(),
            "`{label} ! 7` must not raise (beam_rt: term not found is not the C's answer)"
        );
    }
    fixture.cleanup();
}

/// A message dropped for a dead pid leaves **no residue** in the sender: the sender is still
/// alive afterwards and its own mailbox is empty.
///
/// The sender parks in a `receive` after the send, so it is still in the table when the pass
/// drains — the state a returning sender cannot show (a normal return terminates the process
/// and removes it, which is what the case above relies on). An implementation that
/// *bounced* the undeliverable message back, or that resolved the miss by delivering it to
/// the caller, would park here with a non-empty mailbox and fail.
#[test]
fn a_bang_to_a_dead_pid_leaves_the_sender_alive_with_an_empty_mailbox() {
    let Some(fixture) = build_fixture("bang_alive") else {
        return;
    };
    let beam = fixture.beam.to_string_lossy().into_owned();
    let mut rt = runtime_with(&beam);

    let doomed = rt
        .spawn_process("sp", "done", vec![])
        .expect("spawn sp:done/0");
    rt.run_scheduler().expect("the pass drains the queue");
    assert!(
        rt.processes().get(doomed).is_none(),
        "the process terminated, so its pid is dead"
    );

    for (label, target) in [
        ("a terminated pid", doomed.to_eterm()),
        (
            "a never-allocated pid",
            beam_process::ProcessId::new(999_999, 0).to_eterm(),
        ),
    ] {
        let sender = rt
            .spawn_process("sp", "bang_then_wait", vec![target, make_small(7)])
            .expect("spawn sp:bang_then_wait/2");
        rt.run_scheduler().expect("the pass drains the queue");

        let exit_reason = rt
            .process_exit(sender)
            .map(|reason| rt.atom_name_of(reason).unwrap_or("<unnamed atom>").to_string());

        let parked = rt.processes().get(sender).unwrap_or_else(|| {
            panic!(
                "sending to {label} must not kill the sender: it is parked in `receive` \
                 (the sender exited with {exit_reason:?})"
            )
        });
        assert!(
            parked.mailbox().is_empty(),
            "a message dropped for {label} must not leave residue in the sender's mailbox"
        );
        assert!(
            rt.process_result(sender).is_none(),
            "the sender is parked, so it has not returned a value"
        );
        assert!(
            rt.process_exit(sender).is_none(),
            "sending to {label} must not raise"
        );
    }
    fixture.cleanup();
}

/// The **`-eval`** route's `!` operator drops a dead destination silently too, and the whole
/// probe agrees with the live oracle byte for byte.
///
/// The `-eval` route reaches the same `send_message_with_off_heap` seam through the
/// evaluator's `!` operator, so it is a third front door onto the rule. The `catch` wrapper
/// is what makes a divergence visible as `R={'EXIT',…}` / `R=<reason>` instead of a killed
/// probe, so a runtime that raises for a dead pid fails on the printed line rather than on a
/// missing one.
///
/// The spawned process is given a real chance to die (`timer:sleep(50)`, which both runtimes
/// honour) before it is used as a destination.
#[test]
fn an_eval_bang_to_a_dead_pid_agrees_with_the_live_oracle() {
    let Some(oracle) = support::require_oracle("send") else {
        return;
    };
    let program = "P = spawn(fun() -> ok end), timer:sleep(50), \
                   R = (catch (P ! hi)), io:format(\"R=~p~n\",[R]).";

    let (stdout, stderr) = eval(program);
    assert_eq!(stdout, "R=hi\n", "`DeadPid ! hi` evaluates to the message");
    assert!(stderr.is_empty(), "stderr: {stderr}");

    let theirs = support::oracle_eval(&oracle, None, program)
        .unwrap_or_else(|| panic!("the live oracle could not answer `{program}`"));
    assert_eq!(
        theirs,
        stdout.trim(),
        "the two runtimes must agree on a `!` to a dead pid"
    );
}

/// A destination the oracle rejects with `badarg` raises `badarg` from compiled code too.
///
/// Oracle: `try sp:bif_send(42, x) catch C:E -> {C,E} end` ⇒ `{error,badarg}`.
#[test]
fn a_compiled_bif_send_to_a_non_destination_raises_badarg() {
    let Some(fixture) = build_fixture("badarg") else {
        return;
    };
    let beam = fixture.beam.to_string_lossy().into_owned();
    let mut rt = runtime_with(&beam);

    let sender = rt
        .spawn_process("sp", "bif_send", vec![make_small(42), make_small(1)])
        .expect("spawn sp:bif_send/2");
    rt.run_scheduler().expect("the pass drains the queue");

    let reason = rt
        .process_exit(sender)
        .expect("`erlang:send(42, x)` raises");
    assert_eq!(rt.atom_name_of(reason), Some("badarg"), "oracle: error:badarg");
    assert!(
        rt.process_result(sender).is_none(),
        "a raising process returns no value"
    );
    fixture.cleanup();
}

/// The `send` **instruction** (the `Dest ! Msg` operator) is unaffected: it keeps its own
/// `ExecutionContext::send_message` path, so wiring the BIF cannot have changed it.
#[test]
fn the_send_instruction_still_works() {
    let (stdout, _) = eval(
        "io:format(\"~p~n\",[begin self() ! hello, receive X -> X end end]).",
    );
    assert_eq!(stdout, "hello\n");
}
