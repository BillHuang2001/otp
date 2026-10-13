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

//! `erlang:garbage_collect/0,1,2` (contract A.7) — the observable contract.
//!
//! `src/native/erts_internal.rs` owns the BIF layer and its colocated unit tests pin the
//! argument/return shapes against this runtime's own process table. This suite pins what a
//! *program* sees, in two halves:
//!
//! 1. **The return values, compared with the live oracle.** Every expression below is given
//!    unchanged to the built `beam_rt` binary (`-noshell -eval`) and to the system `erl`
//!    (same `-eval`, terminated with `-s init stop`), and the two printed results must be
//!    identical: `/0` and `/1` on the calling process are `true`, `/1` on another **live**
//!    pid is `true`, `/1` on a pid that has terminated is `false`, and `/1` on a non-pid is
//!    `badarg`. `/2`'s options form is classified the same way — the `{async, _}` form is a
//!    served call (exit `0`) and a bad option is the ordinary `badarg` (exit `1`) — never a
//!    capability gap (exit `3`).
//! 2. **The collection really happens.** A **scheduled** process whose code calls
//!    `erlang:garbage_collect()` and then allocates really moves
//!    [`Runtime::gc_stats`]'s `collections` counter, while the *same* workload without the
//!    call collects nothing — so the counter moves because of the BIF, not because the nursery
//!    happened to fill. The BIF itself can never collect (a BIF does not own the executing
//!    machine's root set), so this is the acceptance evidence for the request latch and its
//!    two serving points.
//!
//! The fixture of half 2 is compiled **at test time** with the system `erlc` into a scratch
//! directory (never committed, like `tests/multimodule.rs`), and both halves are **loud
//! skips** when the system Erlang/OTP toolchain is absent — never a silent pass.

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery
// (the return-value probes need the child's exit code, which `support::oracle_eval` folds
// away), so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use beam_rt::Runtime;
use beam_term::{signed_value, Eterm};

// ---------------------------------------------------------------------------------------
// The bounded child runner and the two runtimes under test
// ---------------------------------------------------------------------------------------

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite. `None` when the
/// child could not be spawned, polled or collected.
fn run_bounded(program: &str, args: &[&str]) -> Option<Output> {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + support::PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().ok(),
            Ok(None) => {
                if Instant::now() >= deadline {
                    eprintln!("warning: `{program}` outlived the probe bound and was killed");
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                eprintln!("warning: cannot poll `{program}`: {error}");
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// One finished probe: its exit code and trimmed stdout/stderr.
struct Run {
    code: i32,
    out: String,
    err: String,
}

/// Runs `program args`, failing loudly when the probe could not be run at all.
fn run(program: &str, args: &[&str]) -> Run {
    let output = run_bounded(program, args)
        .unwrap_or_else(|| panic!("`{program}` {args:?} could not be run to completion"));
    Run {
        code: output.status.code().unwrap_or(-1),
        out: String::from_utf8_lossy(&output.stdout).trim().to_string(),
        err: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    }
}

/// `beam_rt -noshell -eval <expr>` through the built binary.
fn beam_rt_eval(expr: &str) -> Run {
    run(env!("CARGO_BIN_EXE_beam_rt"), &["-noshell", "-eval", expr])
}

/// The same expression under the oracle, terminated by `-s init stop` (an `erl -noshell`
/// never exits on its own).
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    let program = oracle
        .to_str()
        .unwrap_or_else(|| panic!("the oracle path is not UTF-8: {}", oracle.display()));
    run(program, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

// ---------------------------------------------------------------------------------------
// The programs both runtimes are given
// ---------------------------------------------------------------------------------------

/// The calling process's own collection: `/0` (the `erts_internal:garbage_collect(major)`
/// wrapper) and `/1`'s `self()` clause both answer `true`.
///
/// Oracle: `erl -noshell -eval 'io:format("~p~n",[{erlang:garbage_collect(),
/// erlang:garbage_collect(self())}]),halt(0).'` prints `{true,true}`.
const EXPR_OWN_PROCESS: &str = "\
io:format(\"~p~n\", [{erlang:garbage_collect(), erlang:garbage_collect(self())}]), halt(0).";

/// `/1` on **another live** local pid: the request is accepted, so the answer is `true`.
///
/// Oracle (the same expression): `true`.
const EXPR_LIVE_PID: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
io:format(\"~p~n\", [erlang:garbage_collect(P)]), \
halt(0).";

/// `/1` on a pid that has **terminated**: `false`. The `'DOWN'` monitor rendezvous makes the
/// "it really is gone" fact deterministic in both runtimes — the probe blocks on the DOWN (a
/// live process's `receive` still lets the child run to completion), so the lookup that
/// follows can never race the child's exit.
///
/// Oracle (the same expression): `{down,false}`.
const EXPR_DEAD_PID: &str = "\
P = spawn(fun() -> receive stop -> ok end end), \
R = erlang:monitor(process, P), \
P ! stop, \
D = receive {'DOWN', R, _, _, _} -> down after 200 -> timeout end, \
io:format(\"~p~n\", [{D, erlang:garbage_collect(P)}]), \
halt(0).";

/// `/1` on a term that is **not** a pid: the oracle's `badarg` (the C's argument test), caught
/// here so the **reason** — not a stack trace — is what the two runtimes are compared on.
///
/// Oracle (the same expression): `{badarg,badarg,badarg}`.
const EXPR_NON_PID: &str = "\
A = try erlang:garbage_collect(foo) catch error:E1 -> E1 end, \
B = try erlang:garbage_collect(123) catch error:E2 -> E2 end, \
C = try erlang:garbage_collect([self()]) catch error:E3 -> E3 end, \
io:format(\"~p~n\", [{A, B, C}]), \
halt(0).";

/// The four cross-checked programs and the value both runtimes must print.
const PROGRAMS: [(&str, &str, &str); 4] = [
    (
        "garbage_collect/0 and /1 on the calling process",
        EXPR_OWN_PROCESS,
        "{true,true}",
    ),
    ("garbage_collect/1 on a live pid", EXPR_LIVE_PID, "true"),
    (
        "garbage_collect/1 on a terminated pid",
        EXPR_DEAD_PID,
        "{down,false}",
    ),
    ("garbage_collect/1 on a non-pid", EXPR_NON_PID, "{badarg,badarg,badarg}"),
];

// ---------------------------------------------------------------------------------------
// The `/2` async completion message (the follow-up `{garbage_collect, ReqId, Result}`)
// ---------------------------------------------------------------------------------------

/// `/2`'s `{async, ReqId}` form on a **blocked** child: the wrapper answers `async` and the
/// child's collection owes the **caller** `{garbage_collect, ReqId, true}`.
///
/// The child parks in a `receive … after 60000` (rather than a bare `receive`) so it stays a
/// **live** blocked process: a bare `receive` inside a spawned process is a documented
/// capability gap of this runtime's `-eval` evaluator (there is no host timer to block on), and
/// the child would terminate — which is a *different* case, pinned by
/// [`EXPR_ASYNC_DEAD`]. The scheduled-process fixture covers the bare-receive shape.
///
/// The probe prints a **normalized** shape — the reference is compared with `=:=` rather than
/// printed, because a `#Ref<…>`'s textual form names the node's own reference allocator and
/// could never be compared across runtimes. A missing message prints the atom `timeout`, so a
/// regression is a clean mismatch rather than a hang.
///
/// Oracle (the same expression, terminated with `-s init stop`): `{true,true}`.
///
/// **KNOWN GAP — this case disagrees with the oracle (measured, not assumed).** This runtime
/// prints `{true,false}` where the oracle prints `{true,true}`. The divergence is the **liveness
/// `Result`** (the `{garbage_collect, ReqId, Result}` *shape* and the `ReqId` match are both
/// right; only the `true`/`false` differs), and the `false` is honest: the **target is already
/// gone** by the pass boundary that emits the completion, not a wrong answer from the completion
/// logic.
///
/// The premise in the paragraph above does **not** hold here: a spawned `-eval` child's
/// `receive … after 60000` does **not** park it for 60000 ms. The evaluator's `after` arm
/// (`src/eval/mod.rs`, `Evaluator::receive`) polls the port poller once for the remaining time
/// and then falls through to the `after` body — the `break` after that single round is
/// unconditional, and with no live port `poll_port_messages` (`src/port.rs`) returns at once
/// (there is no host timer to block on in M1). So the child runs `ok`, returns and terminates on
/// its **first** slice; `is_process_alive(P)` is already `false` at the `erlang:garbage_collect(P,
/// …)` call, where `erl` says `true` (verified directly). The completion then reports that dead
/// target's `false` — exactly what the C's `liveness_notify_sys_task_executed`
/// (`erts/emulator/beam/erl_process.c:11488-11490`) does for a dead target.
///
/// **Owner:** `src/eval/mod.rs`'s `receive` — the `after T` deadline must actually be waited out
/// (or a spawned `-eval` child otherwise kept alive) before this case can match the oracle. The
/// two modules the harness-level diagnosis points at are **downstream and correct**:
/// `src/runtime/gc.rs::deliver_gc_completions` and the native floor `src/native/erts_internal.rs`
/// (which passes `live` from the request instant and reports the target's real liveness) both
/// answer `false` here because the target really is dead, and both would answer the oracle's
/// `true` unchanged once the child survives the call. The assertion is deliberately **kept, not
/// weakened**, so the gap stays visible.
const EXPR_ASYNC_BLOCKED: &str = "\
Self = self(), \
P = spawn(fun() -> Self ! {ready, self()}, receive _ -> ok after 60000 -> ok end end), \
receive {ready, P} -> ok end, \
Ref = make_ref(), \
async = erlang:garbage_collect(P, [{async, Ref}]), \
R = receive {garbage_collect, Got, Result} -> {Got =:= Ref, Result} after 2000 -> timeout end, \
io:format(\"~p~n\", [R]), halt(0).";

/// The requester is the **target** (`self()`): the completion is delivered into the requesting
/// process's own mailbox, so it can receive it after the call returns.
///
/// Oracle: `{true,true}`.
const EXPR_ASYNC_SELF: &str = "\
Ref = make_ref(), \
async = erlang:garbage_collect(self(), [{async, Ref}]), \
R = receive {garbage_collect, Got, Result} -> {Got =:= Ref, Result} after 2000 -> timeout end, \
io:format(\"~p~n\", [R]), halt(0).";

/// A target that has **terminated** before the call: the oracle's result is `false` (the C's
/// `noproc_res`, `erl_process.c:11420-11461`) — the same `{garbage_collect, ReqId, …}` shape,
/// with the honest liveness answer. The `'DOWN'` rendezvous makes "it really is gone"
/// deterministic in both runtimes.
///
/// Oracle: `{true,false}`.
const EXPR_ASYNC_DEAD: &str = "\
P = spawn(fun() -> ok end), \
M = erlang:monitor(process, P), \
receive {'DOWN', M, _, _, _} -> ok after 2000 -> ok end, \
Ref = make_ref(), \
async = erlang:garbage_collect(P, [{async, Ref}]), \
R = receive {garbage_collect, Got, Result} -> {Got =:= Ref, Result} after 2000 -> timeout end, \
io:format(\"~p~n\", [R]), halt(0).";

/// The three async-completion programs and the value both runtimes must print — the pinned
/// shape of the follow-up message `{garbage_collect, ReqId, Result}`.
const ASYNC_PROGRAMS: [(&str, &str, &str); 3] = [
    (
        "the async completion of a blocked target",
        EXPR_ASYNC_BLOCKED,
        "{true,true}",
    ),
    (
        "the async completion of the requesting process itself",
        EXPR_ASYNC_SELF,
        "{true,true}",
    ),
    (
        "the async completion of a terminated target",
        EXPR_ASYNC_DEAD,
        "{true,false}",
    ),
];

// ---------------------------------------------------------------------------------------
// The scheduled-process fixture (half 2)
// ---------------------------------------------------------------------------------------

/// The fixture of half 2, generated by this test (never committed).
///
/// `ask/1` asks for a collection and then conses `N` times; `plain/1` conses the **same** `N`
/// times without asking. Both return `length(Acc)` — `N` — and `N` is chosen far below the
/// young heap's initial capacity, so the *only* difference between the two runs is the
/// request: `ask` must collect, `plain` must not.
///
/// The module is integer-only (no float instruction), which is what makes it collectable at
/// all (`src/context/gc.rs`'s float guard refuses a module that touches the machine's
/// float-register file).
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/garbage_collect.rs` (never committed).
-module(gc_req).
-export([ask/1, plain/1, ask_other/0, plain_other/0, child/1]).

ask(N) ->
    true = erlang:garbage_collect(),
    cons(N, []).

plain(N) ->
    cons(N, []).

%% Ask for a collection of *another*, live scheduled process: the request must be served
%% when that process next allocates, even though this one only recorded it.
ask_other() ->
    Self = self(),
    Child = spawn(gc_req, child, [Self]),
    receive {ready, Child} -> ok end,
    true = erlang:garbage_collect(Child),
    Child ! go,
    receive {done, Child, V} -> V end.

%% The control: the identical rendezvous and workload without the request.
plain_other() ->
    Self = self(),
    Child = spawn(gc_req, child, [Self]),
    receive {ready, Child} -> ok end,
    Child ! go,
    receive {done, Child, V} -> V end.

child(Parent) ->
    Parent ! {ready, self()},
    receive go -> ok end,
    V = cons(8, []),
    Parent ! {done, self(), V}.

cons(0, Acc) ->
    length(Acc);
cons(N, Acc) ->
    cons(N - 1, [N | Acc]).
"#;

/// How many cons cells the fixture builds — well under the collector's initial young heap
/// (`beam_gc::HeapPolicy::initial_young_words` is in the low hundreds), so `plain/1` never
/// fills the nursery.
const CONS_COUNT: i64 = 8;

/// A compiled fixture in its own scratch directory.
struct Fixture {
    /// The scratch directory holding `gc_req.erl`/`gc_req.beam`.
    dir: PathBuf,
    /// The compiled `gc_req.beam`.
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
    let dir = std::env::temp_dir().join(format!(
        "beam_rt_garbage_collect_{label}_{}",
        std::process::id()
    ));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        println!("SKIP garbage_collect: cannot create {}: {error}", dir.display());
        return None;
    }
    let source = dir.join("gc_req.erl");
    if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
        println!("SKIP garbage_collect: cannot write {}: {error}", source.display());
        return None;
    }
    let output = match Command::new("erlc").arg("-o").arg(&dir).arg(&source).output() {
        Ok(output) => output,
        Err(error) => {
            println!("SKIP garbage_collect: cannot run `erlc`: {error}");
            return None;
        }
    };
    if !output.status.success() {
        println!(
            "SKIP garbage_collect: `erlc` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        return None;
    }
    let beam = dir.join("gc_req.beam");
    if !beam.is_file() {
        println!(
            "SKIP garbage_collect: `erlc` produced no {}",
            beam.display()
        );
        return None;
    }
    Some(Fixture { dir, beam })
}

/// A runtime with the fixture loaded and the boot sequence run, or `None` (a **loud skip**)
/// when the system installation is not resolvable.
fn runtime_with(beam: &Path) -> Option<Runtime> {
    support::require_oracle("garbage_collect")?;
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(beam).expect("the fixture loads");
    Some(rt)
}

/// What one scheduled run of the fixture reported: its returned value and the runtime
/// collections it caused.
#[derive(Debug, Clone, Copy)]
struct ScheduledRun {
    /// The value the entry returned.
    value: Eterm,
    /// How many collections the **runtime** ran during this run (differenced over it).
    collections: u64,
}

/// Runs `module:function(args…)` as a **scheduled process**, to completion.
///
/// One slice is run per step, exactly the granularity `tests/gc_collect.rs` uses: the runner
/// pre-empts at the reduction budget and the process's own young heap is the one the collector
/// moves. The counters are differenced around the run, so a preceding test case can never
/// contaminate the reading.
fn run_entry(rt: &mut Runtime, module: &str, function: &str, args: &[Eterm]) -> ScheduledRun {
    let before = rt.gc_stats();
    let pid = rt
        .spawn_process(module, function, args.to_vec())
        .expect("the fixture entry is spawned");
    let deadline = Instant::now() + Duration::from_secs(120);
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "{module}:{function}/{arity} did not terminate within 120s",
            arity = args.len()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
    }
    let value = rt.process_result(pid).expect("the process returned a value");
    let after = rt.gc_stats();
    ScheduledRun {
        value,
        collections: after.collections - before.collections,
    }
}

/// [`run_entry`] for the fixture's `/{1}` entries.
fn run_scheduled(rt: &mut Runtime, module: &str, function: &str, n: i64) -> ScheduledRun {
    run_entry(rt, module, function, &[beam_term::make_small(n)])
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// **Half 1 — the return values match the live oracle.** Each program is given unchanged to
/// the built binary and to the system `erl`; the printed results must be identical.
#[test]
fn the_return_values_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("garbage_collect") else {
        return;
    };
    for (case, expr, expected) in PROGRAMS {
        let ours = beam_rt_eval(expr);
        assert_eq!(
            ours.code, 0,
            "{case}: `beam_rt -eval` must exit 0; stderr: {}",
            ours.err
        );
        assert_eq!(ours.out, expected, "{case}: our runtime's answer");
        let theirs = oracle_eval(&oracle, expr);
        assert_eq!(
            theirs.code, 0,
            "{case}: the oracle exited {}; stderr: {}",
            theirs.code, theirs.err
        );
        assert_eq!(
            theirs.out, expected,
            "{case}: the live oracle's answer (the recorded ground truth must be re-probed)"
        );
    }
}

/// **Half 1d — `/2`'s async completion message matches the live oracle.**
///
/// The `{async, ReqId}` form does not merely answer `async`: the target's collection owes the
/// **requester** `{garbage_collect, ReqId, Result}`, and the three probes below pin the whole
/// shape — a **blocked** target (`true`), the requesting process itself (`true`) and an
/// already-**terminated** target (`false`, the C's `noproc_res`). Both runtimes are given the
/// same expression and must print the same normalized `{Ref-equality, Result}` pair, so the
/// pinned message shapes are re-probed against the oracle rather than assumed.
#[test]
fn the_async_completion_message_matches_the_live_oracle() {
    let Some(oracle) = support::require_oracle("garbage_collect") else {
        return;
    };
    for (case, expr, expected) in ASYNC_PROGRAMS {
        let ours = beam_rt_eval(expr);
        assert_eq!(
            ours.code, 0,
            "{case}: `beam_rt -eval` must exit 0; stderr: {}",
            ours.err
        );
        // KNOWN GAP: for the "blocked target" case (`EXPR_ASYNC_BLOCKED`) `ours.out` is
        // `{true,false}` against the oracle's `{true,true}` — the sole divergence is the
        // completion message's **liveness `Result`**, because this runtime's `-eval` evaluator
        // lets the spawned child terminate immediately (its `receive … after 60000` does not
        // wait). Kept, not weakened; the cause and owner are on `EXPR_ASYNC_BLOCKED`'s doc.
        assert_eq!(ours.out, expected, "{case}: our runtime's answer (stderr: {})", ours.err);
        let theirs = oracle_eval(&oracle, expr);
        assert_eq!(
            theirs.code, 0,
            "{case}: the oracle exited {}; stderr: {}",
            theirs.code, theirs.err
        );
        assert_eq!(
            theirs.out, expected,
            "{case}: the live oracle's answer (the recorded ground truth must be re-probed)"
        );
    }
}

/// **Half 1b — an uncaught `badarg` is a genuine runtime error, not a capability gap.**
///
/// Both runtimes terminate with exit code `1` (the oracle's `exception error: bad argument`;
/// this runtime's `beam_rt: error badarg`), where an unsupported BIF exits `3`. The `error`
/// **values** are compared by [`the_return_values_match_the_live_oracle`]; this case pins the
/// classification, which is what the differential harness keys off.
#[test]
fn an_uncaught_badarg_is_a_runtime_error_in_both() {
    let Some(oracle) = support::require_oracle("garbage_collect") else {
        return;
    };
    let expr = "erlang:garbage_collect(foo).";
    let ours = beam_rt_eval(expr);
    assert_eq!(ours.code, 1, "our runtime: {} / {}", ours.out, ours.err);
    assert!(
        ours.err.contains("badarg"),
        "our runtime must report the reason: {}",
        ours.err
    );
    let theirs = oracle_eval(&oracle, expr);
    assert_eq!(
        theirs.code, 1,
        "the oracle classifies it as a runtime error too: {} / {}",
        theirs.out, theirs.err
    );
}

/// **Half 1c — the `/2` options form is served, and a bad option is a runtime error.**
///
/// `erlang:garbage_collect/2` is served from the native floor, so the classification the
/// differential harness keys off must be the oracle's: the `{async, _}` option is a genuinely
/// **served** call — exit `0`, the atom `async` — where a capability gap would exit `3`; and an
/// option outside the wrapper's two 2-tuple forms (`{async, _}`/`{type, _}`) is what
/// `get_gc_opts/2` raises its `throw(bad_option)` for, i.e. the ordinary `error:badarg` (exit
/// `1`), never a capability gap either. Both runtimes are probed and both the exit code and the
/// printed value must agree, with the oracle's own expectations re-probed rather than assumed.
#[test]
fn the_async_options_form_is_served_and_a_bad_option_is_a_runtime_error() {
    let Some(oracle) = support::require_oracle("garbage_collect") else {
        return;
    };

    // The `{async, _}` form is a served call: exit 0 and the atom `async` on both runtimes.
    let async_expr = "io:format(\"~p~n\", [erlang:garbage_collect(self(), [{async, false}])]), \
                      halt(0).";
    let ours = beam_rt_eval(async_expr);
    assert_eq!(
        ours.code, 0,
        "the async form is a served call, not a capability gap: {} / {}",
        ours.out, ours.err
    );
    assert_eq!(ours.out, "async", "our runtime's answer to the async form");
    let theirs = oracle_eval(&oracle, async_expr);
    assert_eq!(
        theirs.code, 0,
        "the oracle serves the async form too: {} / {}",
        theirs.out, theirs.err
    );
    assert_eq!(
        theirs.out, "async",
        "the live oracle's answer to the async form"
    );

    // An option outside the wrapper's two 2-tuple forms is the oracle's own `badarg`: exit 1 on
    // both runtimes, never the exit-3 capability gap.
    let bad_expr = "erlang:garbage_collect(self(), [{type, foo}]).";
    let ours = beam_rt_eval(bad_expr);
    assert_eq!(
        ours.code, 1,
        "a bad option is an ordinary runtime error, not a capability gap: {} / {}",
        ours.out, ours.err
    );
    assert!(
        ours.err.contains("badarg"),
        "our runtime must report the reason: {}",
        ours.err
    );
    let theirs = oracle_eval(&oracle, bad_expr);
    assert_eq!(
        theirs.code, 1,
        "the oracle classifies a bad option as a runtime error too: {} / {}",
        theirs.out, theirs.err
    );
}

/// **Half 2 — a requested collection really runs on a scheduled process.**
///
/// The two fixture entries do the **same** work; only `ask/1` calls
/// `erlang:garbage_collect()` first. `ask` must collect and `plain` must not, so the counter
/// moves because of the BIF rather than because the nursery filled — and both must return the
/// correct value (the collection must never change a result).
#[test]
fn a_requested_collection_runs_while_the_same_workload_without_it_does_not() {
    let Some(fixture) = build_fixture("runs") else {
        return;
    };
    let Some(mut rt) = runtime_with(&fixture.beam) else {
        return;
    };

    let control = run_scheduled(&mut rt, "gc_req", "plain", CONS_COUNT);
    assert_eq!(
        signed_value(control.value),
        CONS_COUNT,
        "plain/1 must return its own count"
    );
    assert_eq!(
        control.collections, 0,
        "the control workload is far below the young heap's capacity and must collect nothing; \
         {control:?}"
    );

    let asked = run_scheduled(&mut rt, "gc_req", "ask", CONS_COUNT);
    assert_eq!(
        signed_value(asked.value),
        CONS_COUNT,
        "ask/1 must return the same value as plain/1 — a collection never changes a result"
    );
    assert!(
        asked.collections >= 1,
        "the request must force a collection at the next heap-admission point; {asked:?}"
    );

    let stats = rt.gc_stats();
    assert_eq!(stats.errors, 0, "no collection failed; {stats:?}");
    assert!(!stats.disabled, "no valve tripped; {stats:?}");

    fixture.cleanup();
}

/// **`/1` on another live process is served when that process runs.** A scheduled parent asks
/// for a collection of a second, live scheduled child; the child's own next heap-admission
/// point serves the request, so the runtime's counter moves even though the *caller* only
/// recorded a request. The control is the identical rendezvous and workload with the request
/// removed, so the request is the only difference between the two runs.
#[test]
fn a_request_for_another_live_process_is_served_when_it_runs() {
    let Some(fixture) = build_fixture("other") else {
        return;
    };
    let Some(mut rt) = runtime_with(&fixture.beam) else {
        return;
    };

    let control = run_entry(&mut rt, "gc_req", "plain_other", &[]);
    assert_eq!(
        signed_value(control.value),
        CONS_COUNT,
        "plain_other/0 returns the child's result"
    );
    assert_eq!(
        control.collections, 0,
        "the control makes the same allocations and must collect nothing; {control:?}"
    );

    let asked = run_entry(&mut rt, "gc_req", "ask_other", &[]);
    assert_eq!(
        signed_value(asked.value),
        CONS_COUNT,
        "ask_other/0 returns the same value as the control"
    );
    assert!(
        asked.collections >= 1,
        "the request recorded for another live process must be served when it next runs; \
         {asked:?}"
    );

    fixture.cleanup();
}
