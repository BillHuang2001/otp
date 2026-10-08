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

//! `erlang:group_leader/2` and `erts_internal:group_leader/2,3` at the **executable**
//! boundary, cross-checked against the live system `erl` oracle (contract A.9).
//!
//! `src/native/group_leader.rs`'s unit tests pin the BIF layer against the runtime's own
//! group-leader store; this suite pins the path a user sees: the built `beam_rt` binary, the
//! runtime-native floor and the `-eval`/`spawn`/`receive` machinery, compared with the oracle
//! running the **same** program.
//!
//! # Why the program restores the group leader before printing
//!
//! A process that sets its own group leader and then calls `io:format/2` sends its output to
//! the *new* leader — in the oracle, output that goes nowhere. The program therefore captures
//! the original leader first and puts it back before it prints, and every reported element is
//! a **comparison** (`… =:= self()`, an atom, or a `{Class, Reason}` pair) rather than a bare
//! pid, so the two runtimes produce one identical alphabet.
//!
//! # Oracle ground truth (system `erl`, OTP 29.1.1 / `erts-17.1`)
//!
//! The program below prints exactly (re-probed and re-asserted by
//! [`the_group_leader_matrix_matches_the_live_oracle`]):
//!
//! ```text
//! [false,true,true,true,true,true,true,
//!  {error,badarg},{error,badarg},{error,badarg},{error,undef},
//!  true,badarg,badarg,badarg,ok,ok,{error,badarg},{error,badarg},
//!  true,true]
//! ```
//!
//! which is, in order: the target really is dead; `erlang:group_leader(self(), self())` ⇒
//! `true`; `group_leader/0` and `process_info/2` see it; a **dead** leader is accepted and
//! seen; `erlang:group_leader/2` **raises** `badarg` for a non-pid leader, a non-pid target
//! and a dead target; `erlang:group_leader/3` does not exist (`undef`);
//! `erts_internal:group_leader/2` ⇒ `true`, and **returns** the atom `badarg` for the bad
//! leader / non-pid target / dead target; `/3` ⇒ `ok` — including for a dead target — and
//! raises `badarg` for a bad leader or a non-reference; finally a spawned child inherits the
//! leader its parent had.
//!
//! # Documented divergences (honoured, never hidden)
//!
//! * There is no `init`/`io` process, so an **unset** leader is this runtime's own root pid
//!   (`<0.0.0>`) where the oracle answers its `io` server (`<0.70.0>`). Every assertion here
//!   is therefore relative to `self()`, never to a literal pid.
//! * A wire **external** pid is `badarg` (the group-leader store is a local pid map), where
//!   the C accepts one as the leader or answers `true` for a remote signal. The divergence is
//!   a typed rejection, never a fabricated `true`.
//!
//! A missing `erl` is a **loud** skip, never a silent pass (`tests/support/mod.rs`).

// The shared harness is compiled in whole, but this suite uses only its toolchain discovery
// and probe bound, so the unused remainder is expected here.
#[allow(dead_code)]
mod support;

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------
// The bounded child runner
// ---------------------------------------------------------------------------------------

/// Runs `program args` with `/dev/null` as stdin, polling so a probe that blocks is killed
/// and reaped at [`support::PROBE_TIMEOUT`] instead of hanging the suite. `None` when the
/// child could not be spawned, polled or collected.
///
/// Kept local (as in `tests/monitor.rs`/`tests/exit.rs`) rather than added to the shared
/// harness: every probe here terminates on its own, and the polling guard is the belt to
/// that suspenders.
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
#[derive(Debug)]
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

/// The same expression under the oracle — `-eval` already ends in `halt(0).`, and
/// `-s init stop` is the backstop for a program that raises first.
fn oracle_eval(oracle: &Path, expr: &str) -> Run {
    let program = oracle
        .to_str()
        .unwrap_or_else(|| panic!("the oracle path is not UTF-8: {}", oracle.display()));
    run(program, &["-noshell", "-eval", expr, "-s", "init", "stop"])
}

/// The resolved oracle, or a loud skip.
fn require_oracle() -> Option<PathBuf> {
    support::require_oracle("group_leader")
}

// ---------------------------------------------------------------------------------------
// The program both runtimes are given
// ---------------------------------------------------------------------------------------

/// The whole `true`/`badarg` matrix, the `/3` validation, the `process_info/2` consistency
/// check and the child inheritance rule, in one program (see the module docs).
///
/// `Dead` is derived, never a literal pid: a `spawn`ed process is monitored so the program
/// waits for its `{'DOWN', …}` (which proves it is gone) before using it. That keeps the
/// premise true on a busy oracle node.
const PROGRAM: &str = "\
Orig = erlang:group_leader(), \
Dead = spawn(fun() -> ok end), \
MRef = monitor(process, Dead), \
receive {'DOWN', MRef, process, Dead, _} -> ok end, \
V1 = is_process_alive(Dead), \
V2 = erlang:group_leader(self(), self()), \
V3 = erlang:group_leader() =:= self(), \
V4 = element(2, process_info(self(), group_leader)) =:= self(), \
V5 = erlang:group_leader(Dead, self()), \
V6 = erlang:group_leader() =:= Dead, \
V7 = element(2, process_info(self(), group_leader)) =:= Dead, \
V8 = (try erlang:group_leader(notapid, self()) catch C1:E1 -> {C1, E1} end), \
V9 = (try erlang:group_leader(self(), notapid) catch C2:E2 -> {C2, E2} end), \
V10 = (try erlang:group_leader(self(), Dead) catch C3:E3 -> {C3, E3} end), \
V11 = (try erlang:group_leader(self(), self(), make_ref()) catch C4:E4 -> {C4, E4} end), \
V12 = erts_internal:group_leader(self(), self()), \
V13 = erts_internal:group_leader(notapid, self()), \
V14 = erts_internal:group_leader(self(), notapid), \
V15 = erts_internal:group_leader(self(), Dead), \
V16 = erts_internal:group_leader(self(), self(), make_ref()), \
V17 = erts_internal:group_leader(self(), Dead, make_ref()), \
V18 = (try erts_internal:group_leader(notapid, self(), make_ref()) catch C5:E5 -> {C5, E5} end), \
V19 = (try erts_internal:group_leader(self(), self(), notaref) catch C6:E6 -> {C6, E6} end), \
V20 = erlang:group_leader(self(), self()), \
Parent = self(), \
_Child = spawn(fun() -> Parent ! {child, erlang:group_leader() =:= Parent} end), \
V21 = receive {child, X} -> X end, \
Vs = [V1, V2, V3, V4, V5, V6, V7, V8, V9, V10, V11, V12, V13, V14, V15, V16, V17, V18, V19, \
      V20, V21], \
erlang:group_leader(Orig, self()), \
io:format(\"~w~n\", [Vs]), \
halt(0).";

/// The oracle's answer to [`PROGRAM`] — the 21 checks named in the module docs, in order.
const EXPECTED: &str = "[false,true,true,true,true,true,true,{error,badarg},{error,badarg},\
{error,badarg},{error,undef},true,badarg,badarg,badarg,ok,ok,{error,badarg},{error,badarg},\
true,true]";

// ---------------------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------------------

/// This runtime answers the whole group-leader matrix **exactly** as the live oracle does.
///
/// The expected string is asserted on both sides, and the two are compared with each other,
/// so neither a stale literal nor a changed oracle can pass unnoticed.
#[test]
fn the_group_leader_matrix_matches_the_live_oracle() {
    let Some(oracle) = require_oracle() else {
        return;
    };
    let ours = beam_rt_eval(PROGRAM);
    assert_eq!(
        ours.code, 0,
        "beam_rt exited {} (`halt(0)` should make it 0)\nstderr: {}",
        ours.code, ours.err
    );
    assert_eq!(ours.out, EXPECTED, "beam_rt's group-leader matrix");

    let theirs = oracle_eval(&oracle, PROGRAM);
    assert_eq!(
        theirs.code, 0,
        "the oracle exited {}\nstderr: {}",
        theirs.code, theirs.err
    );
    assert_eq!(theirs.out, EXPECTED, "the oracle's group-leader matrix");
    assert_eq!(ours.out, theirs.out, "the two runtimes must agree");
}

/// A bad group leader is the oracle's `error:badarg`: `beam_rt` reports it on stderr and exits
/// 1 (the CLI's runtime-error code), and the oracle's `try … catch` can catch the same class
/// and reason.
#[test]
fn a_bad_group_leader_is_the_oracles_badarg_raise() {
    let Some(oracle) = require_oracle() else {
        return;
    };
    let ours = beam_rt_eval("erlang:group_leader(notapid, self())");
    assert_eq!(ours.code, 1, "a BIF badarg is a runtime error: {ours:?}");
    assert!(
        ours.err.contains("badarg"),
        "the CLI names the reason: {}",
        ours.err
    );

    // The oracle's half: catching `error:badarg` is what makes the probe terminate cleanly,
    // so a *successful* run proves the class and the reason are exactly `error`/`badarg`.
    let theirs = oracle_eval(
        &oracle,
        "try erlang:group_leader(notapid, self()) of _ -> halt(9) \
         catch error:badarg -> halt(0) end.",
    );
    assert_eq!(
        theirs.code, 0,
        "the oracle raises error:badarg: {}",
        theirs.err
    );
}

/// `erts_internal:group_leader/2` **returns** the atom `badarg` where `erlang:group_leader/2`
/// raises — the distinction `erlang.erl`'s wrapper is written against. Both runtimes agree.
#[test]
fn the_internal_form_returns_rather_than_raises() {
    let Some(oracle) = require_oracle() else {
        return;
    };
    // `badarg` as a *value*: the program exits 0 after printing it, and `halt(9)` would flag a
    // raise instead.
    let program = "case erts_internal:group_leader(notapid, self()) of \
                     badarg -> io:format(\"badarg~n\"); \
                     _ -> io:format(\"other~n\") \
                   end, \
                   halt(0).";
    let ours = beam_rt_eval(program);
    assert_eq!(ours.code, 0, "no raise: {}", ours.err);
    assert_eq!(ours.out, "badarg");

    let theirs = oracle_eval(&oracle, program);
    assert_eq!(theirs.code, 0, "no raise: {}", theirs.err);
    assert_eq!(theirs.out, ours.out, "the two runtimes must agree");
}
