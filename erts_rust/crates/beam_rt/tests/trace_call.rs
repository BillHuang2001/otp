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

//! The `call` / `return_from` / `return_to` execution-trace events (contract A.8, item 1)
//! end to end: a real `.beam`, the **real engine**, the runtime's trace seam and the
//! tracer's mailbox.
//!
//! `beam_rt` implements the engine's
//! [`ExecutionContext::trace_enabled`](beam_engine::ExecutionContext::trace_enabled) /
//! [`ExecutionContext::trace_event`](beam_engine::ExecutionContext::trace_event) pair in
//! `src/context/trace.rs` (matching the installed `erlang:trace_pattern/3` table, building
//! the message, delivering it), and `beam_engine` reports every `call`/`return` it executes
//! to that pair. The unit tests next to the seam drive the hook with **hand-built**
//! [`TraceEvent`](beam_engine::TraceEvent)s, which proves the hook but not that the engine
//! ever calls it. This suite closes that gap: it compiles a fixture with the system `erlc`,
//! installs `erlang:trace/3` flags plus a `[local]` `erlang:trace_pattern/3` entry from a
//! **tracer** process that differs from the **tracee** (ERTS discards an event whose tracer
//! is the tracee, `erts/emulator/nifs/common/erl_tracer_nif.c:143-146`), runs the tracee's
//! code through the engine, and compares the drained messages against the live `erl`
//! oracle, line for line.
//!
//! # The two engine entry points this suite drives
//!
//! **The single-pass path.** Most cases run the tracee with
//! `beam_engine::Machine::run_export` over a
//! [`RuntimeContext::for_process`](beam_rt::RuntimeContext::for_process) — the concrete
//! runtime context, exactly what `Runtime::run_module`/`run_function` (the `-eval`, `-s`,
//! `-run` and external-call path) hands the engine.
//!
//! **The scheduler path** ([`a_scheduled_process_reports_its_call_and_return_to`]). The
//! tracee is an ordinary scheduled process (`Runtime::spawn_process` + `Runtime::run_scheduler`),
//! driven through `crate::runner::budget::BudgetContext`. That wrapper forwards
//! `ExecutionContext::trace_enabled`/`trace_event` to the slice's real `RuntimeContext` like
//! every other method, so a scheduled run latches the gate and reports its `call`/`return_to`
//! events exactly as the single-pass path does. The tracer is itself a scheduled process, so
//! the events land in *its* mailbox and are collected through its own return value.
//!
//! # The fixture
//!
//! `run/0` makes a **non-tail** local call to `a2/2` and then a (pattern-free) call to
//! `r2/0`, so the engine reports `a2`'s `call`, and `a2`'s `return` reports both
//! `return_from` (when a match spec arms it) and the `return_to` of its caller `run/0`.
//! The scheduler-path cases use two more exports with the same traced body: `block/0` (a
//! tracee that blocks on `receive go` until the trace is armed) and `install/1` (a tracer that
//! arms the trace, releases the tracee and returns its own collected mailbox). The module is
//! compiled **at test time** with the system `erlc`; a missing toolchain is a **loud** skip,
//! never a silent pass.
//!
//! # Oracle ground truth (system `erl`, OTP 29.1.1 / `erts-17.1`)
//!
//! ```none
//! $ erlc +deterministic -o <dir> tcall.erl
//! $ erl -noshell -pa <dir> -eval '
//!     Self = self(),
//!     code:ensure_loaded(tcall),
//!     P = spawn(fun() -> receive go -> ok end, tcall:run() end),
//!     erlang:trace(P, true, [call, return_to]),
//!     erlang:trace_pattern({tcall, a2, 2}, true, [local]),
//!     P ! go, timer:sleep(200),
//!     Row = fun({trace,_,call,{M,F,Args}}) -> io:format("call ~w ~w ~w~n",[M,F,Args]);
//!              ({trace,_,return_from,{M,F,A},V}) -> io:format("return_from ~w ~w ~w ~w~n",[M,F,A,V]);
//!              ({trace,_,return_to,{M,F,A}}) -> io:format("return_to ~w ~w ~w~n",[M,F,A]) end,
//!     lists:foreach(Row, element(2, process_info(Self, messages))),
//!     halt(0).'
//! call tcall a2 [1,2]
//! return_to tcall run 0
//! ```
//!
//! The probe must first `code:ensure_loaded(tcall)`: `trace_pattern` only instruments
//! **loaded** modules, so a pattern installed before the module is loaded matches nothing
//! (the oracle answers `0` and emits no event). Every probe ends with `halt(0)` plus the
//! harness's `-s init stop`, and runs under the bounded, always-terminating helper
//! ([`support::oracle_eval`]).
use std::path::{Path, PathBuf};
use std::process::Command;

use beam_engine::{terms, ExecutionContext, Machine};
use beam_loader::{load_file, Module};
use beam_process::{ProcessId, ProcessTable, SignalAtoms};
use beam_rt::{Runtime, RuntimeContext};
use beam_term::prelude::AtomTable;
use beam_term::{make_small, signed_value, Eterm};

mod support;

/// The fixture source (compiled at test time — never committed as a `.beam`).
const FIXTURE_ERL: &str = r#"%% Generated by `beam_rt/tests/trace_call.rs` (never committed).
-module(tcall).
-export([run/0, a2/2, r2/0, block/0, install/1, collect/1]).

run() ->
    X = a2(1, 2),
    r2(),
    X.

a2(A, B) ->
    A + B.

r2() ->
    ok.

%% The **scheduler** path's tracee: it blocks on `receive go` until the tracer has armed
%% the trace, then runs the very same traced body (`a2(1, 2)` + `r2/0`).
block() ->
    receive go -> ok end,
    X = a2(1, 2),
    r2(),
    X.

%% The scheduler path's **tracer**: it is a scheduled process of its own, so `erlang:trace/3`
%% installs *it* as the tracee's tracer, then it releases the tracee and collects its own
%% mailbox. The accumulator is returned **reversed** (the test reverses it).
install(TraceePid) ->
    erlang:trace(TraceePid, true, [call, return_to]),
    erlang:trace_pattern({tcall, a2, 2}, true, [local]),
    TraceePid ! go,
    collect([]).

collect(Acc) ->
    receive
        Msg -> collect([Msg | Acc])
    after 200 ->
            Acc
    end.
"#;

/// The oracle's `-eval` probe, with `TRACEE`, `FLAGS`, `MFA` and `SPEC` substituted per case.
///
/// The tracee **blocks** on `receive go` until the trace is installed: `spawn/3` returns
/// before the child runs, so without the handshake the child could finish its traced body
/// before `erlang:trace/3` installs anything and no event would be produced.
const ORACLE_PROBE: &str = "\
Self = self(), \
code:ensure_loaded(tcall), \
P = spawn(fun() -> TRACEE end), \
erlang:trace(P, true, FLAGS), \
erlang:trace_pattern(MFA, SPEC, [local]), \
P ! go, \
timer:sleep(200), \
Row = fun({trace,_,call,{M,F,Args}}) -> io:format(\"call ~w ~w ~w~n\",[M,F,Args]); \
         ({trace,_,return_from,{M,F,A},V}) -> io:format(\"return_from ~w ~w ~w ~w~n\",[M,F,A,V]); \
         ({trace,_,return_to,{M,F,A}}) -> io:format(\"return_to ~w ~w ~w~n\",[M,F,A]); \
         (M) -> io:format(\"other ~w~n\",[M]) end, \
lists:foreach(Row, element(2, process_info(Self, messages))), \
halt(0).";

/// The single-pass oracle tracee: a spawned process waiting for `go` then running
/// `tcall:run/0` (the body the direct-engine scenarios drive).
const ORACLE_TRACEE_RUN: &str = "receive go -> ok end, tcall:run()";

/// The scheduler-path oracle tracee: `tcall:block/0` blocks on `receive go` **itself**, so the
/// spawn needs no extra handshake — and the `return_to` it produces names `block/0`, exactly as
/// the scheduled Rust tracee's does.
const ORACLE_TRACEE_BLOCK: &str = "tcall:block()";

// ---------------------------------------------------------------------------------------
// The fixture (compiled at test time)
// ---------------------------------------------------------------------------------------

/// A compiled fixture in its own scratch directory.
struct Fixture {
    /// The scratch directory holding `tcall.erl`/`tcall.beam`.
    dir: PathBuf,
    /// The compiled `tcall.beam`.
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
    let dir = std::env::temp_dir().join(format!("beam_rt_trace_call_{label}_{}", std::process::id()));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        support::skip_loudly("trace_call", &format!("cannot create {}: {error}", dir.display()));
        return None;
    }
    let source = dir.join("tcall.erl");
    if let Err(error) = std::fs::write(&source, FIXTURE_ERL) {
        support::skip_loudly("trace_call", &format!("cannot write {}: {error}", source.display()));
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
            support::skip_loudly("trace_call", &format!("cannot run `erlc`: {error}"));
            return None;
        }
    };
    if !output.status.success() {
        support::skip_loudly(
            "trace_call",
            &format!(
                "`erlc` exited with {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            ),
        );
        return None;
    }
    let beam = dir.join("tcall.beam");
    if !beam.is_file() {
        support::skip_loudly("trace_call", &format!("`erlc` produced no {}", beam.display()));
        return None;
    }
    Some(Fixture { dir, beam })
}

// ---------------------------------------------------------------------------------------
// The traced run, in the runtime
// ---------------------------------------------------------------------------------------

/// A traced **tracee** and its distinct **tracer**, in a caller-owned process table, with the
/// fixture module loaded in both the runtime's code server and an engine-owned copy.
struct Scenario {
    rt: Runtime,
    table: ProcessTable,
    /// The engine's copy of the fixture module (the code server keeps its own).
    module: Module,
    tracee: ProcessId,
    tracer: ProcessId,
    /// What `trace_pattern/3` answered: the number of loaded functions the pattern
    /// instrumented (`0` for a pattern on a function the fixture does not define).
    pattern_matches: i64,
}

impl Scenario {
    /// The fixture with `mfa_src` installed as its `[local]` trace pattern (by [`tracer`])
    /// and `flags` installed as the **tracee's** `erlang:trace/3` flags.
    ///
    /// [`tracer`]: Scenario::tracer
    fn new(
        fixture: &Fixture,
        mfa_src: &str,
        flags_src: &str,
        match_spec_src: &str,
        pattern_flags_src: &str,
    ) -> Scenario {
        let mut rt = Runtime::with_defaults().expect("the default runtime");
        // The module must be loaded for `trace_pattern/3` to instrument it (the oracle's
        // `code:ensure_loaded/1`).
        rt.load_module(&fixture.beam).expect("tcall.beam loads");
        let mut atoms = AtomTable::new();
        let mut table = ProcessTable::new(SignalAtoms::from_table(&mut atoms));
        let tracee = table.spawn_default().expect("spawn the tracee");
        let tracer = table.spawn_default().expect("spawn the tracer");

        let how_true = rt.intern("true");
        let flags = rt.eval(flags_src).unwrap_or_else(|e| panic!("{flags_src}: {e:?}"));
        let mfa = rt.eval(mfa_src).unwrap_or_else(|e| panic!("{mfa_src}: {e:?}"));
        let match_spec = rt
            .eval(match_spec_src)
            .unwrap_or_else(|e| panic!("{match_spec_src}: {e:?}"));
        let pattern_flags = rt
            .eval(pattern_flags_src)
            .unwrap_or_else(|e| panic!("{pattern_flags_src}: {e:?}"));

        // `erlang:trace/3` (and its body `erts_internal:trace/3`) always installs the
        // **calling** process as the tracer, so the tracer half of the pair is the context
        // that calls it.
        let pattern_matches;
        {
            let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, tracer);
            let count = ctx
                .call_bif("erlang", "trace", 3, &[tracee.to_eterm(), how_true, flags])
                .expect("trace/3");
            assert_eq!(
                signed_value(count),
                1,
                "the live tracee became the tracee (an unknown pid would answer 0)"
            );
            pattern_matches = ctx
                .call_bif(
                    "erts_internal",
                    "trace_pattern",
                    3,
                    &[mfa, match_spec, pattern_flags],
                )
                .expect("trace_pattern/3");
        }

        let module = load_file(&fixture.beam).expect("load tcall.beam");
        Scenario {
            rt,
            table,
            module,
            tracee,
            tracer,
            pattern_matches: signed_value(pattern_matches),
        }
    }

    /// The number of loaded functions the installed pattern instrumented (the oracle's
    /// `trace_pattern/3` answer).
    fn pattern_matches(&self) -> i64 {
        self.pattern_matches
    }

    /// Runs `tcall:run/0` through the **engine** as the tracee, then drains the tracer's
    /// mailbox in order.
    fn run(&mut self) -> Vec<Eterm> {
        let run = self
            .module
            .atom_table
            .lookup("run")
            .expect("tcall:run/0 is interned in the fixture");
        {
            let mut machine = Machine::new();
            let mut ctx =
                RuntimeContext::for_process(&mut self.rt, &mut self.table, self.tracee);
            let value = machine
                .run_export(&self.module, run, 0, &[], &mut ctx)
                .expect("tcall:run/0");
            assert_eq!(
                signed_value(value),
                3,
                "run/0 evaluates to a2(1, 2) = 3 (the trace must not change the result)"
            );
        }

        let mut ctx = RuntimeContext::for_process(&mut self.rt, &mut self.table, self.tracer);
        ctx.receive_reset().expect("timeout");
        let mut messages = Vec::new();
        while let Some(term) = ctx.receive_next().expect("receive_next") {
            messages.push(term);
            ctx.receive_remove().expect("remove_message");
            assert!(messages.len() < 8, "the mailbox keeps draining: {messages:?}");
        }
        messages
    }

    /// The atom `term` names, panicking when this runtime's shared table does not know it.
    fn name(&self, term: Eterm) -> String {
        atom_name(&self.rt, term)
    }

    /// One canonical line per event, in the shape the oracle probe prints.
    ///
    /// See [`event_row`] for the shapes asserted here.
    fn rows(&mut self, messages: &[Eterm]) -> Vec<String> {
        let tracee = self.tracee;
        event_rows(&mut self.rt, tracee, messages)
    }
}

// ---------------------------------------------------------------------------------------
// The event rows (shared by the direct-engine path and the scheduler path)
// ---------------------------------------------------------------------------------------

/// The atom `term` names in `rt`, panicking when this runtime's shared table does not know it.
fn atom_name(rt: &Runtime, term: Eterm) -> String {
    rt.atom_name_of(term)
        .unwrap_or_else(|| panic!("{term:?} is not an atom this runtime knows"))
        .to_string()
}

/// One canonical line per event, in the shape the oracle probe prints.
///
/// `{trace, Pid, call, {M, F, Args}}` → `call M F Args`,
/// `{trace, Pid, return_from, {M, F, A}, V}` → `return_from M F A V`,
/// `{trace, Pid, return_to, {M, F, A}}` → `return_to M F A`.
fn event_rows(rt: &mut Runtime, tracee: ProcessId, messages: &[Eterm]) -> Vec<String> {
    messages
        .iter()
        .map(|message| event_row(rt, tracee, *message))
        .collect()
}

/// The canonical line of one event message (see [`event_rows`]).
///
/// The shapes are asserted here (tuple arity, the `trace` tag, the tracee, the event atom and
/// the three MFA elements) before the line is built, so a wrong event shape fails loudly rather
/// than rendering an unexpected string.
fn event_row(rt: &mut Runtime, tracee: ProcessId, message: Eterm) -> String {
    let arity = terms::tuple_arity(message).expect("an event tuple");
    assert!(
        arity == 4 || arity == 5,
        "the oracle's event shapes are 4- or 5-tuples: {message:?}"
    );
    assert_eq!(
        atom_name(rt, terms::tuple_element(message, 0).expect("the tag")),
        "trace",
        "an untimestamped event is tagged `trace`"
    );
    assert_eq!(
        terms::tuple_element(message, 1).expect("the tracee"),
        tracee.to_eterm(),
        "the event names the traced process"
    );
    let event = atom_name(rt, terms::tuple_element(message, 2).expect("the event atom"));
    let mfa = terms::tuple_element(message, 3).expect("the MFA");
    assert_eq!(terms::tuple_arity(mfa).expect("an MFA tuple"), 3);
    let module = atom_name(rt, terms::tuple_element(mfa, 0).expect("the module"));
    let function = atom_name(rt, terms::tuple_element(mfa, 1).expect("the function"));
    let third = terms::tuple_element(mfa, 2).expect("the third element");
    match event.as_str() {
        // The argument list — or, with the `arity` process flag, that flag's own value.
        "call" => {
            assert_eq!(arity, 4, "`call` is a 4-tuple: {message:?}");
            let args = rt
                .format_term_w(third, None)
                .expect("the call's argument list prints")
                .trim_end()
                .to_string();
            format!("call {module} {function} {args}")
        }
        "return_from" => {
            assert_eq!(arity, 5, "`return_from` is a 5-tuple: {message:?}");
            let number = signed_value(third);
            let value = terms::tuple_element(message, 4).expect("the returned value");
            let value = rt
                .format_term_w(value, None)
                .expect("the returned value prints")
                .trim_end()
                .to_string();
            format!("return_from {module} {function} {number} {value}")
        }
        "return_to" => {
            assert_eq!(arity, 4, "`return_to` is a 4-tuple: {message:?}");
            let number = signed_value(third);
            format!("return_to {module} {function} {number}")
        }
        other => panic!("unexpected trace event {other}"),
    }
}

// ---------------------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------------------

/// The live oracle's canonical lines for one probe, or `None` when the probe could not
/// answer (a loud skip is the caller's).
fn oracle_rows(
    oracle: &Path,
    dir: &Path,
    tracee_src: &str,
    mfa_src: &str,
    flags_src: &str,
    match_spec_src: &str,
) -> Option<Vec<String>> {
    let expr = ORACLE_PROBE
        .replace("TRACEE", tracee_src)
        .replace("FLAGS", flags_src)
        .replace("MFA", mfa_src)
        .replace("SPEC", match_spec_src);
    let stdout = support::oracle_eval(oracle, Some(dir), &expr)?;
    Some(
        stdout
            .lines()
            .map(str::to_string)
            .filter(|line| !line.is_empty())
            .collect(),
    )
}

/// Runs one scenario under both runtimes and asserts the runtime's lines equal `expected`
/// **and** the live oracle's.
fn check(
    label: &str,
    mfa_src: &str,
    flags_src: &str,
    match_spec_src: &str,
    expected: &[&str],
) {
    let Some(fixture) = build_fixture(label) else {
        return;
    };
    let mut scenario = Scenario::new(&fixture, mfa_src, flags_src, match_spec_src, "[local]");
    assert_eq!(
        scenario.pattern_matches(),
        1,
        "the installed pattern instrumented exactly the loaded fixture's export ({label})"
    );
    let messages = scenario.run();
    let rows = scenario.rows(&messages);
    let expected: Vec<String> = expected.iter().map(|line| (*line).to_string()).collect();
    assert_eq!(rows, expected, "the runtime's events, in order ({label})");

    if let Some(oracle) = support::require_oracle("trace_call") {
        let oracle_rows = oracle_rows(
            &oracle,
            &fixture.dir,
            ORACLE_TRACEE_RUN,
            mfa_src,
            flags_src,
            match_spec_src,
        )
        .unwrap_or_else(|| panic!("the oracle probe for {label} could not answer"));
        assert_eq!(
            rows, oracle_rows,
            "the runtime's events must match the live oracle's, line for line ({label})"
        );
    }
    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------------------

/// The process `call` flag turns the engine's `Call` into `{trace, Pid, call, {M, F, Args}}`,
/// and the `return_to` flag turns the `return`'s `ReturnTo` into
/// `{trace, Pid, return_to, {Caller, F, A}}` — never `return_from`, which no `erlang:trace/3`
/// flag arms (it needs a `{return_trace}` match-spec **action**).
///
/// The `true` (`MatchAll`) match spec is exactly the oracle case above.
#[test]
fn a_call_and_its_return_to_are_reported_in_order() {
    check(
        "call_return_to",
        "{tcall, a2, 2}",
        "[call, return_to]",
        "true",
        &["call tcall a2 [1,2]", "return_to tcall run 0"],
    );
}

/// `return_from` is armed by the match function's `{return_trace}` **action**, not by a flag,
/// and carries the returned value (`a2(1, 2)` → `3`). The `return_to` flag is off here, so
/// the same return reports no `return_to`.
#[test]
fn the_return_from_event_needs_its_match_spec_action() {
    check(
        "return_from",
        "{tcall, a2, 2}",
        "[call]",
        "[{'_',[],[{return_trace}]}]",
        &["call tcall a2 [1,2]", "return_from tcall a2 2 3"],
    );
}

/// `[call]` alone (with a `MatchAll` spec) reports the call and nothing on the return: the
/// file's other local call (`r2/0`) is matched by no pattern and so reports nothing at all.
#[test]
fn call_alone_reports_the_call_and_nothing_on_the_return() {
    check(
        "call_only",
        "{tcall, a2, 2}",
        "[call]",
        "true",
        &["call tcall a2 [1,2]"],
    );
}

/// The `arity` process flag replaces the `call` event's argument list with the bare arity.
#[test]
fn the_arity_flag_replaces_the_argument_list() {
    check(
        "arity",
        "{tcall, a2, 2}",
        "[call, arity]",
        "true",
        &["call tcall a2 2"],
    );
}

/// A pattern on the fixture's **second** local call (`r2/0`) reports *its* call — with the
/// empty argument list of an arity-0 call — and its own `return_to`.
#[test]
fn a_pattern_on_the_second_call_reports_that_call() {
    check(
        "second_call",
        "{tcall, r2, 0}",
        "[call, return_to]",
        "true",
        &["call tcall r2 []", "return_to tcall run 0"],
    );
}

/// The event **terms** have the oracle's exact shape, asserted structurally (not only through
/// the rendered lines): `{trace, Tracee, call, {tcall, a2, [1, 2]}}`,
/// `{trace, Tracee, return_from, {tcall, a2, 2}, 3}` and `{trace, Tracee, return_to, {tcall,
/// run, 0}}`.
#[test]
fn the_event_terms_have_the_oracles_exact_shape() {
    let Some(fixture) = build_fixture("shape") else {
        return;
    };

    // The `call` + `return_to` pair.
    let mut scenario = Scenario::new(
        &fixture,
        "{tcall, a2, 2}",
        "[call, return_to]",
        "true",
        "[local]",
    );
    let tracee = scenario.tracee.to_eterm();
    let messages = scenario.run();
    assert_eq!(messages.len(), 2, "the call and the return_to: {messages:?}");

    let call = messages[0];
    assert_eq!(terms::tuple_arity(call).expect("a tuple"), 4);
    assert_eq!(scenario.name(terms::tuple_element(call, 0).unwrap()), "trace");
    assert_eq!(terms::tuple_element(call, 1).unwrap(), tracee);
    assert_eq!(scenario.name(terms::tuple_element(call, 2).unwrap()), "call");
    let mfa = terms::tuple_element(call, 3).unwrap();
    assert_eq!(terms::tuple_arity(mfa).unwrap(), 3);
    assert_eq!(scenario.name(terms::tuple_element(mfa, 0).unwrap()), "tcall");
    assert_eq!(scenario.name(terms::tuple_element(mfa, 1).unwrap()), "a2");
    let args = terms::list_elements(terms::tuple_element(mfa, 2).unwrap())
        .expect("the call's argument list");
    assert_eq!(args, vec![make_small(1), make_small(2)], "a2(1, 2)'s arguments");

    let return_to = messages[1];
    assert_eq!(terms::tuple_arity(return_to).expect("a tuple"), 4);
    assert_eq!(terms::tuple_element(return_to, 1).unwrap(), tracee);
    assert_eq!(
        scenario.name(terms::tuple_element(return_to, 2).unwrap()),
        "return_to"
    );
    let mfa = terms::tuple_element(return_to, 3).unwrap();
    assert_eq!(scenario.name(terms::tuple_element(mfa, 0).unwrap()), "tcall");
    assert_eq!(scenario.name(terms::tuple_element(mfa, 1).unwrap()), "run");
    assert_eq!(signed_value(terms::tuple_element(mfa, 2).unwrap()), 0);

    // The `return_from` event, with its returned value.
    let mut scenario = Scenario::new(
        &fixture,
        "{tcall, a2, 2}",
        "[call]",
        "[{'_',[],[{return_trace}]}]",
        "[local]",
    );
    let tracee = scenario.tracee.to_eterm();
    let messages = scenario.run();
    assert_eq!(messages.len(), 2, "the call and the return_from: {messages:?}");

    let return_from = messages[1];
    assert_eq!(terms::tuple_arity(return_from).expect("a tuple"), 5);
    assert_eq!(terms::tuple_element(return_from, 1).unwrap(), tracee);
    assert_eq!(
        scenario.name(terms::tuple_element(return_from, 2).unwrap()),
        "return_from"
    );
    let mfa = terms::tuple_element(return_from, 3).unwrap();
    assert_eq!(scenario.name(terms::tuple_element(mfa, 0).unwrap()), "tcall");
    assert_eq!(scenario.name(terms::tuple_element(mfa, 1).unwrap()), "a2");
    assert_eq!(signed_value(terms::tuple_element(mfa, 2).unwrap()), 2);
    assert_eq!(
        signed_value(terms::tuple_element(return_from, 4).unwrap()),
        3,
        "a2(1, 2)'s value"
    );

    fixture.cleanup();
}

/// Tracing **never changes the traced program**: with no trace at all the same run yields the
/// same value and an empty tracer mailbox.
#[test]
fn an_untraced_run_reports_nothing() {
    let Some(fixture) = build_fixture("untraced") else {
        return;
    };
    let mut scenario = Scenario::new(&fixture, "{tcall, nope, 0}", "[call, return_to]", "true", "[local]");
    assert_eq!(
        scenario.pattern_matches(),
        0,
        "the fixture has no `nope/0`, so nothing is instrumented"
    );
    let messages = scenario.run();
    assert!(
        messages.is_empty(),
        "a pattern that matches no function reports nothing: {messages:?}"
    );
    fixture.cleanup();
}

// ---------------------------------------------------------------------------------------
// The scheduler path
// ---------------------------------------------------------------------------------------

/// **The scheduler path reports the same events.** A tracee driven through the runtime's real
/// scheduler (`Runtime::spawn_process` + `Runtime::run_scheduler`, under
/// `crate::runner::budget::BudgetContext`) reports its `call`/`return_to` events into a
/// **different tracer process's** mailbox, exactly as the single-pass path does.
///
/// Both processes are scheduled: the tracee (`tcall:block/0`) blocks on `receive go` until the
/// tracer (`tcall:install/1`) has armed the trace — so the trace is installed *before* the
/// traced body runs — and the tracer then collects its own mailbox and returns it. Its result
/// is therefore the event list (collected in reverse; the test reverses it), read back through
/// `Runtime::process_result`.
#[test]
fn a_scheduled_process_reports_its_call_and_return_to() {
    let Some(fixture) = build_fixture("scheduled") else {
        return;
    };
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    rt.load_module(&fixture.beam).expect("tcall.beam loads");

    // The tracee is spawned FIRST (it runs first and blocks on `receive go`); the tracer is
    // spawned SECOND with the tracee's pid, so it can arm the trace before releasing it.
    let tracee = rt
        .spawn_process("tcall", "block", vec![])
        .expect("spawn tcall:block/0");
    let tracer = rt
        .spawn_process("tcall", "install", vec![tracee.to_eterm()])
        .expect("spawn tcall:install/1");

    // The full scheduler: the tracee parks, the tracer arms the trace and releases it, the
    // resumed tracee reports its events, and the tracer's `after 200` deadline ends it.
    rt.run_scheduler().expect("the scheduler drains the queue");

    let collected = rt
        .process_result(tracer)
        .expect("the tracer returned its collected events");
    let mut messages = terms::list_elements(collected).expect("the tracer returns a list");
    messages.reverse();
    assert_eq!(
        messages.len(),
        2,
        "the call and the return_to: {messages:?}"
    );

    let rows = event_rows(&mut rt, tracee, &messages);
    // The `return_to` names the *caller* of `a2/2`, which is the traced body's function
    // `block/0` (the single-pass cases trace `run/0` instead, so theirs names `run/0`).
    let expected = ["call tcall a2 [1,2]", "return_to tcall block 0"];
    assert_eq!(rows, expected, "the scheduled run's events, in order");

    // The same oracle ground truth, through an oracle tracee that blocks the same way
    // (`tcall:block/0`), so the event lines — including the `return_to` caller — are identical.
    if let Some(oracle) = support::require_oracle("trace_call") {
        let oracle_rows = oracle_rows(
            &oracle,
            &fixture.dir,
            ORACLE_TRACEE_BLOCK,
            "{tcall, a2, 2}",
            "[call, return_to]",
            "true",
        )
        .unwrap_or_else(|| panic!("the scheduled-path oracle probe could not answer"));
        assert_eq!(
            rows, oracle_rows,
            "the scheduled run's events must match the live oracle's, line for line"
        );
    }
    fixture.cleanup();
}
