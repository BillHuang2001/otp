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
//! keying, the pid-immediate round trip, and the tid substitution's draw arithmetic); this
//! suite pins what a user observes: an ETS table
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
//! [`the_owner_death_flows_match_the_live_oracle`] and
//! [`unnamed_owner_death_flows_match_the_live_oracle`]):
//!
//! ```text
//! undefined          %% (a) a named_table's dead owner leaves ets:info(Name) undefined
//! {true,true,{gift,42}}   %% (b) {heir, Self, {gift,42}}: Tid =:= Name, From =:= Owner,
//!                         %%     Data =:= the gift (`Tid` is the table's *name atom*)
//! {no_transfer,true,true} %% (c) the arity-2 {heir, Self} inherits in silence, table survives
//! {true,true,undefined}   %% (d) an arity-3 {heir, Self, undefined} *does* transfer `undefined`
//! true                    %% (e) an *unnamed* table's dead owner leaves ets:info(Tid) undefined
//! {true,true,{gift,42},true,1,true}
//!                         %% (f) an *unnamed* table with {heir, Self, {gift,42}}: Tid is a
//!                         %%     reference, From =:= Owner, Data =:= the gift, the heir can
//!                         %%     ets:insert/2 through the received tid and reads size 1 and
//!                         %%     itself as owner
//! ```
//!
//! Flow (b) pins that an arity-3 `{heir, Self, {gift,42}}` transfers the gift with `Tid` = the
//! table's **name atom**.
//!
//! # *Unnamed* tables: reclaimed through the registry roster
//!
//! `beam_bifs::TableRegistry::handles_owned_by` is the public owner→handle roster, so the
//! runtime now acts on the **roster** (unioned with the handles it indexed at `ets:new/2` time)
//! rather than on the index alone — which is what closes the unnamed-table leak flows (e) and
//! (f) pin. The index is kept only for the one thing the roster cannot supply, a
//! `named_table`'s **registered name**.
//!
//! Flow (f)'s tid is the table's **reference** (the oracle's tid for an unnamed table); the
//! runtime materializes it through `beam_process::MonitorRef::to_eterm`, which writes the very
//! same frozen `[header(3,REF_SUBTAG), node, serial, id]` block `beam_bifs`'s `make_ref_thing`
//! does — see `runtime::ets_owner::tid_ref_thing`. The flow therefore asserts only **derived**
//! facts (`is_reference(Tid)`, `From =:= Pid`, the gift, and that the tid still *works* for
//! `ets:insert/2`/`ets:info/2`), never the raw ref, whose id space differs between the two
//! runtimes.
//!
//! # Not observed: a `{heir, …}` installed *after* creation
//!
//! This crate observes only `ets:new/2`, so a `named_table` whose **name registration** it did
//! not itself see created is the one thing it could leave behind (a *name* leak, never a table
//! leak — the table is still reclaimed through the roster, and `beam_ets::Table::owner_died`
//! honours a `setopts`-installed heir like any other). The gift data of a `setopts`-installed
//! arity-3 heir is likewise not copied into table-owned storage (`Table::setopts` is the
//! non-copying primitive), which is a `beam_bifs` handoff, not a wrong result here.
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

/// (e) An **unnamed** table's owner dies: the table is reclaimed like any other, so the tid the
/// owner handed over before dying no longer resolves (`ets:info/1` is `undefined`).
const EXPR_UNNAMED_TABLE_DIES: &str = r#"
Self = self(),
{Pid, Ref} = spawn_opt(fun() -> T = ets:new(owned_unnamed, [public]), Self ! {tid, T} end, [monitor]),
T = receive {tid, X} -> X end,
receive {'DOWN', Ref, process, _, _} -> ok end,
io:format("~p~n", [ets:info(T) =:= undefined]),
halt(0).
"#;

/// (f) An **unnamed** table with `{heir, Self, {gift,42}}`: the heir receives the table — this
/// time under the table's **reference** as the tid, not a name atom — and the received tid is
/// still a live handle (`ets:insert/2` succeeds and the table now holds one object, owned by
/// the heir). Only derived facts are compared: the raw reference's id differs between runtimes.
const EXPR_UNNAMED_HEIR_GIFT: &str = r#"
Self = self(),
{Pid, Ref} = spawn_opt(fun() -> ets:new(unnamed_gift_probe, [public, {heir, Self, {gift,42}}]) end, [monitor]),
receive {'DOWN', Ref, process, _, _} -> ok end,
Obs = receive {'ETS-TRANSFER', Tid, From, Data} ->
          Inserted = ets:insert(Tid, {k, v}),
          {is_reference(Tid), From =:= Pid, Data, Inserted,
           ets:info(Tid, size), ets:info(Tid, owner) =:= Self}
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

/// The **unnamed**-table owner-death flows (see the module documentation's (e) and (f)): the
/// table itself is reclaimed through the `beam_bifs` owner→handle roster, and a transferred one
/// carries the table's **reference** as its tid.
const UNNAMED_FLOWS: [Flow; 2] = [
    Flow {
        label: "(e) a dead owner's unnamed table is gone",
        expr: EXPR_UNNAMED_TABLE_DIES,
        expected: "true",
    },
    Flow {
        label: "(f) an unnamed table with {heir, Self, {gift,42}} transfers its reference tid",
        expr: EXPR_UNNAMED_HEIR_GIFT,
        expected: "{true,true,{gift,42},true,1,true}",
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

/// The **unnamed**-table flows print exactly what the live oracle prints for the same program:
/// the gone tid of a dead owner's unnamed table, and the heir's
/// `{'ETS-TRANSFER', Tid, FromPid, Data}` carrying the table's **reference** (a tid that still
/// works), the dead owner's pid, the gift, and the heir's ownership afterwards.
///
/// The oracle is consulted per flow, so a divergence fails on the side that disagrees.
#[test]
fn unnamed_owner_death_flows_match_the_live_oracle() {
    let Some(oracle) = support::require_oracle("ets_owner") else {
        return;
    };
    for Flow {
        label,
        expr,
        expected,
    } in UNNAMED_FLOWS
    {
        let ours = beam_rt_eval(expr);
        assert_run("beam_rt", label, &ours, expected);

        let theirs = oracle_eval(&oracle, expr);
        assert_run("erl", label, &theirs, expected);
    }
}
