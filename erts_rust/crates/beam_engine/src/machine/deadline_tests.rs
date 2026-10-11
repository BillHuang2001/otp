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

//! The parked-`receive` **deadline** seam driven to completion, on a real compiled
//! module and against the system oracle.
//!
//! [`Machine::wait_deadline`] (in `../location.rs`) exposes the finite `after T`
//! deadline of the `receive` a machine is parked at, plus the `timeout` continuation to
//! re-enter when the deadline fires. The unit tests beside that implementation pin the
//! *read* of the deadline; the crate integration suite (`tests/receive.rs`) pins that a
//! non-zero `after` parks and reports [`RunOutcome::Wait`].
//!
//! What is pinned **here** is the whole engine-side shape a host relies on to make an
//! `after T` actually **fire** — including from the nested / blocking run path:
//!
//! 1. A **nested** run entered through the non-blocking [`Machine::run_export`] folds
//!    the empty-mailbox `receive` into the typed [`crate::error::blocked`] error — the
//!    shape a host's nested `run_export_blocking` sees. That fold must leave the
//!    machine **parked**, so [`Machine::wait_deadline`] still reports the deadline
//!    afterwards rather than the deadline being lost with the fault.
//! 2. Re-entering at the deadline's `resume` (the `timeout` opcode) through
//!    [`Machine::resume_outcome`] must run the **`after` clause** and return its value
//!    — never `blocked()` again.
//! 3. A park that happened inside a **cross-module callee** (module context != 0) has
//!    its `(func, pc)` in the *callee's* instruction stream, so the deadline must be
//!    recovered from the callee — reading the entry module the caller passed would
//!    silently lose the deadline.
//!
//! The first two are exercised on a **real** module compiled by the system `erlc`
//! (`receive foo -> foo after 5 -> timed_out end`), and the resulting value is
//! cross-checked against the system `erl` (the compatibility oracle). Those tests
//! **skip loudly** when `erlc`/`erl` are not on `PATH`.
//!
//! The engine arms **no timer**: `wait_deadline` is how the host recovers the deadline
//! to arm its own. A machine whose deadline is not resumed simply stays parked — never
//! a wrong value.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use beam_common::BeamError;
use beam_loader::{load_file, HeapNeed, Instr, Label, Module, Operand};
use beam_term::{make_atom, Atom, Eterm};

use super::test_support::{seam, EmptyMailbox, Fixture};
use super::{Machine, RunOutcome, WaitDeadline};
use crate::context::MinimalContext;

/// `receive foo -> foo after 5 -> timed_out end` — the `after T` shape the compiler
/// lowers to `loop_rec` … `wait_timeout` … `timeout`.
const AFTER_SRC: &str = "\
-module(afterrecv).
-export([recv/0]).
recv() ->
    receive
        foo -> foo
    after 5 ->
        timed_out
    end.
";

/// Compile [`AFTER_SRC`] into a per-`tag` fresh temp dir and return that dir; `None`
/// when no runnable `erlc` is on `PATH` at all (the caller skips loudly).
///
/// The `tag` names the scratch directory so tests running in parallel on the same host
/// never share a path (and thus never race on a partially-written `afterrecv.beam`).
///
/// `None` means `erlc` could not be **spawned**. An `erlc` that *ran* and exited
/// non-zero is a **harness** failure (the source is valid Erlang), so it panics with the
/// captured stderr rather than masquerading as an absent toolchain.
fn compile_after(tag: &str) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_engine_machine_deadline_oracle_{tag}"));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    std::fs::write(dir.join("afterrecv.erl"), AFTER_SRC)
        .unwrap_or_else(|error| panic!("write afterrecv.erl: {error}"));
    let output = Command::new("erlc")
        .arg("+deterministic")
        .arg("afterrecv.erl")
        .current_dir(&dir)
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erlc` exited {} compiling afterrecv.erl\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(dir)
}

/// Run `afterrecv:recv()` under the system `erl` with `dir` **appended** to the code
/// path and return its stdout; `None` when no runnable `erl` is on `PATH`.
///
/// `-pz`, never `-pa`: appending keeps the real stdlib ahead of any fixture module. A
/// spawned `erl` that exits non-zero is a harness failure and panics with the stderr.
fn oracle_recv(dir: &Path) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-pz"])
        .arg(dir)
        .args(["-eval", "io:format(\"~p~n\",[afterrecv:recv()]),halt(0)."])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for afterrecv:recv()\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Load `afterrecv.beam` from `dir` and return the module plus its `recv/0` export atom
/// and its `timed_out` result term (resolved in the **module's own** atom table, the one
/// the engine hands back).
fn load_after(dir: &Path) -> (Module, Atom, Eterm) {
    let beam = dir.join("afterrecv.beam");
    let module = load_file(&beam).unwrap_or_else(|e| panic!("load {}: {e:?}", beam.display()));
    let recv = module
        .atom_table
        .lookup("recv")
        .expect("afterrecv exports recv/0");
    let timed_out = module
        .atom_table
        .lookup("timed_out")
        .expect("the after clause names `timed_out`");
    (module, recv, make_atom(timed_out))
}

/// The typed reason atom of an engine raise.
fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("the engine's blocked error carries an atom reason")
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}

#[test]
fn a_nested_after_timeout_fires_when_the_engine_deadline_is_resumed() {
    let Some(dir) = compile_after("nested") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the nested `after T` test is skipped");
        return;
    };
    let (module, recv, timed_out) = load_after(&dir);

    // (1) A **nested** run: the non-blocking entry folds the empty-mailbox `receive`
    //     into the typed `blocked()` error -- exactly the shape a host's nested
    //     `run_export_blocking` observes.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let err = machine
        .run_export(&module, recv, 0, &[], &mut ctx)
        .expect_err("a non-blocking entry folds the wait into the typed blocked error");
    assert_eq!(
        reason(err),
        "unsupported_opcode: run: process blocked (no message available)"
    );

    // The fold must **preserve** the parked state: the machine is still at the
    // `wait_timeout`, so the host can recover the deadline and arm its own timer.
    let deadline = machine
        .wait_deadline(&module)
        .expect("the parked `after 5` deadline survives the non-blocking fold");
    assert_eq!(deadline.delay_ms, Some(5), "the `after 5` delay is readable");

    // (2) Fire the deadline: re-enter at the `timeout` continuation. The `after`
    //     clause must run, and the function must return its value -- not `blocked()`
    //     again.
    let resumed = machine
        .resume_outcome(&module, deadline.resume.0, deadline.resume.1, &mut ctx)
        .expect("resume at the `timeout` continuation");
    assert_eq!(
        resumed,
        RunOutcome::Returned(timed_out),
        "the fired timer must run the `after` clause"
    );

    // The system oracle must agree on the value.
    match oracle_recv(&dir) {
        Some(stdout) => assert_eq!(
            stdout.trim(),
            "timed_out",
            "the oracle disagrees with the engine's `after` value"
        ),
        None => eprintln!("SKIP: no system `erl` on PATH -- the afterrecv oracle check is skipped"),
    }
}

#[test]
fn a_blocking_entry_after_timeout_fires_at_the_deadline() {
    // The blocking-aware entry reports the suspension (instead of folding it); the same
    // deadline -> resume path must then run the `after` clause.
    let Some(dir) = compile_after("blocking") else {
        eprintln!("SKIP: no system `erlc` on PATH -- the blocking `after T` test is skipped");
        return;
    };
    let (module, recv, timed_out) = load_after(&dir);

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_export_outcome(&module, recv, 0, &[], &mut ctx)
        .expect("recv/0");
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "a non-zero `after` with no message parks, got {outcome:?}"
    );

    let deadline = machine
        .wait_deadline(&module)
        .expect("a parked wait_timeout has a deadline");
    assert_eq!(deadline.delay_ms, Some(5));

    let resumed = machine
        .resume_outcome(&module, deadline.resume.0, deadline.resume.1, &mut ctx)
        .expect("resume at the `timeout` continuation");
    assert_eq!(resumed, RunOutcome::Returned(timed_out));
}

// ---------------------------------------------------------------------------
// The cross-module park: the deadline belongs to the callee, not the entry module
// ---------------------------------------------------------------------------

/// The caller module `m`: `run/0` allocates a frame, `call_ext`s `c:recv_timeout/0`,
/// then deallocates and returns the callee's value.
fn caller_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let target = f.import("c", "recv_timeout", 0);
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::CallExt { target, arity: 0 },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The callee module `c`: `recv_timeout/0` — `receive foo -> foo after 20 -> timed_out
/// end` whose timed-out branch returns a distinctive atom.
fn callee_module() -> Arc<Module> {
    let mut f = Fixture::new("c");
    let timed_out = f.atom("timed_out");
    let recv = f.add_function(
        "recv_timeout",
        0,
        vec![
            Instr::Label(Label(10)),
            Instr::LoopRec {
                fail: Label(11),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Return,
            Instr::Label(Label(11)),
            Instr::WaitTimeout {
                target: Label(10),
                time: Operand::Integer(20),
            },
            Instr::Timeout,
            Instr::Move {
                src: Operand::Atom(timed_out),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    f.add_export("recv_timeout", 0, recv);
    Arc::new(f.finish())
}

#[test]
fn a_cross_module_park_reads_the_callee_deadline_not_the_entry_module() {
    // A machine parked inside a cross-module callee: the `(func, pc)` belong to the
    // *callee's* instruction stream (module context != 0), so the deadline must be read
    // from the callee -- never the entry module the caller passed (whose `(func, pc)` is
    // an unrelated instruction, and would report `None`). This is the engine side of the
    // runtime's timeout rearming: reading the entry module would silently lose the
    // deadline and park the process forever.
    let (caller, run) = caller_module();
    let callee = callee_module();
    let mut ctx = seam(vec![("c".to_string(), Arc::clone(&callee))]);

    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("run m:run/0");
    assert!(
        matches!(outcome, RunOutcome::Wait { .. }),
        "the callee receive parks, got {outcome:?}"
    );

    // The deadline is the callee's `after 20`. Reading it against the *entry* module
    // (which has no `wait_timeout` at all) would report `None`.
    let deadline = machine
        .wait_deadline(&caller)
        .expect("the deadline is read from the callee context, not the entry module");
    assert_eq!(
        deadline,
        WaitDeadline {
            delay_ms: Some(20),
            resume: (0, 6),
        }
    );

    // Firing it runs the callee's `after` clause and returns through the caller.
    let resumed = machine
        .resume_outcome(&caller, deadline.resume.0, deadline.resume.1, &mut ctx)
        .expect("resume at the callee's `timeout` continuation");
    let timed_out = make_atom(
        callee
            .atom_table
            .lookup("timed_out")
            .expect("the callee's after clause names `timed_out`"),
    );
    assert_eq!(resumed, RunOutcome::Returned(timed_out));
}