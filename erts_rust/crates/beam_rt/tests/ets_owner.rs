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

//! **ETS owner-death reclamation** at the **executable** boundary.
//!
//! `src/runtime/ets_owner.rs`'s unit tests pin the owner index in isolation (recording,
//! keying, the pid-immediate round trip); this suite pins what a user observes: an ETS table
//! whose owning process dies is released — dropped, or handed to its installed heir with the
//! oracle's `{'ETS-TRANSFER', Tid, FromPid, Data}` message — through the built `beam_rt`
//! binary, the process finalizer, the `ProcessExitObserver` seam and a scheduler-pass drain,
//! compared with the live system `erl` oracle running the **same** program.
//!
//! # Why `-eval` and not a compiled fixture
//!
//! A table's owner death only means something for a process that can be *observed* dying, and
//! the cheapest such process is a `spawn_opt(…, [monitor])` from the `-eval` program itself —
//! the same shape as `tests/exit.rs` / `tests/monitor.rs`, with no fixture to compile. The
//! owner is the spawnee; the observer is the `-eval` process (the heir, or — for the
//! disappeared-table flow — a plain waiter for the `{'DOWN', …}`).
//!
//! # Oracle ground truth (system `erl`, OTP 29 / `erts-17.1`)
//!
//! The flows print exactly (re-probed — and re-asserted — by
//! [`the_owner_death_flows_match_the_live_oracle`]):
//!
//! ```text
//! undefined          %% (a) a named_table's dead owner leaves ets:info(Name) undefined
//! {true,true,{gift,42}}   %% (b) {heir, Self, {gift,42}}: Tid =:= Name, From =:= Owner,
//!                         %%     Data =:= the gift (`Tid` is the table's *name atom*)
//! {no_transfer,true,true} %% (c) the arity-2 {heir, Self} inherits in silence, table survives
//! {true,true,undefined}   %% (d) an arity-3 {heir, Self, undefined} *does* transfer `undefined`
//! ```
//!
//! Flow (b) is also the regression pin for the two defects the owner-death work shipped with:
//! the transfer tag was interned under the name `'ETS-TRANSFER'` (quotes included), so the
//! pattern could never match; and the gift `Data` arrived as `{}` because the creation-time
//! heir held a raw reference into the creating process's heap instead of a table-owned copy
//! (`beam_rt`'s `Runtime::own_heir_gift`). Either defect turns (b) into `no_transfer` /
//! `{true,true,{}}`, so the flow fails loudly if either regresses.
//!
//! # The unreachable case: an *unnamed* table (the documented capability gap)
//!
//! `beam_rt` reclaims only tables it can *prove* an owner created: `beam_bifs`'s table
//! registry exposes no public owner→handle roster and no handle iterator (`all_tids` is
//! `pub(super)`), so the runtime resolves a fresh table's handle at `ets:new/2` time through
//! the public `lookup_name` surface — which exists only for a **`named_table`**. An owner's
//! **unnamed** table is therefore not indexed and not reclaimed: it leaks, exactly as it did
//! before the owner-death work, and no fabricated tid is ever produced. The oracle instead
//! drops (or transfers) an unnamed table like any other, so the case is a **known divergence**
//! rather than a wrong result — pinned by
//! [`unnamed_tables_are_not_reclaimed_the_documented_gap`], and reported as the handoff
//! `beam_bifs::TableRegistry` needs a public owner/handle roster to close.
//!
//! The `ets:setopts/2`-installed heir path is likewise unindexed: this crate observes only
//! `ets:new/2`, so a table whose heir is installed *after* creation is not reclaimed (its
//! gift survives here only because M1 never resets a process heap). Same handoff, same owner.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain
// discovery and probe bound (the probes need the exit code, which `support::oracle_eval`
// folds away), so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite. `None` when the
/// child could not be spawned, polled or collected.
///
/// The bounded runner lives here rather than in `tests/support/mod.rs` because every probe
/// in this suite is a program that terminates on its own; the polling guard is the belt to
/// that suspenders, and keeping it local avoids editing the shared harness.
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

/// Asserts BeamError-free success and the exact stdout both runtimes must print.
///
/// `who` labels the runtime in the failure message so a divergence names its side.
fn assert_run(who: &str, label: &str, printed: &Run, expected: &str) {
    assert_eq!(
        printed.code, 0,
        "{label} [{who}]: exit {} stderr {}",
        printed.code, printed.err
    );
    assert!(
        printed.err.is_empty(),
        "{label} [{who}]: stderr {}",
        printed.err
    );
    assert_eq!(printed.out, expected, "{label} [{who}]");
}

// ---------------------------------------------------------------------------------------
// The programs both runtimes are given
// ---------------------------------------------------------------------------------------

/// (a) A `named_table`'s owner dies a normal death: the table is gone, so its name no longer
/// resolves (`ets:info/1` is `undefined`, the oracle's answer for a dead owner's table).
///
/// The owner is monitored so the `-eval` process knows the death was processed before it
/// asks — an unmonitored `spawn` would let the question race the death.
const EXPR_NAMED_TABLE_DIES: &str = r#"
{Pid, Ref} = spawn_opt(fun() -> ets:new(own_probe, [named_table, public]) end, [monitor]),
receive {'DOWN', Ref, process, _, _} -> ok end,
io:format("~p~n", [ets:info(own_probe)]),
halt(0).
"#;

/// (b) `{heir, Self, {gift,42}}`: the heir receives the table, carrying the **name atom** as
/// the tid and the dead owner's pid — and the gift data.
const EXPR_HEIR_GIFT: &str = r#"
Self = self(),
{Pid, Ref} = spawn_opt(fun() -> ets:new(gift_probe, [named_table, public, {heir, Self, {gift,42}}]) end, [monitor]),
receive {'DOWN', Ref, process, _, _} -> ok end,
Obs = receive {'ETS-TRANSFER', Tid, From, Data} -> {Tid =:= gift_probe, From =:= Pid, Data}
                     after 2000 -> no_transfer end,
io:format("~p~n", [Obs]),
halt(0).
"#;

/// (c) The arity-2 `{heir, Self}` inherits **in silence**: no message, the table survives and
/// the heir becomes its owner.
const EXPR_HEIR_SILENT: &str = r#"
Self = self(),
{Pid, Ref} = spawn_opt(fun() -> ets:new(quiet_probe, [named_table, public, {heir, Self}]) end, [monitor]),
receive {'DOWN', Ref, process, _, _} -> ok end,
Obs = receive {'ETS-TRANSFER', _, _, _} -> transfer after 500 -> no_transfer end,
io:format("~p~n", [{Obs, ets:info(quiet_probe) =/= undefined, ets:info(quiet_probe, owner) =:= Self}]),
halt(0).
"#;

/// (d) An arity-3 heir whose gift is the atom `undefined` **does** transfer: same shape as (b)
/// with `undefined` as the data — the arity decides whether a message is owed, not the value.
const EXPR_HEIR_UNDEFINED_GIFT: &str = r#"
Self = self(),
{Pid, Ref} = spawn_opt(fun() -> ets:new(u_probe, [named_table, public, {heir, Self, undefined}]) end, [monitor]),
receive {'DOWN', Ref, process, _, _} -> ok end,
Obs = receive {'ETS-TRANSFER', Tid, From, Data} -> {Tid =:= u_probe, From =:= Pid, Data}
                     after 2000 -> no_transfer end,
io:format("~p~n", [Obs]),
halt(0).
"#;

/// One `-eval` program and the value both runtimes must print for it.
struct Flow {
    /// A one-line description used in failure messages.
    label: &'static str,
    /// The program handed to both runtimes.
    expr: &'static str,
    /// The oracle's exact stdout for it.
    expected: &'static str,
}

/// The owner-death flows of the module documentation, in order.
const FLOWS: [Flow; 4] = [
    Flow {
        label: "(a) a dead owner's named_table is gone",
        expr: EXPR_NAMED_TABLE_DIES,
        expected: "undefined",
    },
    Flow {
        label: "(b) {heir, Self, {gift,42}} transfers the name, the owner pid and the gift",
        expr: EXPR_HEIR_GIFT,
        expected: "{true,true,{gift,42}}",
    },
    Flow {
        label: "(c) the arity-2 {heir, Self} inherits in silence and keeps the table",
        expr: EXPR_HEIR_SILENT,
        expected: "{no_transfer,true,true}",
    },
    Flow {
        label: "(d) an arity-3 {heir, Self, undefined} still transfers `undefined`",
        expr: EXPR_HEIR_UNDEFINED_GIFT,
        expected: "{true,true,undefined}",
    },
];

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// Every flow prints exactly what the live oracle prints for the same program: the
/// disappeared name of a dead owner's table, the heir's `{'ETS-TRANSFER', Tid, FromPid, Data}`
/// (name atom, dead owner pid, gift data), the silent arity-2 inheritance, and the
/// `undefined`-valued gift — the values quoted in the module documentation.
///
/// The oracle is consulted per flow (not once) so its answer is compared with a **live**
/// runtime, not a recorded one; a divergence therefore fails on the side that disagrees.
#[test]
fn the_owner_death_flows_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("ets_owner") else {
        return;
    };
    for Flow {
        label,
        expr,
        expected,
    } in FLOWS
    {
        let ours = beam_rt_eval(expr);
        assert_run("beam_rt", label, &ours, expected);

        let theirs = oracle_eval(&oracle, expr);
        assert_run("erl", label, &theirs, expected);
    }
}

/// The documented capability gap: an **unnamed** table's dead owner does not release it here/// (the oracle releases it — `ets:info(Tid)` is `undefined`).
///
/// The owner hands its `ets:new/2` handle to the `-eval` process before dying, so both
/// runtimes are asked the same question about the same tid. Asserting the *divergence*
/// explicitly (rather than skipping the case) keeps the gap executable: the day `beam_bifs`
/// exposes a public owner/handle roster and this crate indexes unnamed tables, the `beam_rt`
/// half below fails loudly and must be updated.
#[test]
fn unnamed_tables_are_not_reclaimed_the_documented_gap() {
    let Some(oracle) = support::require_oracle("ets_owner") else {
        return;
    };
    let expr = r#"
Self = self(),
{Pid, Ref} = spawn_opt(fun() -> T = ets:new(owned_unnamed, [public]), Self ! {tid, T} end, [monitor]),
T = receive {tid, X} -> X end,
receive {'DOWN', Ref, process, _, _} -> ok end,
io:format("~p~n", [ets:info(T) =:= undefined]),
halt(0).
"#;

    // The oracle: the table is gone with its owner.
    let theirs = oracle_eval(&oracle, expr);
    assert_run("erl", "unnamed table reclaimed", &theirs, "true");

    // This runtime: only `named_table`s are indexed, so the table survives (the gap).
    let ours = beam_rt_eval(expr);
    assert_run(
        "beam_rt",
        "unnamed table NOT reclaimed (documented gap — needs a public owner/handle roster \
         on beam_bifs::TableRegistry; see tests/ets_owner.rs)",
        &ours,
        "false",
    );
}
