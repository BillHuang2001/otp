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

//! Unit tests for the outcome bookkeeping: the recorded result, the mapped exit
//! reason and the typed error for a pid with no engine. The end-to-end behaviour
//! (blocking, resume-on-send) is `tests/scheduler.rs`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_engine::context::RootSet;
use beam_engine::jit::cache::{baseline_cache_len, clear_baseline_cache};
use beam_engine::machine::WaitDeadline;
use beam_engine::{terms, Machine, Mailbox, MinimalContext, RunOutcome as EngineOutcome, Tier};
use beam_loader::{Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand};
use beam_process::{
    ProcessId, ProcessRunner, ProcessState, ProcessTable, RunOutcome as ProcOutcome, SignalAtoms,
    DEFAULT_REDUCTIONS_PER_SLICE,
};
use beam_term::{make_atom, make_small, signed_value, Atom, AtomTable, Eterm, NIL};

use super::budget::{BudgetContext, BudgetProbe};
use super::entry::{ProcessEntry, RunnerState};
use super::perf::record_baseline_code;
use super::reason::{exit_reason, normal};
use super::run::{
    boundary_door_counters, park_publish_counters, park_window, publish_parked_roots,
    reset_boundary_door_counters, reset_park_publish_counters, reset_run_slice_counters,
    run_slice_counters, ParkExemption, ParkPublishCounters, RuntimeRunner,
};
use super::tier::{mode_from_value, select_entry_tier, tier_unavailable, JitMode};
mod boundary_cost;
mod reds_door;
mod slice_charging;
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// A process table independent of the runtime (what the scheduler owns).
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

/// A pid the scheduler hands to the runner with **no engine entry point** is *diagnosed*,
/// never fatal: one unindexed run-queue entry must not take a whole pass — and the node —
/// down.
///
/// The runner owns both halves of the refusal, because nobody else touches the process in
/// this window: the typed record ([`UnrunnableProcess`]) and the state restore. The
/// scheduler's `slice_begin` has already taken the pid off the run queue and marked it
/// [`ProcessState::Running`], and `slice_end`'s `Wait` arm changes nothing, so leaving it
/// alone would leave the pid `Running` forever — a lie that also flips
/// [`Process::has_unpublished_roots`](beam_process::Process::has_unpublished_roots).
#[test]
fn a_pid_without_an_engine_is_diagnosed_and_left_runnable() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    // What the scheduler's `slice_begin` does before the runner ever sees the pid.
    {
        let process = table.get_mut(pid).expect("a fresh process stays in the table");
        process.set_state(ProcessState::Running);
        process.flags_mut().set_in_run_queue(false);
    }

    let mut runner = RuntimeRunner::new(&mut rt, &mut state);
    let outcome = runner
        .run(pid, &mut table, 1)
        .expect("an engine-less pid is not an error");
    assert_eq!(
        outcome,
        ProcOutcome::Wait,
        "it left the run queue and nothing will run it — the honest outcome"
    );
    drop(runner);

    let diagnostic = state.unrunnable(pid).expect("the pid is diagnosed");
    assert_eq!(diagnostic.pid, pid);
    assert_eq!(
        diagnostic.state,
        ProcessState::Running,
        "the record says what the pass actually observed, before any restore"
    );
    assert!(!diagnostic.in_run_queue, "`slice_begin` took it off the queue");
    assert_eq!(
        diagnostic.registered_name, None,
        "a bare `spawn_default` process is unregistered"
    );
    let text = diagnostic.to_string();
    assert!(text.contains("has no engine entry point"), "{text}");
    assert!(text.contains(&format!("{pid:?}")), "the message names the pid: {text}");

    let restored = table.get(pid).expect("it stays in the table");
    assert_eq!(
        restored.state(),
        ProcessState::Runnable,
        "the `Running` lie `slice_begin` wrote is undone"
    );
    assert!(!restored.flags().in_run_queue(), "nothing enqueued it");

    // A pid is diagnosed **once**: a second refusal keeps the first observation rather than
    // burying it under a later one.
    let now_waiting = table.get_mut(pid).expect("it stays in the table");
    now_waiting.set_state(ProcessState::Waiting);
    let mut runner = RuntimeRunner::new(&mut rt, &mut state);
    assert_eq!(
        runner.run(pid, &mut table, 1).expect("still not an error"),
        ProcOutcome::Wait
    );
    drop(runner);
    assert_eq!(
        state.unrunnable(pid).expect("still recorded").state,
        ProcessState::Running,
        "the first record is kept verbatim"
    );
    assert_eq!(
        table.get(pid).expect("it stays").state(),
        ProcessState::Runnable,
        "every refusal restores a truthful state"
    );
    // The consuming half hands the record over and clears it.
    let taken = state.take_unrunnable(pid).expect("the record");
    assert_eq!(taken.pid, pid);
    assert!(state.unrunnable(pid).is_none(), "taking consumes it");
}

#[test]
fn a_pid_that_left_the_process_table_is_refused_without_a_record() {
    // The runner is handed a pid the table no longer holds (terminated between the enqueue
    // and the slice): there is nothing to describe and nothing to restore, and the outcome is
    // still the non-fatal `Wait`.
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = ProcessId::new(4242, 0);
    assert!(table.get(pid).is_none(), "the fixture pid is not live");

    let mut runner = RuntimeRunner::new(&mut rt, &mut state);
    assert_eq!(
        runner.run(pid, &mut table, 1).expect("not an error"),
        ProcOutcome::Wait
    );
    drop(runner);
    assert!(
        state.unrunnable(pid).is_none(),
        "a pid with no PCB cannot be described"
    );
}
#[test]
fn the_standard_error_atoms_become_atom_reasons() {
    let mut rt = runtime();
    assert_eq!(
        exit_reason(&mut rt, &BeamError::BadArith),
        rt.intern("badarith")
    );
    assert_eq!(
        exit_reason(&mut rt, &BeamError::SystemLimit),
        rt.intern("system_limit")
    );
    // A raise whose reason is an OWNED atom (`BeamError::as_atom` cannot see it).
    let owned = BeamError::Raise(beam_common::Raise::error(ExitReason::atom(String::from(
        "unsupported_opcode: nope",
    ))));
    let reason = exit_reason(&mut rt, &owned);
    assert_eq!(rt.atom_name_of(reason), Some("unsupported_opcode: nope"));
}

#[test]
fn a_reasonless_error_falls_back_to_its_diagnostic_text() {
    // `TermNotFound` carries no atom; the message is interned instead, so the
    // failure is still observable rather than silently dropped.
    let mut rt = runtime();
    let reason = exit_reason(&mut rt, &BeamError::TermNotFound);
    assert_eq!(rt.atom_name_of(reason), Some(BeamError::TermNotFound.to_string().as_str()));
}

#[test]
fn the_recorded_result_is_readable_after_the_pass() {
    let mut state = RunnerState::default();
    let pid = ProcessId::new(7, 0);
    assert!(state.result(pid).is_none());
    assert!(state.exit(pid).is_none());
    state.results.insert(pid, make_small(41));
    assert_eq!(signed_value(state.result(pid).expect("recorded")), 41);
}

#[test]
fn spawn_registers_an_engine_and_reports_it() {
    // A module is not needed to register the engine (the entry is only resolved when
    // the process runs), so this pins the bookkeeping alone.
    let mut state = RunnerState::default();
    let pid = ProcessId::new(3, 0);
    assert!(!state.contains(pid));
    let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
    let function = module.atom_table.lookup("add").expect("add is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 2,
        },
        vec![make_small(2), make_small(3)],
    );
    assert!(state.contains(pid));
    assert!(state.result(pid).is_none());
}

#[test]
fn spawn_fun_registers_a_fun_engine() {
    // The `erlang:spawn/1` bookkeeping: a fun-bodied process has an engine (so the
    // scheduler can run it and a re-enqueue is not a silent no-op), with no module
    // and no recorded outcome yet.
    let mut state = RunnerState::default();
    let pid = ProcessId::new(5, 0);
    assert!(!state.contains(pid));
    state.spawn_fun(pid, make_small(1), Vec::new());
    assert!(state.contains(pid));
    assert!(state.result(pid).is_none());
    assert!(state.exit(pid).is_none());
}

/// Whether `error` is the probe's typed pre-emption refusal -- the `unsupported`-prefixed
/// signal the runner keys off to park the process instead of recording an exit.
fn is_preemption(error: &BeamError) -> bool {
    crate::error::unsupported_reason(error)
        .is_some_and(|reason| reason.contains("reduction budget"))
}

/// A fresh probe is never exhausted; the `.max(1)` floor buys exactly one unit, so a slice
/// can never spin; and the refusal past exhaustion is sticky (no unit is charged).
#[test]
fn a_zero_budget_still_charges_exactly_one_unit_and_then_refuses() {
    let mut probe = BudgetProbe::new(0);
    assert!(!probe.is_exhausted(), "a fresh probe is not exhausted");
    assert!(probe.charge().is_ok(), "the floor guarantees one charge");
    assert_eq!(probe.used(), 1);
    assert!(!probe.is_exhausted(), "one unit of progress is not exhaustion");

    let error = probe.charge().expect_err("the floor is spent");
    assert!(is_preemption(&error), "{error}");
    assert!(probe.is_exhausted());
    assert!(probe.charge().is_err(), "an exhausted probe stays exhausted");
    assert_eq!(probe.used(), 1, "no unit past the floor is charged");
}

/// A budget of `n` allows exactly `n` charges -- never exhausted while units remain -- and
/// the `(n+1)`-th charge is the single refusal the runner turns into `RunOutcome::Yield`.
#[test]
fn a_budget_allows_exactly_its_units_then_refuses() {
    const N: u32 = 3;
    let mut probe = BudgetProbe::new(N);
    for charged in 1..=N {
        assert!(!probe.is_exhausted(), "units remain before charge {charged}");
        assert!(probe.charge().is_ok(), "charge {charged} of {N}");
        assert_eq!(probe.used(), charged);
    }
    assert!(!probe.is_exhausted(), "the budget is spent, not yet exceeded");

    let error = probe.charge().expect_err("the (n+1)-th charge is the refusal");
    assert!(is_preemption(&error), "{error}");
    assert!(probe.is_exhausted());
    assert_eq!(probe.used(), N, "the refusal charges nothing");
}

/// **The slice wrapper is transparent for the local-call seam.**
///
/// `BudgetContext` is the `ExecutionContext` a scheduled slice's machine runs on, so the
/// engine's three local-call arms (`Call`/`CallOnly`/`CallLast`) and the native baseline tier's
/// `jit_local_call` probe consult **it**, never the wrapped `RuntimeContext`. Without the
/// forward the trait's default `Ok(false)` applies: every local call is declined and jumps
/// in-machine into the callee's own code, so a built-in/static NIF library stub
/// (`prim_tty:tty_create/1`, body `erlang:nif_error(undef)`) raises `undef` instead of reaching
/// the host that serves it.
///
/// The seam is driven through the wrapper and every answer is cross-checked against the bare
/// inner context, so this pins the **forward** (a served key stays served, an unserved one
/// stays declined) rather than a constant: dropping the forward flips the served key from
/// `true` to the default `false` and fails the assertion.
#[test]
fn the_slice_wrapper_forwards_the_local_call_seam() {
    let mut rt = runtime();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");

    // A key the runtime really serves (a `prim_tty` NIF stub) and one it must decline.
    let served = crate::native::prim_tty::LIBRARY;
    let unserved = "lists";

    // The bare context's own answers — the ground truth the wrapper must reproduce.
    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    let inner_served =
        beam_engine::ExecutionContext::resolve_local_call(&mut ctx, served, "tty_create", 1)
            .expect("the seam answers");
    let inner_unserved =
        beam_engine::ExecutionContext::resolve_local_call(&mut ctx, unserved, "reverse", 1)
            .expect("the seam answers");
    assert!(inner_served, "the runtime serves prim_tty:tty_create/1");
    assert!(!inner_unserved, "the runtime declines lists:reverse/1");

    // The same two keys through the slice wrapper: identical answers.
    let mut probe = BudgetProbe::new(1_000);
    let mut budget_ctx = BudgetContext {
        inner: &mut ctx,
        probe: &mut probe,
    };
    assert_eq!(
        beam_engine::ExecutionContext::resolve_local_call(&mut budget_ctx, served, "tty_create", 1)
            .expect("the seam answers"),
        inner_served,
        "the wrapper must forward the served key, not fall back to the `Ok(false)` default"
    );
    assert_eq!(
        beam_engine::ExecutionContext::resolve_local_call(&mut budget_ctx, unserved, "reverse", 1)
            .expect("the seam answers"),
        inner_unserved,
        "the wrapper must forward the declined key"
    );
}

/// **The slice wrapper is transparent for the foreign-record-definition seam.**
///
/// `BudgetContext` is the `ExecutionContext` a scheduled slice's machine runs on, so a
/// `put_record` **create** whose `Id` names **another** module's record asks *it* for the
/// definition (`beam_engine::ExecutionContext::foreign_record_definition`), never the wrapped
/// `RuntimeContext`. Without the forward the trait's default `Ok(None)` applies — "this host has
/// no record table" — and the family answers its typed
/// `… create: no record definition table for another module` instead of building the record.
///
/// The seam is driven through the wrapper and cross-checked against the bare inner context, so
/// this pins the **forward** (the same definition, verbatim) rather than a constant: dropping
/// the forward flips the answer to `None` and fails the assertion. It is deliberately **not
/// priced** — a table lookup is not a call resolution — so the probe must stay at `0`.
#[test]
fn the_slice_wrapper_forwards_the_foreign_record_definition_seam() {
    let mut rt = runtime();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");

    // The committed native-record fixture pair's owner image: `nr_def` defines and exports
    // `#rec{a, b}`, and the using module's `{Module,Name}` literal carries the placeholder
    // owner `recdef` — registered here under exactly that name.
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/nr_def.beam"
    ))
    .expect("the committed nr_def fixture reads");
    let module = rt.code_server.load_bytes(&bytes).expect("nr_def decodes");
    rt.code_server.insert_loaded(
        "recdef",
        module,
        crate::codeserver::RecordedName::Str("recdef.beam".to_string()),
    );

    let owner = beam_term::atom_val(rt.intern("recdef"));
    let name = beam_term::atom_val(rt.intern("rec"));

    // The bare context's own answer — the ground truth the wrapper must reproduce.
    let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
    let inner = beam_engine::ExecutionContext::foreign_record_definition(&mut ctx, owner, name)
        .expect("the seam answers")
        .expect("the loaded owner's exported `rec` is answered");
    assert!(inner.is_exported, "`-export_record([rec])` is in the definition");
    assert_eq!(inner.name, name);
    assert_eq!(inner.fields.len(), 2, "`#rec{{a, b}}` declares two fields");

    // The same request through the slice wrapper: the identical definition, at no cost.
    let mut probe = BudgetProbe::new(1_000);
    let mut budget_ctx = BudgetContext {
        inner: &mut ctx,
        probe: &mut probe,
    };
    let forwarded =
        beam_engine::ExecutionContext::foreign_record_definition(&mut budget_ctx, owner, name)
            .expect("the seam answers")
            .expect("the wrapper must forward, not fall back to the `Ok(None)` default");
    assert_eq!(
        forwarded, inner,
        "the wrapper must forward the definition verbatim"
    );
    assert_eq!(
        probe.used(),
        0,
        "the seam is a table lookup, not a call resolution — it charges no reduction"
    );
}

/// A scheduled export runs to completion through the runner's **tier-aware** first-entry
/// path: its value is recorded and it exits `normal` (the selected tier is documented there).
#[test]
fn a_scheduled_export_runs_through_the_tier_aware_entry() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
    let function = module.atom_table.lookup("add").expect("add is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 2,
        },
        vec![make_small(2), make_small(3)],
    );

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner.run(pid, &mut table, 1_000).expect("the slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    assert_eq!(
        signed_value(state.result(pid).expect("the value is recorded")),
        5,
        "the tier-aware entry computes the interpreted result"
    );
}

/// The `+JPperf` map is filled from the baseline artifact's **real** code ranges -- and a
/// disabled flag (the default) compiles nothing and records nothing.
#[test]
fn the_baseline_code_map_fills_only_an_enabled_perf_map() {
    let module = beam_loader::load_file(slice_fixture()).expect("slice.beam");

    let mut off = runtime();
    assert!(!off.perf_map().config().map_enabled(), "+JPperf is off by default");
    record_baseline_code(&mut off, &module);
    assert_eq!(off.perf_map().entry_count(), 0, "a disabled map costs nothing");
    assert!(!off.perf_map().is_active(), "and creates no file");

    let dir = std::env::temp_dir().join(format!("beam_rt_runner_perf_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
    let mut perf = crate::perf_map::PerfMapConfig::new();
    perf.set_directory(&dir);
    perf.set_flag("map").expect("map is a grammar value");
    let mut on = Runtime::new(crate::config::RuntimeConfig::new().with_perf_map(perf))
        .expect("the runtime is created");

    record_baseline_code(&mut on, &module);
    assert_eq!(on.perf_map().error(), None);
    assert!(on.perf_map().entry_count() > 0, "the code map has real regions");
    let text = std::fs::read_to_string(on.perf_map().map_path().expect("the map is open"))
        .expect("the map is readable");
    for line in text.lines() {
        assert!(
            line.starts_with("0x") && line.contains(" $slice:"),
            "a real range of the module: {line:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}    #[test]
fn a_returned_value_is_not_the_exit_reason() {
    // The runner DOES return `RunOutcome::Yield` once this slice's reduction budget is
    // spent (pre-emption is implemented -- see `BudgetProbe`), but that is not what this
    // pins: a process that *returns* exits with the `normal` atom -- NOT with the value it
    // produced (the value is recorded separately by `RunnerState::result`) -- so its
    // outcome is a termination, never a pre-emption.
    let mut rt = runtime();
    let reason = normal(&mut rt);
    assert_eq!(rt.atom_name_of(reason), Some("normal"));
    assert_ne!(
        ProcOutcome::Exit(reason),
        ProcOutcome::Yield,
        "a normal exit is a termination, not a pre-emption"
    );
}

/// The committed loader fixture, reused by relative path (never copied).
fn slice_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_loader/tests/fixtures/slice.beam"
    )
}

// -- finite `receive … after T` deadline recovery -------------------------

/// The committed finite-receive fixture, reused by relative path.
fn recv_after_fixture() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/recv_after.beam")
}

/// `(function index, code index of the `wait_timeout`, scan code index)` for the finite
/// receive of `module`'s `name/0`.
///
/// The scan index is the code index of the `wait_timeout`'s jump-back label — the
/// `loop_rec` the engine parks at (what it reports as the
/// [`Wait`](beam_engine::RunOutcome::Wait) address).
fn finite_receive(module: &Module, name: &str) -> (u32, usize, u32) {
    let atom = module.atom_table.lookup(name).expect("the export is interned");
    let index = module
        .functions
        .iter()
        .position(|f| f.name == atom && f.arity == 0)
        .expect("the fixture defines it");
    let function = &module.functions[index];
    let (code_index, target) = function
        .code
        .iter()
        .enumerate()
        .find_map(|(i, instr)| match instr {
            Instr::WaitTimeout { target, .. } => Some((i, *target)),
            _ => None,
        })
        .expect("the export has a finite receive");
    let scan = function.labels.get(&target).copied().expect("the jump-back label");
    (index as u32, code_index, scan as u32)
}

#[test]
fn a_finite_receive_recovers_its_timed_out_continuation() {
    // The compiled shape the runner reads, straight from `beam_disasm`:
    //   Scan: loop_rec Fail,Dst | … | Fail: wait_timeout Scan,{integer,T} | timeout | …
    // The engine parks at `Scan` (the `wait_timeout` jump-back target), and — per the C
    // (`emu/msg_instrs.tab:356-379`: on timeout control goes to the instruction
    // *following* `wait_timeout`) — the timed-out continuation is the `timeout` opcode.
    //
    // The deadline is recovered through `Machine::wait_deadline` on a **really parked**
    // machine (an unparked one has no `current_code` and reports `None`), which is exactly
    // what the runner's `Wait` arm consumes.
    let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
    let (func, wait_index, scan) = finite_receive(&module, "after20");
    let after20 = module.atom_table.lookup("after20").expect("after20 is interned");

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_export_outcome(&module, after20, 0, &[], &mut ctx)
        .expect("run after20/0");
    assert_eq!(
        outcome,
        EngineOutcome::Wait { func, pc: scan },
        "a non-zero `after` with no message parks at the scan address"
    );

    let deadline = machine
        .wait_deadline(&module)
        .expect("after20 arms a 20 ms deadline");
    assert_eq!(deadline.delay_ms, Some(20));
    assert_eq!(
        deadline.resume,
        (func, wait_index as u32 + 1),
        "resume at the instruction after wait_timeout"
    );
    assert!(
        matches!(module.functions[func as usize].code.get(wait_index + 1), Some(Instr::Timeout)),
        "the timed-out path is the `timeout` instruction"
    );
}

#[test]
fn an_infinite_receive_arms_no_deadline() {
    // `after infinity` compiles to a bare `wait`: there is no `wait_timeout` to recover
    // a delay from, so the process parks indefinitely (never a fabricated deadline).
    let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
    let atom = module.atom_table.lookup("wait_forever").expect("wait_forever");
    let function = module.function(atom, 0).expect("wait_forever/0");
    assert!(
        !function.code.iter().any(|i| matches!(i, Instr::WaitTimeout { .. })),
        "a bare wait has no wait_timeout at all"
    );

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_export_outcome(&module, atom, 0, &[], &mut ctx)
        .expect("run wait_forever/0");
    assert!(
        matches!(outcome, EngineOutcome::Wait { .. }),
        "a bare wait parks, got {outcome:?}"
    );
    assert_eq!(
        machine.wait_deadline(&module),
        None,
        "an infinite receive never times out"
    );
}

#[test]
fn a_timeout_delay_must_be_a_statically_known_positive_millisecond_count() {
    // The delay decoding is the engine's `Machine::timeout_delay` (the runner no longer
    // owns a copy); this pins the value semantics the `delay_ms: Some(d)` gate relies on.
    let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
    let machine = Machine::new();
    // A literal positive delay is a deadline.
    assert_eq!(machine.timeout_delay(&module, &Operand::Integer(20)), Some(20));
    // Zero and negative are not delays (the engine handles a bad/`0` time itself), and
    // neither is a register the machine cannot resolve — an honest "no deadline" rather
    // than a guessed one.
    assert_eq!(machine.timeout_delay(&module, &Operand::Integer(0)), None);
    assert_eq!(machine.timeout_delay(&module, &Operand::Integer(-1)), None);
    assert_eq!(
        machine.timeout_delay(&module, &Operand::X(0)),
        None,
        "an unset x register is no delay"
    );
    assert_eq!(
        machine.timeout_delay(&module, &Operand::Y(0)),
        None,
        "no frame ⇒ no Y slot to read; a parked frame's slot IS read (see the y-delay tests)"
    );
}

// -- a runtime-variable (`Y`-register) `receive … after T` ----------------

/// A mailbox that never holds a message, so a `receive` suspends at its scan address.
struct EmptyMailbox;

impl Mailbox for EmptyMailbox {
    fn send_message(&mut self, _to: Eterm, _message: Eterm) -> beam_common::Result<()> {
        Ok(())
    }
    fn receive_next(&mut self) -> beam_common::Result<Option<Eterm>> {
        Ok(None)
    }
    fn receive_skip(&mut self) -> beam_common::Result<()> {
        Ok(())
    }
    fn receive_remove(&mut self) -> beam_common::Result<()> {
        Ok(())
    }
    fn receive_reset(&mut self) -> beam_common::Result<()> {
        Ok(())
    }
    fn receive_marker_reserve(&mut self) -> beam_common::Result<Eterm> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> beam_common::Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_use(&mut self, _reference: Eterm) -> beam_common::Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_clear(&mut self, _reference: Eterm) -> beam_common::Result<()> {
        Err(BeamError::BadArg)
    }
}

/// A one-function synthetic module for a `receive … after T` whose `T` is a **runtime
/// variable**, so the compiler kept it in a **Y** slot and the parked `wait_timeout`'s `Time`
/// operand is [`Operand::Y(0)`](Operand::Y) — never a literal the loader could fold.
///
/// This is the shape `erlc` emits for `recv(T) -> receive m -> ok after T -> timeout end.`
/// (the delay is a `{bif,max,…}`-style runtime value), with the `timeout` continuation at code
/// index 9:
///
/// ```text
/// 0  label 1
/// 1  allocate 1
/// 2  move <delay>, y0
/// 3  label 2
/// 4  loop_rec 3, x0
/// 5  remove_message
/// 6  return
/// 7  label 3
/// 8  wait_timeout 2, y0     ; <-- the parked instruction
/// 9  timeout                ; <-- the timed-out continuation (pc + 1)
/// 10 return
/// ```
fn y_delay_module(delay: Operand) -> Module {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: delay,
            dst: Operand::Y(0),
        },
        Instr::Label(Label(2)),
        Instr::LoopRec {
            fail: Label(3),
            dst: Operand::X(0),
        },
        Instr::RemoveMessage,
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::WaitTimeout {
            target: Label(2),
            time: Operand::Y(0),
        },
        Instr::Timeout,
        Instr::Return,
    ];
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    let mut module = Module::default();
    module.name = module.atom_table.intern("recv");
    let module_atom = module.name;
    let name = module.atom_table.intern("recv");
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module_atom,
        name,
        arity: 1,
        code,
        blocks: Vec::new(),
        labels,
    });
    module
}

/// Run `recv/1` of [`y_delay_module`] against an empty mailbox, returning the parked machine.
fn park_on_y_delay(module: &Module) -> (EngineOutcome, Machine, MinimalContext) {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_label_outcome(module, Label(1), 1, &[make_small(7)], &mut ctx)
        .expect("run recv/1");
    (outcome, machine, ctx)
}

#[test]
fn a_y_register_receive_delay_is_armed_and_fires() {
    // A `receive … after T` with `T` a runtime variable keeps the delay in a Y slot; the runner
    // reads that slot back through the public `Machine::y` accessor while the process is parked,
    // so the deadline is recovered rather than lost.
    let module = y_delay_module(Operand::Integer(20));
    let (outcome, mut machine, mut ctx) = park_on_y_delay(&module);
    // The non-zero delay suspends at the receive's scan label (`label 2` = code index 3).
    assert_eq!(outcome, EngineOutcome::Wait { func: 0, pc: 3 });

    // The Y slot of the parked frame IS readable — the deadline is a real 20 ms, not a guess.
    assert_eq!(machine.y(0), Some(make_small(20)));
    assert_eq!(machine.timeout_delay(&module, &Operand::Y(0)), Some(20));
    let deadline = machine
        .wait_deadline(&module)
        .expect("the y-register delay arms a 20 ms deadline");
    assert_eq!(
        deadline,
        WaitDeadline {
            delay_ms: Some(20),
            resume: (0, 9)
        },
        "resume at the `timeout` after wait_timeout"
    );
    assert_eq!(module.functions[0].code[9], Instr::Timeout);

    // ...and it fires: resuming at the recovered address runs the timed-out continuation.
    let fired = machine
        .resume_outcome(&module, deadline.resume.0, deadline.resume.1, &mut ctx)
        .expect("a fired deadline resumes at the timeout instruction");
    assert_eq!(fired, EngineOutcome::Returned(make_small(7)));

    // Bounds: the frame owns exactly one Y slot, and an out-of-range one is no deadline.
    assert_eq!(machine.y(1), None, "the frame has one Y slot");
    assert_eq!(machine.timeout_delay(&module, &Operand::Y(1)), None);
}

#[test]
fn a_y_register_delay_that_is_not_a_positive_integer_arms_nothing() {
    // `infinity` (like a zero/negative or non-integer value) in the Y slot is the honest
    // absence of a deadline: the process parks until a message arrives, never on a guessed one.
    let mut module = y_delay_module(Operand::Integer(20));
    let infinity = module.atom_table.intern("infinity");
    module.functions[0].code[2] = Instr::Move {
        src: Operand::Atom(infinity),
        dst: Operand::Y(0),
    };

    let (outcome, machine, _ctx) = park_on_y_delay(&module);
    // A non-integer delay still suspends (it is not the small integer 0), so the machine IS
    // parked — and the unreadable slot must therefore stay "no deadline", not become one: the
    // runner arms a timer only when `delay_ms` is `Some`.
    assert_eq!(outcome, EngineOutcome::Wait { func: 0, pc: 3 });
    assert_eq!(machine.y(0), Some(make_atom(infinity)));
    assert_eq!(machine.timeout_delay(&module, &Operand::Y(0)), None);
    let deadline = machine
        .wait_deadline(&module)
        .expect("a parked wait_timeout still has a `resume`");
    assert_eq!(
        deadline.delay_ms, None,
        "an unreadable delay arms no timer, so the process parks indefinitely"
    );
    assert_eq!(deadline.resume, (0, 9), "the timed-out continuation is still resolved");
}

// -- a cross-module park: the deadline belongs to the CALLEE ---------------

/// Map every [`Instr::Label`] in `code` to its index (the synthetic-module builders here
/// share the shape of the engine's own `Fixture`).
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// The caller module `m`: `run/0` allocates its own frame, `call_ext`s `c:recv_timeout/0`,
/// then deallocates and returns the callee's value.
///
/// It contains **no** `wait_timeout` at all, which is the crux of the cross-module case: a
/// deadline recovered by scanning this (entry) module would find nothing.
fn cross_caller_module() -> (Module, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let import_module = module.atom_table.intern("c");
    let import_function = module.atom_table.intern("recv_timeout");
    module.imports.push(Import {
        module: import_module,
        function: import_function,
        arity: 0,
    });
    let name = module.atom_table.intern("run");
    let module_atom = module.name;
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 0,
        },
        Instr::CallExt {
            target: ImportRef(0),
            arity: 0,
        },
        Instr::Deallocate(0),
        Instr::Return,
    ];
    let labels = labels_of(&code);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module_atom,
        name,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function: name,
        arity: 0,
        label: Label(1),
    });
    (module, name)
}

/// The callee module `c`: `recv_timeout/0` — `receive foo -> foo after 20 -> timed_out end`
/// whose timed-out branch moves a distinctive atom into `x(0)`. Its `wait_timeout` sits at
/// code index 5 and the `timeout` continuation at index 6.
fn cross_callee_module() -> Arc<Module> {
    let mut module = Module::default();
    module.name = module.atom_table.intern("c");
    let timed_out = module.atom_table.intern("timed_out");
    let name = module.atom_table.intern("recv_timeout");
    let module_atom = module.name;
    let code = vec![
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
    ];
    let labels = labels_of(&code);
    module.functions.push(Function {
        func_label: Label(10),
        entry_label: Label(10),
        module: module_atom,
        name,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function: name,
        arity: 0,
        label: Label(10),
    });
    Arc::new(module)
}

/// The whole point of the fix: a machine parked inside a **cross-module callee** has its
/// deadline recovered through the **entry** module the runner passes.
///
/// The defect this guards: the runner used to recover a parked receive's deadline by
/// scanning the process's **entry** module (`parked_receive_timeout(&machine, &entry_module,
/// func, pc)`). A `call_ext*` the host resolved through
/// [`ExecutionContext::resolve_external_call`](beam_engine::ExecutionContext::resolve_external_call)
/// is entered **in the calling machine**, so its `receive … after T` parks at a `wait_timeout`
/// in the **callee's** instruction stream — which the entry module does not contain. The
/// entry-module read therefore found nothing, armed no timer, and the process parked forever.
/// `Machine::wait_deadline` resolves the callee module internally, so passing the entry module
/// is correct and sufficient.
#[test]
fn a_cross_module_park_recovers_the_callee_deadline_through_the_entry_module() {
    let (caller, run) = cross_caller_module();
    let callee = cross_callee_module();

    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let resolved = Arc::clone(&callee);
    ctx.set_external_resolver(Box::new(move |module, function, arity| {
        if module != "c" {
            return Ok(None);
        }
        let Some(function) = resolved.atom_table.lookup(function) else {
            return Ok(None);
        };
        Ok(resolved
            .export(function, arity)
            .map(|label| (Arc::clone(&resolved), label)))
    }));

    let mut machine = Machine::new();
    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("run m:run/0");
    assert!(
        matches!(outcome, EngineOutcome::Wait { .. }),
        "the callee's receive parks the calling process, got {outcome:?}"
    );

    // The entry module has no `wait_timeout` at all — reading it alone (the pre-fix behaviour)
    // can only lose the deadline, which is exactly how the process used to park forever.
    assert!(
        !caller.functions[0]
            .code
            .iter()
            .any(|instr| matches!(instr, Instr::WaitTimeout { .. })),
        "the entry module holds no wait_timeout: the deadline lives in the callee"
    );

    // Yet the deadline IS recovered by passing the ENTRY module: the callee's `after 20`,
    // with the callee's own `timeout` continuation (func 0, code index 6).
    let deadline = machine
        .wait_deadline(&caller)
        .expect("the callee's finite delay is recovered through the entry module");
    assert_eq!(
        deadline,
        WaitDeadline {
            delay_ms: Some(20),
            resume: (0, 6)
        }
    );

    // Firing it runs the callee's `after` clause and returns its value through the caller.
    let resumed = machine
        .resume_outcome(&caller, deadline.resume.0, deadline.resume.1, &mut ctx)
        .expect("resume at the callee's `timeout` continuation");
    let timed_out = make_atom(
        callee
            .atom_table
            .lookup("timed_out")
            .expect("the callee's after clause names `timed_out`"),
    );
    assert_eq!(resumed, EngineOutcome::Returned(timed_out));
}

/// Compile each `(module-name, source)` into one fresh temp dir and return that dir; `None`
/// when no runnable `erlc` is on `PATH` (the caller skips loudly). A spawned `erlc` that
/// *ran* and exited non-zero is a harness failure, so it panics with the captured stderr.
fn compile_modules(tag: &str, sources: &[(&str, &str)]) -> Option<std::path::PathBuf> {
    let dir = std::env::temp_dir().join(format!("beam_rt_runner_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("the scratch directory is creatable");
    for (name, source) in sources {
        std::fs::write(dir.join(format!("{name}.erl")), source)
            .unwrap_or_else(|error| panic!("write {name}.erl: {error}"));
        let output = std::process::Command::new("erlc")
            .arg("+deterministic")
            .arg(format!("{name}.erl"))
            .current_dir(&dir)
            .output()
            .ok()?;
        assert!(
            output.status.success(),
            "the system `erlc` exited {} compiling {name}.erl\n  stderr: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Some(dir)
}

/// The end-to-end proof of the fixed runner arm: a **scheduled** process whose `call_ext`
/// parks inside a cross-module **callee**'s `receive … after 20` must have its timer armed.
///
/// The caller's `run/0` tail-calls `xm_callee:recv_timeout/0`, which the real
/// [`RuntimeContext`](crate::context::RuntimeContext) enters **in this machine**, so the
/// `wait_timeout` is in the callee's stream. Before the fix the runner scanned the entry
/// module (which has no `wait_timeout`), armed nothing and the process parked forever; the
/// observable here is the engine's `parked_timeout` handle.
#[test]
fn a_process_parked_in_a_cross_module_callee_arms_the_callee_timeout() {
    let callee = "\
-module(xm_callee).
-export([recv_timeout/0]).
recv_timeout() ->
    receive
        foo -> foo
    after 20 ->
        timed_out
    end.
";
    let caller = "\
-module(xm_caller).
-export([run/0]).
run() ->
    xm_callee:recv_timeout().
";
    let Some(dir) = compile_modules("xmod", &[("xm_callee", callee), ("xm_caller", caller)])
    else {
        eprintln!("SKIP: no system `erlc` on PATH -- the cross-module park test is skipped");
        return;
    };

    let mut rt = runtime();
    rt.add_code_path(&dir);
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(
        beam_loader::load_file(dir.join("xm_caller.beam")).expect("load the compiled caller"),
    );
    let run = module.atom_table.lookup("run").expect("run/0 is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function: run,
            arity: 0,
        },
        Vec::new(),
    );

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Wait,
        "the callee's `after T` parks the calling process"
    );
    assert!(
        state
            .engines
            .get(&pid)
            .is_some_and(|engine| engine.parked_timeout.is_some()),
        "the callee's deadline is armed through the entry module (the pre-fix arm lost it)"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **A running process's off-heap refc binaries are its engine's reservoir — nothing is
/// mirrored onto the PCB.**
///
/// `erlang:process_info(Pid, binary)` reports the payloads a process built. The one holder of
/// them is the machine's `Machine::refc_binaries` reservoir (never cleared by `Machine::reset`;
/// pruned at the slice boundary once the process's binary virtual heap triggers, see
/// `RuntimeRunner::reclaim_off_heap_binaries`), which the runner keeps where it is rather than
/// mirroring it: it reads them back through `RunnerState::reservoir_at`. The process's **own**
/// off-heap table is therefore handed out **no slot at all** — which is the whole point, because
/// `beam_gc::OffHeap`'s slot space is never compacted, so a slot per payload per slice made the
/// O(slots) `overhead_bytes`/`sweep` walks quadratic in the number of iterations a
/// build-a-binary loop had run.
///
/// This pins all three halves for a process parked at a `receive` **after** building a
/// runtime-sized (off-heap, above the 64-byte on-heap limit) binary: the reservoir is non-empty,
/// the process's own table is empty, and the real observable — `erlang:process_info(Pid, binary)`
/// — still lists the payload. The fixture is a real compile of `<<0:N/unit:8>>` with a
/// **variable** size, so the compiler lowers it to `bs_create_bin` (unit 8 → `N` **bytes**) →
/// `alloc_refc_binary`, which is exactly the production route. The reservoir assertion is
/// load-bearing: it fails loudly if the fixture ever stops building off-heap, so the roster
/// assertion can never be satisfied vacuously.
#[test]
fn a_process_building_an_off_heap_binary_comes_to_own_it() {
    let source = "\
-module(ohb).
-export([build_and_park/1]).
build_and_park(N) ->
    Bin = <<0:N/unit:8>>,
    receive X -> {Bin, X} end.
";
    let Some(dir) = compile_modules("offheap_bin", &[("ohb", source)]) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the off-heap binary ownership test is skipped");
        return;
    };

    let mut rt = runtime();
    rt.add_code_path(&dir);
    // Loaded through the runtime's code server (not `beam_loader::load_file`), so the child
    // shares the runtime's one atom table and both live in the runtime's own process table and
    // engine state — which is what makes the `erlang:process_info/2` read below the real path.
    rt.load_module(dir.join("ohb.beam"))
        .expect("the compiled module loads");
    let pid = rt
        .spawn_process("ohb", "build_and_park", vec![make_small(256)])
        .expect("spawn ohb:build_and_park/1");
    assert_eq!(
        rt.run_one_slice().expect("one slice"),
        1,
        "the child ran in exactly one slice"
    );
    assert_eq!(
        rt.processes()
            .get(pid)
            .expect("the parked child stays in the table")
            .state(),
        ProcessState::Waiting,
        "the bare `receive` parks the process after it builds the binary"
    );

    // The fixture really builds off-heap: the machine's never-cleared reservoir holds the payload.
    let reservoir = rt
        .runner_state()
        .reservoir_at(pid)
        .expect("the parked child has an engine");
    assert!(
        !reservoir.is_empty(),
        "the fixture must build its binary off-heap (a runtime-sized `<<0:N/unit:8>>`, N = 256)"
    );

    // ...and the process's own off-heap table was handed out NO slot: nothing is mirrored.
    let (slots, bytes) = {
        let process = rt.processes().get(pid).expect("the parked process");
        (
            process.off_heap_binary_count(),
            process.off_heap_binary_bytes(),
        )
    };
    assert_eq!(
        slots, 0,
        "no payload is registered on the PCB: the engine reservoir is the one holder"
    );
    assert_eq!(
        bytes, 0,
        "so the never-compacted off-heap slot space stays empty"
    );

    // The real observable: `erlang:process_info(Pid, binary)` still lists the payload it built.
    let info = crate::native::test_support::atom(&mut rt, "binary");
    let reply = crate::native::test_support::call(
        &mut rt,
        "erlang",
        "process_info",
        &[pid.to_eterm(), info],
    )
    .expect("erlang:process_info/2 binary");
    assert_eq!(terms::tuple_arity(reply).expect("a tuple"), 2);
    let roster = terms::tuple_element(reply, 1).expect("the item's value");
    let entries = terms::list_elements(roster).expect("the roster is a list");
    assert!(
        !entries.is_empty(),
        "a process holding an off-heap refc binary must not report `[]`"
    );
    let sizes: Vec<i64> = entries
        .iter()
        .map(|entry| signed_value(terms::tuple_element(*entry, 1).expect("the size")))
        .collect();
    assert!(
        sizes.contains(&256),
        "the roster carries the 256-byte payload the fixture built: {sizes:?}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **A process never hands out an off-heap slot, no matter how many slices it runs.**
///
/// The deterministic mechanism pin for the quadratic `foreign`/`enc` regression. The removed
/// seam mirrored the machine's append-only `Machine::refc_binaries` reservoir onto the
/// process's own off-heap table once per slice; `beam_gc::OffHeap` never compacts, so the slot
/// vector only ever grew and every `Process::grow_nursery` / `ensure_capacity` /
/// `sweep_off_heap_binaries` then walked all of it — O(reservoir) work per call, i.e. quadratic
/// in the loop count.
///
/// The pin drives a tail loop that builds runtime-sized (off-heap) binaries across several
/// slices and asserts, **after every slice**:
///
///   * the process's own off-heap table is still empty — **no** slot was ever handed out
///     (`off_heap_binary_count() == 0`), so no O(slots) scan can exist, and
///     `off_heap_binary_bytes() == 0` measures that same (never-compacted) slot space;
///   * the engine reservoir grew by a **whole number of iterations**, with an iteration always
///     building the same number of payloads: the per-iteration count `K` is measured from the
///     first slice that grew and every later slice's growth is checked against it. `K = 2` here
///     (the fixture builds two binaries per iteration), so the divisibility has teeth.
///
/// The pin cannot pass vacuously:
///   * the loop is long and the reduction budget tiny (`JitMode::Interpreter` charges **every**
///     instruction), so the process really pre-empts and every slice is asserted to pre-empt;
///   * the fixture builds its binaries from a **variable** size (`<<0:Size/unit:8>>`,
///     `Size = 256`, passed as an argument), so the compiler cannot fold them to literals and
///     every iteration really reaches `alloc_refc_binary`; the reservoir is asserted to hold
///     **≥ 2** payloads and to grow **across** slices, so an inert fixture (no off-heap build)
///     fails rather than passing trivially.
#[test]
fn a_process_never_hands_out_off_heap_slots_across_slices() {
    let source = "\
-module(ohb_loop).
-export([loop/2]).
loop(N, Size) -> loop(N, Size, 0).
loop(0, _Size, Acc) -> Acc;
loop(N, Size, Acc) ->
    A = <<0:Size/unit:8>>,
    B = <<1:Size/unit:8>>,
    loop(N - 1, Size, Acc + byte_size(A) + byte_size(B)).
";
    let Some(dir) = compile_modules("offheap_cursor", &[("ohb_loop", source)]) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the off-heap slot-space pin is skipped");
        return;
    };

    let mut rt = runtime();
    rt.add_code_path(&dir);
    let mut state = RunnerState::default();
    // The interpreter tier charges **every** instruction, so the tiny budget below pre-empts
    // deterministically and the loop genuinely spans several slices.
    state.jit_mode = Some(JitMode::Interpreter);
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(
        beam_loader::load_file(dir.join("ohb_loop.beam")).expect("load the compiled module"),
    );
    let loop_fun = module.atom_table.lookup("loop").expect("loop/2 is interned");
    // 50 iterations of two 256-byte (off-heap) binaries each: far more than the slices driven
    // below can finish, so the process is still pre-empting when the test stops.
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function: loop_fun,
            arity: 2,
        },
        vec![make_small(50), make_small(256)],
    );

    // Drive small slices; each must pre-empt (the loop is far from done).
    const SLICES: usize = 50;
    const SLICE_BUDGET: u32 = 8;
    let mut reservoir = 0;
    let mut first_slice_reservoir = 0;
    // The payloads one iteration builds, measured from the first slice that grew. `None` until
    // then: a slice can end mid-iteration, so a zero delta is legitimate.
    let mut per_iteration: Option<usize> = None;
    for slice in 0..SLICES {
        let outcome = {
            let mut runner = RuntimeRunner::new(&mut rt, &mut state);
            runner
                .run(pid, &mut table, SLICE_BUDGET)
                .expect("a small slice runs")
        };
        assert_eq!(
            outcome,
            ProcOutcome::Yield,
            "the tiny budget pre-empts the loop long before its 50 iterations finish"
        );
        let (slots, bytes, length) = {
            let engine = state
                .engines
                .get(&pid)
                .expect("the pre-empted engine is parked");
            let process = table.get(pid).expect("the pre-empted process");
            (
                process.off_heap_binary_count(),
                process.off_heap_binary_bytes(),
                engine.machine.refc_binaries().len(),
            )
        };
        // (a) The process's own off-heap table was handed out no slot, in this slice or any
        // earlier one — the never-compacted slot space stays empty, so no O(slots) scan exists.
        assert_eq!(
            slots, 0,
            "slice {slice}: no off-heap slot is ever handed out (the engine reservoir holds them)"
        );
        assert_eq!(
            bytes, 0,
            "slice {slice}: the never-compacted off-heap slot space stays empty"
        );
        // (b) This fixture stays below the bin-vheap trigger (its ~25 KB of payloads against the
        // 46 422-word budget), so no slice-boundary prune runs and the reservoir grows by whole
        // iterations, each building `K` payloads.
        let delta = length - reservoir;
        match per_iteration {
            Some(k) => assert_eq!(
                delta % k,
                0,
                "slice {slice}: {delta} new payloads is not a whole number of {k}-payload \
                 iterations"
            ),
            None => {
                if delta > 0 {
                    per_iteration = Some(delta);
                }
            }
        }
        if slice == 0 {
            first_slice_reservoir = length;
        }
        reservoir = length;
    }

    let per_iteration = per_iteration.expect(
        "the fixture must build off-heap payloads in at least one slice (two runtime-sized \
         `<<N:Size/unit:8>>` constructions per iteration)",
    );
    assert_eq!(
        per_iteration, 2,
        "an iteration builds exactly two off-heap payloads (the fixture's two constructions)"
    );
    assert_eq!(
        reservoir % per_iteration,
        0,
        "{reservoir} payloads is not a whole number of {per_iteration}-payload iterations"
    );

    // Non-vacuity: the fixture really built off-heap payloads, and built MORE of them by the end
    // than it held after the first slice (so the reservoir grew across slices).
    assert!(
        reservoir >= 2,
        "the fixture must build multiple off-heap binaries across the slices (got {reservoir})"
    );
    assert!(
        reservoir > first_slice_reservoir,
        "the reservoir must GROW across slices (first slice {first_slice_reservoir}, final \
         {reservoir})"
    );
    assert!(
        reservoir / per_iteration >= 2,
        "the slices really completed several iterations (got {} of them)",
        reservoir / per_iteration
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **The A.9 E0 shape, end to end.** A **scheduled** process calls a **float-using** module
/// across a module boundary; that callee blocks in a `receive`, so the process must suspend and
/// be resumed by the message — and its return value is the float the callee computed.
///
/// Why this is the regression guard for the removed float guard in
/// [`RuntimeContext::resolve_external_call`](crate::context::RuntimeContext): while the seam
/// refused a float-using callee (`compile.beam` lowers `fconv`, which is what blocked the E0
/// route), the `call_ext` fell to the synchronous `call_bif` → `run_export_blocking` path. That
/// path builds a **nested** machine on the same context, and a nested machine's admission point
/// is never armed (`may_pump_scheduler() == false` for a scheduled slice), so the callee's
/// blocking `receive` could not suspend: the first slice would end in an exit carrying the
/// typed unsupported `blocked()` gap instead of `ProcOutcome::Wait`. The seam now enters the
/// callee in the calling machine, and the full GC door publishes that machine's FR file up to its
/// pointer-bearing prefix, so the float registers it leaves live are relocated rather than
/// dangled (see
/// `crate::context::gc`).
#[test]
fn a_float_using_callee_suspends_and_resumes_a_scheduled_process() {
    let callee = "\
-module(xmf_callee).
-export([await/1]).
await(Self) ->
    receive N -> Self ! {result, N + float(N)} end.
";
    let caller = "\
-module(xmf_caller).
-export([run/0]).
run() ->
    Self = self(),
    xmf_callee:await(Self),
    receive {result, R} -> R end.
";
    let Some(dir) = compile_modules(
        "xmod_float",
        &[("xmf_callee", callee), ("xmf_caller", caller)],
    ) else {
        eprintln!(
            "SKIP: no system `erlc` on PATH -- the float cross-module suspend test is skipped"
        );
        return;
    };

    let mut rt = runtime();
    rt.add_code_path(&dir);
    // Both modules are loaded through the runtime's code server, so they share its one atom
    // table — the tuple the callee sends and the tuple the caller matches must agree on the
    // `result` atom's index. (The entry module is loaded here rather than with
    // `beam_loader::load_file`, whose private atom table would not be the shared one.)
    rt.load_module(dir.join("xmf_callee.beam"))
        .expect("the compiled callee loads");
    rt.load_module(dir.join("xmf_caller.beam"))
        .expect("the compiled caller loads");
    let callee_module = rt
        .code_server()
        .module_handle("xmf_callee")
        .expect("the callee is cached");
    // The premise: the compiled callee really does lower float-family instructions
    // (`N + float(N)` lowers to `gc_bif float`/`fconv`/`fmove`/`fadd`), so the case exercises
    // exactly the guard the seam no longer applies.
    assert!(
        crate::context::gc::module_uses_floats(&callee_module),
        "the callee's source lowers a float-family instruction"
    );

    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = rt
        .code_server()
        .module_handle("xmf_caller")
        .expect("the caller is cached");
    let run = module.atom_table.lookup("run").expect("run/0 is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function: run,
            arity: 0,
        },
        Vec::new(),
    );

    // Slice 1: the caller enters the float-using callee in-machine and parks in its `receive`.
    // (Before the guard was removed this was an exit carrying `unsupported: … blocked()`.)
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Wait,
        "the float-using callee's blocking `receive` suspends the calling process"
    );

    // Slice 2: the value the callee awaits arrives; the process resumes inside the callee,
    // computes `3 + float(3)`, sends itself the result and returns it.
    table
        .deliver_simple(pid, pid.to_eterm(), make_small(3))
        .expect("the awaited value is delivered");
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    let value = beam_engine::terms::float_value(state.result(pid).expect("the result is recorded"))
        .expect("the process returns the float the callee computed");
    assert!(
        (value - 6.0).abs() < 1e-12,
        "3 + float(3) is 6.0, got {value}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
// -- execution-tier selection (`super::tier`) -----------------------------

/// The committed engine fixtures reused by relative path: `slice.beam` (the vertical slice),
/// `stdlib_more_harness.beam` (whose code reaches other loadable `.beam`s — `orddict` and
/// `queue`) and `funs.beam` (dynamic `apply`/`apply_last`/`call_fun`).
fn tier_fixtures() -> [&'static str; 3] {
    [
        slice_fixture(),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_engine/tests/fixtures/stdlib_more_harness.beam"
        ),
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_engine/tests/fixtures/funs.beam"
        ),
    ]
}

/// The **default** (`JitMode::Auto`) selects the native baseline tier for a scheduled export
/// entry, whatever the entry module's code looks like.
///
/// `stdlib_more_harness` reaches other loadable `.beam`s and `funs` uses dynamic `apply` — the
/// two shapes a retired per-module predicate used to refuse. The engine's baseline driver
/// handles the module-context switch (`Step::Enter`) itself (`beam_engine`'s `switch_into`), so
/// there is nothing left to refuse and the tier is chosen **without inspecting the module**.
#[test]
fn the_default_selects_the_baseline_tier_for_any_export_entry() {
    let rt = runtime();
    let mut state = RunnerState::default();
    for fixture in tier_fixtures() {
        let module = Arc::new(beam_loader::load_file(fixture).expect("the fixture loads"));
        let entry = ProcessEntry::Export {
            module,
            function: Atom::from_raw(0),
            arity: 0,
        };
        assert_eq!(
            select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
            Tier::BaselineJit,
            "{fixture} is tiered natively by default"
        );
    }
}

/// `JitMode::Cranelift` selects the optional Cranelift tier for a scheduled export entry —
/// for **every** fixture shape, since the mode alone decides and nothing about the module is
/// inspected.
///
/// The answer is unconditional: `select_entry_tier` is not the feature gate (that lives once, in
/// `beam_engine`), so this holds in a build without the backend too — and it is exactly why a
/// `cranelift` request in such a build is a typed capability gap rather than a downgrade.
#[test]
fn the_cranelift_mode_selects_the_cranelift_tier_for_any_export_entry() {
    let rt = runtime();
    let mut state = RunnerState::default();
    for fixture in tier_fixtures() {
        let module = Arc::new(beam_loader::load_file(fixture).expect("the fixture loads"));
        let entry = ProcessEntry::Export {
            module,
            function: Atom::from_raw(0),
            arity: 0,
        };
        assert_eq!(
            select_entry_tier(&rt, &mut state, &entry, JitMode::Cranelift),
            Tier::Cranelift,
            "{fixture} is tiered on Cranelift when that mode is named"
        );
    }
}

/// `JitMode::Interpreter` forces the interpreter; the default (`JitMode::Auto`) and the
/// explicit `JitMode::Baseline` alias select the native tier for the **same** entry, so the
/// kill switch is the only thing that turns the tier off.
#[test]
fn the_mode_forces_the_tier() {
    let rt = runtime();
    let harness = tier_fixtures()[1];
    let module = Arc::new(beam_loader::load_file(harness).expect("harness"));
    let mut state = RunnerState::default();
    let entry = ProcessEntry::Export {
        module: Arc::clone(&module),
        function: Atom::from_raw(0),
        arity: 0,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Baseline),
        Tier::BaselineJit
    );
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::BaselineJit,
        "Auto and Baseline name the same tier"
    );
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Interpreter),
        Tier::Interpreter
    );
}

/// An unresolved fun entry (its module is only known once the context resolves the closure)
/// takes the **same** tier as an export entry: with no per-module predicate left there is
/// nothing a missing module name could make unsafe, and the engine's driver handles the
/// in-machine entry.
#[test]
fn an_unresolved_fun_entry_takes_the_default_tier() {
    let rt = runtime();
    let mut state = RunnerState::default();
    let entry = ProcessEntry::Fun {
        fun: make_small(0),
        target: None,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::BaselineJit,
        "a fun entry is tiered like an export entry"
    );
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Interpreter),
        Tier::Interpreter
    );
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Cranelift),
        Tier::Cranelift,
        "a fun entry takes the Cranelift tier when that mode is named"
    );
}
// -- the process-wide default mode ----------------------------------------

/// The effective mode for an **unset** `BEAM_RT_TIER` is `Auto`, which selects the native
/// baseline tier for every process entry. `"baseline"` names that same default explicitly,
/// `"cranelift"` (and the engine's own `"jit-cranelift"` spelling of it) names the optional hot
/// tier, and any unknown value falls back to the always-correct interpreter. Pinned through the
/// **pure** resolution helper ([`mode_from_value`]) rather than by mutating the process
/// environment (the crate is `#![deny(unsafe_code)]`), and the derived [`JitMode::default`]
/// must agree with it so no call site can pick a different tier by omission.
#[test]
fn the_unset_default_resolves_to_auto() {
    assert_eq!(
        mode_from_value(None),
        JitMode::Auto,
        "an unset BEAM_RT_TIER is the native-tier default"
    );
    assert_eq!(mode_from_value(Some("interpreter")), JitMode::Interpreter);
    assert_eq!(
        mode_from_value(Some("auto")),
        JitMode::Auto,
        "`auto` names the default explicitly"
    );
    assert_eq!(mode_from_value(Some("baseline")), JitMode::Baseline);
    // The optional hot tier, named either by the runtime's own spelling or by `Tier::name()`'s —
    // the resolution never consults the Cargo feature, so both spellings work in every build
    // (whether the backend is compiled in is the engine's answer, at the entry point).
    assert_eq!(mode_from_value(Some("cranelift")), JitMode::Cranelift);
    assert_eq!(
        mode_from_value(Some("jit-cranelift")),
        JitMode::Cranelift,
        "`jit-cranelift` is `Tier::name()`'s spelling of the same request"
    );
    // An unknown value is the always-correct interpreter, never an error and never a JIT —
    // including `"hot"`, which names no tier and must not quietly mean the Cranelift one.
    for unknown in ["jit", "", "hot", "cranelift-jit", "CRANELIFT"] {
        assert_eq!(
            mode_from_value(Some(unknown)),
            JitMode::Interpreter,
            "{unknown:?} names no tier, so a typo must stay the always-correct interpreter"
        );
    }
    // The derived `Default` is the same tier an unset variable resolves to.
    assert_eq!(JitMode::default(), JitMode::Auto);
}

// -- end-to-end tier equivalence on the scheduled path --------------------

/// `slice:fib/1 10` through the **scheduled** runner records `55` on *both* tiers: the
/// auto/JIT path and the forced interpreter. The JIT is oracle-identical to the
/// interpreter — the deliverable of adopting it is correctness, and this pins the recorded
/// value (55) for each.
#[test]
fn fib_runs_to_the_same_result_on_both_tiers() {
    const EXPECTED: i64 = 55;
    for mode in [JitMode::Auto, JitMode::Interpreter] {
        let mut rt = runtime();
        let mut state = RunnerState::default();
        state.jit_mode = Some(mode);
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
        let function = module.atom_table.lookup("fib").expect("fib is interned");
        state.spawn(
            pid,
            ProcessEntry::Export {
                module,
                function,
                arity: 1,
            },
            vec![make_small(10)],
        );

        let outcome = {
            let mut runner = RuntimeRunner::new(&mut rt, &mut state);
            runner
                .run(pid, &mut table, 1_000)
                .unwrap_or_else(|error| panic!("the {mode:?} slice runs: {error}"))
        };
        assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)), "{mode:?}");
        let result = state.result(pid).expect("the value is recorded");
        // `make_small(EXPECTED)` and `result` are compared structurally: both are immediate
        // small ints, so the raw word comparison is exact.
        assert_eq!(
            signed_value(result),
            EXPECTED,
            "{mode:?} tier computes fib(10) = {EXPECTED}"
        );
    }
}

/// **Feature off: naming the Cranelift tier is a typed capability gap, never a silent
/// substitution.**
///
/// `slice:fib/1 10` through the scheduled runner with `JitMode::Cranelift` records **no**
/// result and terminates the process abnormally: the runtime selects `Tier::Cranelift`
/// unconditionally ([`select_entry_tier`] is not the feature gate) and the engine's own
/// feature-gated entry point answers `unsupported: opcode: jit-cranelift tier not built` — the
/// typed signal [`crate::error::unsupported_reason`] is the single interpreter of (the CLI's exit
/// code 3 whenever that raise is the error the action reports).
#[cfg(not(feature = "cranelift"))]
#[test]
fn the_cranelift_tier_without_its_feature_is_the_typed_capability_gap() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    state.jit_mode = Some(JitMode::Cranelift);
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
    let function = module.atom_table.lookup("fib").expect("fib is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 1,
        },
        vec![make_small(10)],
    );

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner.run(pid, &mut table, 1_000).expect("the slice runs")
    };
    assert!(
        matches!(outcome, ProcOutcome::Exit(_)),
        "a process on a tier with no backend terminates: {outcome:?}"
    );
    assert_ne!(
        outcome,
        ProcOutcome::Exit(normal(&mut rt)),
        "the exit is abnormal — no other tier ran the code to a normal return"
    );
    assert!(
        state.result(pid).is_none(),
        "no value is recorded: nothing ran, so nothing was silently substituted"
    );
    let error = state
        .take_error(pid)
        .expect("the failure is recorded as the process's typed error");
    let reason = crate::error::unsupported_reason(&error)
        .expect("the refusal is a capability gap, not a runtime error");
    assert!(
        reason.contains("cranelift"),
        "the reason names the missing tier: {reason}"
    );
}

/// **The startup capability check refuses exactly the unbuildable request, and nothing else.**
///
/// [`tier_unavailable`] is the CLI-facing half of *"the tier must be built to be selected"*: it
/// answers the typed capability gap for a `JitMode::Cranelift` request in a build **without** the
/// `cranelift` Cargo feature, and `None` for every mode the build can serve. That second half is
/// asserted in **both** builds, because it is the property that keeps the check `src/cli.rs` runs
/// before any boot work from ever refusing an invocation that would have run: no served mode may
/// be refused, in any build.
///
/// The gap must be a **capability gap** ([`crate::error::unsupported_reason`] classifies it, which
/// is what makes the CLI exit 3) and its text must name the Cargo feature the caller has to
/// rebuild with — the whole point of deciding it at the CLI boundary instead of letting the
/// per-process failure be re-wrapped into an exit-1 diagnostic that names nothing.
#[test]
fn tier_unavailable_refuses_only_the_unbuilt_cranelift_tier() {
    for mode in [JitMode::Auto, JitMode::Baseline, JitMode::Interpreter] {
        assert!(
            tier_unavailable(mode).is_none(),
            "{mode:?} is served by every build, so the startup check must never refuse it"
        );
    }

    #[cfg(not(feature = "cranelift"))]
    {
        let error = tier_unavailable(JitMode::Cranelift)
            .expect("a build without the `cranelift` feature has no tier to run Cranelift on");
        let reason = crate::error::unsupported_reason(&error).expect(
            "the refusal is a capability gap (exit 3), not a genuine runtime error (exit 1)",
        );
        assert!(
            reason.contains("cranelift"),
            "the reason names the tier that is missing: {reason}"
        );
        assert!(
            reason.contains("Cargo feature"),
            "the reason names what to rebuild with: {reason}"
        );
        assert!(
            reason.contains("BEAM_RT_TIER"),
            "the reason names the setting that triggered it: {reason}"
        );
    }

    #[cfg(feature = "cranelift")]
    {
        assert!(
            tier_unavailable(JitMode::Cranelift).is_none(),
            "this build compiled the Cranelift tier in, so the startup check must not fire"
        );
        assert!(
            Tier::Cranelift.is_available(),
            "the runtime's `cranelift` feature compiles the engine's backend in"
        );
    }
}

/// **Feature on: `slice:fib/1 10` records the same `55` on all three tiers.**
///
/// The three runs happen in one test so the row is a real three-tier agreement rather than three
/// independent claims, and the value each tier records is compared against the interpreter's.
/// The seam's answer for the entry each process is spawned with is pinned too, so the tier is
/// really *selected* per mode (a silently substituted tier would still compute `55`).
///
/// The remembered `ProcessEngine::tier` is not the observable here: it is only readable while a
/// pid is still parked (the pre-emption and `receive` tests above read it there), and this process
/// returns inside its first slice, so its engine leaves with it. The selection is pinned through
/// [`select_entry_tier`] — the single seam the runner consults at a first entry — and, from the
/// other side, by the feature-off sibling test above, where the *same* selection is what makes
/// the engine's Cranelift entry point the one that answers.
///
/// The slice budget is deliberately far larger than `fib(10)` can charge (a default slice is
/// `DEFAULT_REDUCTIONS_PER_SLICE` = 4 000 units): the subject here is the **first entry** on each
/// tier, so the run is kept inside one slice. That a tiered process **survives a suspension** is
/// the neighbouring resume rows' subject — `a_preempted_process_resumes_on_its_cranelift_tier` and
/// `a_receive_parked_process_resumes_on_its_cranelift_tier` cover the hot tier exactly as the
/// baseline and interpreter rows cover theirs.
#[cfg(feature = "cranelift")]
#[test]
fn fib_runs_to_the_same_result_on_all_three_tiers() {
    const EXPECTED: i64 = 55;
    assert!(
        Tier::Cranelift.is_available(),
        "the runtime's `cranelift` feature compiles the engine's backend in"
    );

    let mut recorded = Vec::new();
    for (mode, expected_tier) in [
        (JitMode::Interpreter, Tier::Interpreter),
        (JitMode::Auto, Tier::BaselineJit),
        (JitMode::Cranelift, Tier::Cranelift),
    ] {
        let mut rt = runtime();
        let mut state = RunnerState::default();
        state.jit_mode = Some(mode);
        let mut table = table();
        let pid = table.spawn_default().expect("a fresh process");
        let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
        let function = module.atom_table.lookup("fib").expect("fib is interned");
        let entry = ProcessEntry::Export {
            module,
            function,
            arity: 1,
        };
        // The tier the runner picks at this process's first entry (and remembers on its engine
        // for a later resume): `select_entry_tier` is that one decision seam.
        assert_eq!(
            select_entry_tier(&rt, &mut state, &entry, mode),
            expected_tier,
            "{mode:?} selects its own tier"
        );
        state.spawn(pid, entry, vec![make_small(10)]);

        let outcome = {
            let mut runner = RuntimeRunner::new(&mut rt, &mut state);
            runner
                .run(pid, &mut table, 1_000_000)
                .unwrap_or_else(|error| panic!("the {mode:?} slice runs: {error}"))
        };
        assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)), "{mode:?}");
        let result = state.result(pid).expect("the value is recorded");
        assert_eq!(
            signed_value(result),
            EXPECTED,
            "{expected_tier:?} computes fib(10) = {EXPECTED}"
        );
        recorded.push(signed_value(result));
    }
    assert_eq!(
        recorded,
        vec![EXPECTED; 3],
        "the interpreter, baseline and Cranelift tiers agree"
    );
}

/// The whole runtime's **scheduler** path (not just one slice) runs `slice:fib/1 10` and
/// records `55` — the tier selection is wired into the real `RuntimeRunner` the scheduler
/// drives, on whatever mode the process environment selects (an unset `BEAM_RT_TIER` ⇒ the
/// baseline tier for `slice`). The test asserts the *result*, so it holds on every tier.
#[test]
fn the_scheduler_path_computes_fib_under_the_selected_tier() {
    let mut rt = runtime();
    rt.load_module(slice_fixture()).expect("slice.beam loads");
    let pid = rt
        .spawn_process("slice", "fib", vec![make_small(10)])
        .expect("spawn slice:fib/1");
    let slices = rt.run_scheduler().expect("the pass runs");
    assert!(slices > 0, "at least one slice ran");
    assert_eq!(
        signed_value(rt.process_result(pid).expect("the process returned")),
        55
    );
}

// -- tier persistence across a resume (`resume_outcome_with_tier`) ---------

/// The committed **allocating** tail-loop fixture, reused by relative path (never copied).
///
/// `alloc_loop/1` builds one cons cell per iteration, so the compiler emits a `test_heap` — a
/// heap-admission point, the charge a native baseline block reaches — and a small budget
/// therefore **pre-empts** it part-way through on either tier (see the fixture's own docs).
fn preempt_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/preempt.beam"
    )
}

/// `alloc_loop/1`'s result length: it returns `[N, N-1, …, 1]`.
const PREEMPT_N: i64 = 50;

/// Spawns `preempt:alloc_loop/1` as a process on `mode` and registers its engine.
fn spawn_preempting_loop(
    state: &mut RunnerState,
    pid: ProcessId,
    mode: JitMode,
) -> Arc<Module> {
    state.jit_mode = Some(mode);
    let module = Arc::new(beam_loader::load_file(preempt_fixture()).expect("preempt.beam"));
    let function = module
        .atom_table
        .lookup("alloc_loop")
        .expect("alloc_loop is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module: Arc::clone(&module),
            function,
            arity: 1,
        },
        vec![make_small(PREEMPT_N)],
    );
    module
}

/// A process selected for the **baseline** tier that is pre-empted on the reduction budget
/// resumes on that **same** tier.
///
/// The tier actually taken is proven through the per-thread compiled-block cache the baseline
/// backend runs over: the first entry compiles `preempt` into it, and the **resume** brings
/// the (emptied) entry back. A resume that fell back to the interpreter — the pre-tier-aware
/// behaviour — compiles nothing, so the post-resume assertion is the discriminating one.
#[test]
fn a_preempted_process_resumes_on_its_baseline_tier() {
    // The cache is per thread; start from a known state so the counts below are exact.
    clear_baseline_cache();
    assert_eq!(baseline_cache_len(), 0, "the per-thread cache starts empty");

    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    spawn_preempting_loop(&mut state, pid, JitMode::Baseline);

    // The first slice: the allocating loop spends its (tiny) budget and is pre-empted.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 2)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Yield, "the budget pre-empts the loop");
    assert_eq!(
        state.engines.get(&pid).map(|engine| engine.tier),
        Some(Tier::BaselineJit),
        "the tier chosen at the first entry is remembered on the engine"
    );
    assert!(
        state
            .engines
            .get(&pid)
            .is_some_and(|engine| engine.parked.is_some()),
        "the pre-empted machine is parked at the exhausting instruction"
    );
    assert_eq!(
        baseline_cache_len(),
        1,
        "the first entry ran on the baseline tier"
    );

    // Forget the compiled artifact: after this, only the resume's own tier lookup can bring
    // the entry back.
    clear_baseline_cache();
    assert_eq!(baseline_cache_len(), 0);

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    let elements = beam_engine::terms::list_elements(state.result(pid).expect("recorded"))
        .expect("alloc_loop returns a proper list");
    assert_eq!(
        elements.len(),
        PREEMPT_N as usize,
        "the two slices together ran the loop to completion"
    );
    assert_eq!(
        baseline_cache_len(),
        1,
        "the resume ran on the remembered baseline tier (the interpreter compiles nothing)"
    );
}

/// A process parked in a `receive` and woken by a message resumes on its remembered tier.
///
/// The discriminating observation is again the compiled-block cache: the parked slice ran on
/// the baseline tier (it compiled the module), the cache is emptied while the process waits,
/// and the **resume** puts the entry back — which only the baseline tier's resume path does.
#[test]
fn a_receive_parked_process_resumes_on_its_baseline_tier() {
    clear_baseline_cache();
    assert_eq!(baseline_cache_len(), 0, "the per-thread cache starts empty");

    let mut rt = runtime();
    let mut state = RunnerState::default();
    state.jit_mode = Some(JitMode::Baseline);
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );

    // The first slice: `receive X -> X after infinity` parks on the empty mailbox.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks the receive");
    assert_eq!(
        state.engines.get(&pid).map(|engine| engine.tier),
        Some(Tier::BaselineJit),
        "the tier chosen at the first entry is remembered on the engine"
    );
    assert_eq!(
        baseline_cache_len(),
        1,
        "the first entry ran on the baseline tier"
    );

    clear_baseline_cache();
    assert_eq!(baseline_cache_len(), 0);

    // Wake the receiver: the message is copied into its own mailbox queue (the very queue the
    // runner's receive seam scans). A delivery-driven collection may relocate its heap while
    // it waits, which is why the parked machine published its roots.
    table
        .deliver_simple(pid, pid.to_eterm(), make_small(42))
        .expect("the message is delivered");

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    assert_eq!(
        signed_value(state.result(pid).expect("the received value is recorded")),
        42,
        "the resumed receive took the delivered message"
    );
    assert_eq!(
        baseline_cache_len(),
        1,
        "the resume ran on the remembered baseline tier (the interpreter compiles nothing)"
    );
}

/// An **interpreter**-tier process is unchanged: it still pre-empts and resumes, records the
/// same result, and never compiles native code on either slice.
#[test]
fn an_interpreter_tier_process_is_unchanged_across_a_resume() {
    clear_baseline_cache();

    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    spawn_preempting_loop(&mut state, pid, JitMode::Interpreter);

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 2)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Yield, "the budget pre-empts the loop");
    assert_eq!(
        state.engines.get(&pid).map(|engine| engine.tier),
        Some(Tier::Interpreter),
        "the interpreter tier is remembered just the same"
    );

    clear_baseline_cache();
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    let elements = beam_engine::terms::list_elements(state.result(pid).expect("recorded"))
        .expect("alloc_loop returns a proper list");
    assert_eq!(
        elements.len(),
        PREEMPT_N as usize,
        "the interpreter tier computes the same result across the resume"
    );
    assert_eq!(
        baseline_cache_len(),
        0,
        "an interpreter-tier process compiles no native code on either slice"
    );
}

/// `alloc_loop`'s recorded result as element **values**.
///
/// Two runs of one fixture live on two different `Runtime` stores, so their words can never be
/// compared directly; this fixture's elements are small ints, for which [`signed_value`] is an
/// exact (not lossy) reading.
#[cfg(feature = "cranelift")]
fn preempt_list_values(result: Eterm) -> Vec<i64> {
    let elements =
        beam_engine::terms::list_elements(result).expect("alloc_loop returns a proper list");
    elements.into_iter().map(signed_value).collect()
}

/// The Cranelift rows' reference: `preempt:alloc_loop/1` run to completion on the **interpreter**
/// tier under the same budget schedule those rows use (`2`, `2`, then `1_000_000`), returned as
/// the recorded result's element **values**.
///
/// The baseline siblings compare their result against a *length*, because their discriminating
/// observable is the compiled-block cache. This tier's artifact store is `pub(super)` inside
/// `beam_engine`, so the value itself carries the evidence — and it must be the reference
/// interpreter's, not merely a list of the right length.
#[cfg(feature = "cranelift")]
fn preempt_interpreter_reference() -> Vec<i64> {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh control process");
    spawn_preempting_loop(&mut state, pid, JitMode::Interpreter);
    for (budget, completing) in [(2, false), (2, false), (1_000_000, true)] {
        let outcome = {
            let mut runner = RuntimeRunner::new(&mut rt, &mut state);
            runner
                .run(pid, &mut table, budget)
                .expect("the control slice runs")
        };
        if completing {
            assert_eq!(
                outcome,
                ProcOutcome::Exit(normal(&mut rt)),
                "the control's completing slice returns normally"
            );
        } else {
            assert_eq!(outcome, ProcOutcome::Yield, "the control pre-empts too");
        }
    }
    assert!(
        state.take_error(pid).is_none(),
        "the interpreter control runs the fixture with no error"
    );
    preempt_list_values(
        state
            .result(pid)
            .expect("the control's value is recorded"),
    )
}

/// A process selected for the **Cranelift** tier that is pre-empted on the reduction budget
/// resumes on that **same** tier, and the two slices together compute the loop's whole result.
///
/// The baseline siblings discriminate their tier through the per-thread compiled-block cache
/// ([`baseline_cache_len`]); this tier's artifact store keeps its observables `#[cfg(test)]
/// pub(super)` inside `beam_engine`, so they are unreachable from here. The Cranelift evidence is
/// threefold instead: the tier remembered on the engine across the park ([`ProcessEngine::tier`],
/// read on both parks), the **absence of the typed resume gap** — a resume that fell back to the
/// old refusal records no result and leaves [`RunnerState::take_error`] set — and the recorded
/// value agreeing element for element with an **interpreter-tier control** run of the same
/// fixture, on the same budget schedule, in this same test.
///
/// The schedule hands the process a tiny budget **twice** before the completing slice, so the
/// tier is observed across a re-park as well as across the first park: a resume that silently
/// substituted another tier would show up on the second park's `engine.tier`.
#[cfg(feature = "cranelift")]
#[test]
fn a_preempted_process_resumes_on_its_cranelift_tier() {
    assert!(
        Tier::Cranelift.is_available(),
        "the runtime's `cranelift` feature compiles the engine's backend in"
    );

    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    spawn_preempting_loop(&mut state, pid, JitMode::Cranelift);

    // The first slice: the allocating loop spends its (tiny) budget and is pre-empted.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 2)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Yield, "the budget pre-empts the loop");
    assert_eq!(
        state.engines.get(&pid).map(|engine| engine.tier),
        Some(Tier::Cranelift),
        "the tier chosen at the first entry is remembered on the engine"
    );
    assert!(
        state
            .engines
            .get(&pid)
            .is_some_and(|engine| engine.parked.is_some()),
        "the pre-empted machine is parked at the exhausting instruction"
    );

    // A second tiny-budget slice: the resumed process parks again, still on its own tier.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 2)
            .expect("the second slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Yield,
        "the resumed slice's own budget pre-empts it again"
    );
    assert_eq!(
        state.engines.get(&pid).map(|engine| engine.tier),
        Some(Tier::Cranelift),
        "a resume never silently downgrades the process's tier"
    );

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000_000)
            .expect("the completing slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    assert!(
        state.take_error(pid).is_none(),
        "the resumes took the Cranelift resume entry point, not a typed resume gap"
    );
    let cranelift = preempt_list_values(state.result(pid).expect("the value is recorded"));
    assert_eq!(
        cranelift.len(),
        PREEMPT_N as usize,
        "the three slices together ran the loop to completion"
    );

    // The interpreter-tier reference: the same fixture, the same budget schedule, its own runtime.
    assert_eq!(
        cranelift,
        preempt_interpreter_reference(),
        "the Cranelift slices compute the interpreter's exact result"
    );
}

/// A process parked in a `receive` on the **Cranelift** tier and woken by a message resumes on
/// its remembered tier and takes the delivered message.
///
/// This is the `wait` half of the same contract the pre-emption test above covers for a budget
/// park: a `receive` suspension reports the `wait`'s branch target, an address the tier serves
/// through its resume entry point (and, for an address it holds no block for, through the
/// driver's per-instruction interpreter step — the documented deopt, never a wrong result). The
/// observables are the same three the pre-emption test uses, because the tier's own artifact-store
/// counters are `pub(super)` inside `beam_engine`: the remembered `ProcessEngine::tier`, the
/// absence of a typed resume gap, and the delivered value.
#[cfg(feature = "cranelift")]
#[test]
fn a_receive_parked_process_resumes_on_its_cranelift_tier() {
    assert!(
        Tier::Cranelift.is_available(),
        "the runtime's `cranelift` feature compiles the engine's backend in"
    );

    let mut rt = runtime();
    let mut state = RunnerState::default();
    state.jit_mode = Some(JitMode::Cranelift);
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );

    // The first slice: `receive X -> X after infinity` parks on the empty mailbox.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks the receive");
    assert_eq!(
        state.engines.get(&pid).map(|engine| engine.tier),
        Some(Tier::Cranelift),
        "the tier chosen at the first entry is remembered on the engine"
    );
    assert!(
        state
            .engines
            .get(&pid)
            .is_some_and(|engine| engine.parked.is_some()),
        "the parked machine is held for the resume"
    );

    // Wake the receiver through the very queue the runner's receive seam scans.
    table
        .deliver_simple(pid, pid.to_eterm(), make_small(42))
        .expect("the message is delivered");

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    assert!(
        state.take_error(pid).is_none(),
        "the resume took the Cranelift resume entry point, not a typed resume gap"
    );
    assert_eq!(
        signed_value(state.result(pid).expect("the received value is recorded")),
        42,
        "the resumed receive took the delivered message"
    );
}

// -- pre-emption of a non-allocating, interpreted loop ---------------------

/// The non-allocating loop's iteration count in the pre-emption tests: far above one slice's
/// [`DEFAULT_REDUCTIONS_PER_SLICE`] budget, so the interpreter tier cannot finish it in one
/// slice.
const NON_ALLOCATING_N: i64 = 100_000;

/// Loads `fixture` through the runtime's **code server** (so its atoms land in the runtime's
/// one shared table, exactly as the CLI path's do) and hands back the module handle.
fn load_module(rt: &mut Runtime, fixture: &str, name: &str) -> Arc<Module> {
    rt.load_module(fixture).expect("the fixture loads");
    rt.code_server()
        .module_handle(name)
        .expect("the loaded module is registered by name")
}

/// Spawns `preempt:spin/1` (the PRE-EMPTION fixture's non-allocating tail loop) as process
/// `pid`.
///
/// `spin(N) -> spin(N, 0)` / `spin(N, Count) -> spin(N - 1, Count + 1)` is pure small-integer
/// arithmetic and a tail call, so its disassembly is only
/// `func_info`/`test`/`move`/`gc_bif`/`call_only`/`line`/`label`/`return` — **no** `test_heap`,
/// `allocate` or `allocate_heap`. It therefore reaches **no heap-admission point at all**, so a
/// pre-emption can only come from the interpreter's per-instruction reduction charge. Its
/// result is an **iteration count** (`spin(N)` returns exactly `N`), so a skipped or
/// double-applied instruction across a pre-emption/resume boundary changes the recorded value.
fn spawn_non_allocating_loop(
    state: &mut RunnerState,
    pid: ProcessId,
    module: Arc<Module>,
    n: i64,
) {
    let function = module.atom_table.lookup("spin").expect("spin is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 1,
        },
        vec![make_small(n)],
    );
}

/// **A non-allocating interpreted loop IS pre-empted, and the process queued behind it really
/// runs.**
///
/// The loop is `preempt:spin/1` of the PRE-EMPTION fixture and reaches no heap-admission
/// instruction (see [`spawn_non_allocating_loop`]), so a [`ProcOutcome::Yield`] after one
/// slice's budget can only have come from the engine's per-instruction `reduce` hook. The
/// yield is *resumption-exact and work-preserving*: no result is recorded
/// (`state.result(pid_a)` stays `None`) and the machine is parked at the instruction it
/// charged for. A second, short process registered on the same table then runs to completion
/// **while the loop is still unfinished** — the fairness a budget-ignoring runner cannot
/// provide. Driving the loop to completion finally records exactly its iteration count, whose
/// value is count-sensitive: a resume that skipped or double-applied an instruction would
/// change it.
#[test]
fn a_non_allocating_interpreted_loop_is_pre_empted() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    state.jit_mode = Some(JitMode::Interpreter);
    let mut table = table();

    // The loop and the short process `quick/0` both live in the PRE-EMPTION fixture, loaded
    // through the runtime's code server so its atoms land in the runtime's shared table.
    let module = load_module(&mut rt, preempt_fixture(), "preempt");

    // Registered FIRST: the CPU-bound loop. Registered SECOND: the short process it must not
    // starve.
    let pid_a = table.spawn_default().expect("a fresh process");
    let pid_b = table.spawn_default().expect("a fresh process");
    spawn_non_allocating_loop(&mut state, pid_a, Arc::clone(&module), NON_ALLOCATING_N);
    let quick = module
        .atom_table
        .lookup("quick")
        .expect("quick is interned");
    state.spawn(
        pid_b,
        ProcessEntry::Export {
            module,
            function: quick,
            arity: 0,
        },
        Vec::new(),
    );

    // Slice 1: the non-allocating loop spends a whole slice's budget and yields.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid_a, &mut table, DEFAULT_REDUCTIONS_PER_SLICE)
            .expect("the loop's first slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Yield,
        "the per-instruction charge pre-empts a loop with no allocation at all"
    );
    assert!(
        state.result(pid_a).is_none(),
        "a pre-emption fabricates no result and loses no work"
    );
    assert!(
        state
            .engines
            .get(&pid_a)
            .is_some_and(|engine| engine.parked.is_some()),
        "the pre-empted machine is parked at the instruction it charged for"
    );

    // The process behind it gets its turn while the loop is still unfinished.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid_b, &mut table, DEFAULT_REDUCTIONS_PER_SLICE)
            .expect("the short process's slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    assert_eq!(
        rt.atom_name_of(state.result(pid_b).expect("quick/0 returned a value")),
        Some("done"),
        "the short process really ran to its return"
    );
    assert!(
        state.result(pid_a).is_none(),
        "the loop is still unfinished after the short process ran"
    );

    // Drive the loop to completion in further slices. A generous budget makes the remaining
    // work one slice; the loop is bounded so a lost pre-emption can never hang the suite.
    let mut slices = 1;
    let outcome = loop {
        let outcome = {
            let mut runner = RuntimeRunner::new(&mut rt, &mut state);
            runner
                .run(pid_a, &mut table, 5_000_000)
                .expect("a further slice of the loop runs")
        };
        if outcome != ProcOutcome::Yield {
            break outcome;
        }
        slices += 1;
        assert!(slices < 64, "the loop finishes in a bounded number of slices");
    };
    assert_eq!(outcome, ProcOutcome::Exit(normal(&mut rt)));
    assert_eq!(
        signed_value(state.result(pid_a).expect("the loop returns its iteration count")),
        NON_ALLOCATING_N,
        "the charges cost no work: the resume applied every instruction exactly once"
    );
}

/// **The honest native-tier contrast: a compiled block is not charged per instruction.**
///
/// The *same* `preempt:spin/1` call with the *same* iteration count completes inside **one**
/// slice under the native baseline tier, even though that slice's budget is only
/// [`DEFAULT_REDUCTIONS_PER_SLICE`]: the loop reaches no heap-admission point (see
/// [`spawn_non_allocating_loop`]) and a natively executed block never crosses the
/// per-instruction `reduce` hook, so nothing charges mid-block. The recorded value is still
/// the iteration count — the tier is interpreter-identical, only its pre-emption granularity
/// differs. This is the residual limitation the interpreter gap's closure leaves behind, and
/// it is pinned rather than papered over.
#[test]
fn a_non_allocating_loop_on_the_native_tier_completes_within_one_slice() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    state.jit_mode = Some(JitMode::Baseline);
    let mut table = table();

    let module = load_module(&mut rt, preempt_fixture(), "preempt");
    let pid = table.spawn_default().expect("a fresh process");
    spawn_non_allocating_loop(&mut state, pid, module, NON_ALLOCATING_N);

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, DEFAULT_REDUCTIONS_PER_SLICE)
            .expect("the slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Exit(normal(&mut rt)),
        "a native compiled block reaches no per-instruction charge, so the loop is not \
         pre-empted mid-block"
    );
    assert_eq!(
        signed_value(state.result(pid).expect("the loop returns its iteration count")),
        NON_ALLOCATING_N,
        "the native tier computes the same result as the interpreter"
    );
}

// ---------------------------------------------------------------------------------------
// The parked machine's pending-exception reason is a GC root across the park window
// ---------------------------------------------------------------------------------------

/// `boom/0` raises `{badmatch,7}` **uncaught**: `Machine::throw` records the `(class, reason)`
/// pair in the machine's pending-exception slot even though the raise then propagates as `Err`.
/// That recorded slot is exactly the state `publish_parked_roots` must publish as a root.
fn raising_module() -> (Module, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("boom");
    // The raise tag must be interned in the module's own atom table.
    module.atom_table.intern("badmatch");
    let name = module.atom_table.intern("boom");
    let module_atom = module.name;
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Badmatch {
            value: Operand::Integer(7),
        },
        Instr::Return,
    ];
    let labels = labels_of(&code);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module_atom,
        name,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function: name,
        arity: 0,
        label: Label(1),
    });
    (module, name)
}

/// A **parked-shape** machine whose live pending-exception reason is `reason`.
///
/// It really raises (through the synthetic [`raising_module`]) so the slot exists, then the reason
/// is replaced through the public `Machine::pending_exception_mut()` write half. The raise's own
/// `{badmatch,7}` would live on the throwaway `MinimalContext` heap; overwriting the slot is what
/// puts a term on the *process's* young heap in it — exactly the shape a real parked machine's
/// reason has (a term the process's own heap owns) and therefore exactly what a collection in the
/// park window must relocate. The register/stack/FR root files are left empty, so the reason is
/// the only live word this machine holds.
fn machine_with_pending_reason(reason: Eterm) -> Machine {
    let (module, name) = raising_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let outcome = machine.run_export_outcome(&module, name, 0, &[], &mut ctx);
    assert!(
        outcome.is_err(),
        "boom/0 raises uncaught, got {outcome:?}"
    );
    assert_eq!(
        machine.pending_class(),
        Some(ExceptionClass::Error),
        "the raise recorded an error-class pending exception"
    );
    for slot in machine.x_mut() {
        *slot = NIL;
    }
    for slot in machine.stack_mut() {
        *slot = NIL;
    }
    for slot in machine.fr_mut() {
        *slot = NIL;
    }
    machine
        .pending_exception_mut()
        .expect("the recorded raise")
        .1 = reason;
    machine
}

/// Registers `state`'s engine for `pid` and seats `machine` in it, with the engine's own
/// host-held families marked **consumed**.
///
/// `publish_parked_roots` publishes an engine, not a bare machine: besides the machine's root set
/// and its pending-exception reason it appends the engine's own families
/// ([`ProcessEngine::append_parked_host_terms`]) — the first-run arguments, a fun entry's fun term
/// and a resolved local fun's argument list. A process that has already run one slice has consumed
/// all three (`ProcessEngine::args_consumed`), which is the shape every park arm in the runner
/// opens, so that is the shape these publication tests need: the reason is then the only extra
/// root, exactly as the assertions below require.
fn seat_machine(state: &mut RunnerState, pid: ProcessId, machine: Machine) {
    state.spawn_fun(pid, NIL, Vec::new());
    let engine = state
        .engines
        .get_mut(&pid)
        .expect("the engine is registered");
    engine.machine = machine;
    engine.args_consumed = true;
}

/// Assert `relocated` is the two-element small-integer tuple `expected` **at a new address**: the/// term really moved (a copied term never keeps its from-space address) and the copy is intact.
fn assert_tuple_relocated(relocated: Eterm, stale: Eterm, expected: [i64; 2], what: &str) {
    assert_ne!(
        relocated, stale,
        "{what}: the word was left pointing into the from-space — a collection moved the heap \
         without relocating it"
    );
    assert_eq!(
        terms::tuple_arity(relocated).unwrap_or_else(|e| panic!("{what}: {e}")),
        2,
        "{what}: the relocated word is not the two-element tuple it was"
    );
    for (index, value) in expected.iter().enumerate() {
        assert_eq!(
            terms::tuple_element(relocated, index).expect("in range"),
            make_small(*value),
            "{what}: element {index} of the relocated tuple"
        );
    }
}

/// **The parked machine's live pending-exception reason survives a moving collection.**
///
/// A suspended process's machine lives in `RunnerState::engines`; while it is parked, a
/// delivery-driven collection (`Process::deliver`'s exit cascade, ungated by
/// `gc_process_enabled`) relocates the process's heap against its own root set. The reason term is
/// a machine *field*, not an `x`/stack/FR slot, so `publish_parked_roots` publishes it as the
/// process's one extra root; the collection then moves it with the heap, and
/// `resync_parked_roots` reads the **relocated** word back into the machine.
///
/// The proof is by address: the reason is a `{7,8}` tuple built on the process's own young heap,
/// so a collection that did not publish/relocate it would leave the machine's slot on the dead
/// from-space word (a **wrong** term, not merely a stale one).
#[test]
fn a_parked_machines_pending_exception_reason_survives_a_moving_collection() {
    let mut rt = runtime();
    let mut table = table();
    let mut state = RunnerState::default();
    let pid = table.spawn_default().expect("a fresh process");

    // The reason: a two-element tuple of small ints on the PROCESS's own young heap.
    let reason = {
        let process = table.get_mut(pid).expect("the spawned process");
        process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds {7,8}")
    };

    seat_machine(&mut state, pid, machine_with_pending_reason(reason));

    // Publish the parked machine's roots: the reason rides the process's extra roots.
    {
        let engine = state
            .engines
            .get_mut(&pid)
            .expect("the engine is registered");
        publish_parked_roots(&mut table, pid, engine, 0);
    }
    let extra = table.get(pid).expect("the process").registers().extra_roots();
    assert_eq!(
        extra.len(),
        1,
        "the parked pending-exception reason is the one published extra root, got {extra:?}"
    );
    assert_eq!(extra[0], reason, "the published extra root is the reason itself");

    // Fill the nursery with unreachable tuples, so the collection is a genuine evacuation of a
    // full young heap — the shape a delivery-driven collection really has.
    {
        let process = table.get_mut(pid).expect("the process is in the table");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
    }

    // A genuine MOVING collection: `Process::collect()` is the same `run_rooted_collection` path a
    // delivery's `ensure_capacity` drives — it gathers x + stack + extra roots and scatters the
    // relocated words back into exactly those slots.
    table
        .get_mut(pid)
        .expect("the process is in the table")
        .collect()
        .expect("the collection runs");

    // The reason MOVED with the heap: its address changed, its value did not.
    let relocated = table
        .get(pid)
        .expect("the process")
        .registers()
        .extra_roots()
        .first()
        .copied()
        .expect("the extra root survived the collection");
    assert_tuple_relocated(
        relocated,
        reason,
        [7, 8],
        "the parked pending-exception reason",
    );

    // Resume: the machine reads the relocated reason back, and the extra roots are consumed.
    {
        let engine = state
            .engines
            .get_mut(&pid)
            .expect("the engine is registered");
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.resync_parked_roots(pid, &mut engine.machine, 0);
    }
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the engine is registered")
            .machine
            .pending_exception(),
        Some((ExceptionClass::Error, relocated)),
        "the resumed machine points at the RELOCATED reason, not the abandoned from-space word"
    );
    assert!(
        table.get(pid).expect("the process").registers().extra_roots().is_empty(),
        "the extra root is consumed on resume, so no stale root outlives the park window"
    );
}

/// **No live pending exception ⇒ no extra root.**
///
/// A parked machine with nothing to root must leave the process's extra roots **empty** — never a
/// NIL/placeholder — so the set is only ever the reason that is genuinely live.
#[test]
fn publishing_a_machine_without_a_pending_exception_leaves_no_extra_root() {
    let mut table = table();
    let mut state = RunnerState::default();
    let pid = table.spawn_default().expect("a fresh process");
    assert_eq!(
        Machine::new().pending_exception(),
        None,
        "a fresh machine has no raise"
    );
    seat_machine(&mut state, pid, Machine::new());

    {
        let engine = state
            .engines
            .get_mut(&pid)
            .expect("the engine is registered");
        publish_parked_roots(&mut table, pid, engine, 0);
    }
    assert!(
        table.get(pid).expect("the process").registers().extra_roots().is_empty(),
        "a machine with no pending exception publishes no extra root"
    );
}

/// **A stale extra root is cleared, never left behind.**
///
/// Publishing once with a live pending exception sets the extra root; once the machine's raise is
/// handled (here modelled by `Machine::reset`, which is what every `run_*` entry does) the very
/// same publish must leave the set empty rather than keeping the now-dead term alive forever.
#[test]
fn publishing_again_after_the_pending_exception_is_consumed_drops_the_stale_extra_root() {
    let mut table = table();
    let mut state = RunnerState::default();
    let pid = table.spawn_default().expect("a fresh process");
    let reason = {
        let process = table.get_mut(pid).expect("the spawned process");
        process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds {7,8}")
    };
    seat_machine(&mut state, pid, machine_with_pending_reason(reason));

    {
        let engine = state
            .engines
            .get_mut(&pid)
            .expect("the engine is registered");
        publish_parked_roots(&mut table, pid, engine, 0);
    }
    assert_eq!(
        table.get(pid).expect("the process").registers().extra_roots(),
        &[reason][..],
        "the first publish roots the live reason"
    );

    // The raise is handled: the machine has no pending exception any more.
    {
        let engine = state
            .engines
            .get_mut(&pid)
            .expect("the engine is registered");
        engine.machine.reset();
        assert_eq!(
            engine.machine.pending_exception(),
            None,
            "the raise is gone after a reset"
        );
        publish_parked_roots(&mut table, pid, engine, 0);
    }
    assert!(
        table.get(pid).expect("the process").registers().extra_roots().is_empty(),
        "the stale extra root is cleared, never left as a leftover/NIL placeholder"
    );
}

/// **A `receive` park publishes, so a signal-driven delivery collects and grows the receiver's
/// nursery instead of being refused.**
///
/// A `receive` park whose slice ended with a **dead** FR prefix publishes the machine's whole
/// root set (the x file, the Erlang stack, the empty FR prefix and the engine's extra roots) into
/// the process's [`Registers`](beam_process::Registers) and holds **no** relocation exemption, so
/// [`Process::has_unpublished_roots`](beam_process::Process::has_unpublished_roots) is `false`
/// for it and a signal — the `beam_process` signal builders are not gated by
/// `Runtime::gc_process_enabled` — really collects and grows the parked heap.
///
/// That is the whole point of the publication, and this case pins both halves of it: the
/// relocation happens (a nursery growth for a reason larger than the whole nursery), and the
/// notification is **not** lost. The installed host collection gate is carried by the table
/// exactly as production carries it, and it is asserted to answer `false` here — the receiver's
/// gathered roots reach nothing foreign — so the delivery route below is decided by the park's
/// own state alone, never by the gate.
#[test]
fn a_receive_park_publishes_so_a_signal_collects_the_receivers_nursery() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    // Mirror production exactly: the runtime's own table carries the installed gate
    // (`Runtime::new`), and `take_processes` carries it into the scheduler pass.
    table.set_collection_gate(Box::new(crate::context::gc_walk::RuntimeCollectionGate::new()));
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );

    // The first slice: `receive X -> X after infinity` parks on the empty mailbox.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks the receive");
    assert!(
        !table.is_relocation_exempt(pid).expect("the pid is live"),
        "a `receive` park with a dead FR prefix published its root set, so it holds no exemption"
    );
    assert!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .parked_published,
        "the park really published (the machine is resumable against the process's registers)"
    );

    // Non-vacuity of the route below: the delivery-driven collection is *permitted* for this
    // receiver, and the installed gate is not what decides it.
    {
        let process = table.get(pid).expect("the process is live");
        assert!(
            !process.has_unpublished_roots(),
            "a published `receive` park does not hand its roots over unpublished"
        );
        let mut gate = crate::context::gc_walk::RuntimeCollectionGate::new();
        assert!(
            !<crate::context::gc_walk::RuntimeCollectionGate as beam_process::CollectionGate>::declines_relocation(
                &mut gate,
                process
            ),
            "no foreign container is reachable from the parked process's gathered roots, so the \
             gate answers `false` — the park's own publication is what lets the move happen"
        );
    }

    // A reason LARGER than the receiver's WHOLE nursery: the on-enqueue copy must collect and
    // grow the parked heap, which is exactly what a published window permits.
    let capacity = table
        .get(pid)
        .expect("live")
        .collector()
        .young()
        .capacity_words();
    let mut src = beam_alloc::Heap::new(capacity + 8).expect("the source arena allocates");
    let elems: Vec<Eterm> = (0..capacity).map(|i| make_small(i as i64)).collect();
    let reason = src.tuple(&elems).expect("the oversized reason fits the arena");
    let signaller = table.spawn_default().expect("a signaller");
    table.get_mut(pid).expect("live").set_trap_exit(true);

    let minor_before = table.get(pid).expect("live").collector().minor_gcs();
    let base_before = table.get(pid).expect("live").collector().young().base();
    table
        .send_exit(signaller, pid, reason)
        .expect("the trapping receiver takes the signal");

    let r = table.get(pid).expect("the receiver survived");
    assert!(
        r.collector().minor_gcs() > minor_before,
        "the signal-driven delivery collected the parked receiver's heap (was {minor_before}, \
         now {})",
        r.collector().minor_gcs()
    );
    assert_ne!(
        r.collector().young().base(),
        base_before,
        "the reason exceeds the whole nursery, so the delivery had to grow it and the nursery \
         really moved"
    );
    assert_eq!(r.mailbox().len(), 1, "the notification is NOT lost");

    // The resume re-syncs the machine from the process's (relocated) registers and takes the
    // delivered exit message.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Exit(normal(&mut rt)),
        "the resumed receive takes the delivered exit message"
    );
    assert!(
        !table.is_relocation_exempt(pid).expect("the pid is live"),
        "no window left an exemption behind: the process collects normally at every later slice"
    );
}

/// **A park of a float-free process does NOT publish any FR word.**
///
/// `publish_roots_full` *can* publish the machine's float-register file by appending its
/// `Eterm` slots to the process's Erlang stack — real cost on every park and every collection
/// that follows it. The runner bounds that to the machine's **live leading prefix**, read once
/// per slice from `Machine::fr_mark()`. For a process whose FR is entirely `NIL` (the
/// overwhelming majority: it never touched a float) `fr_mark() == 0`, so the park appends no FR
/// word at all: the FR reaction contributes **nothing** — no FR word in the published stack
/// region, no exemption, no pin. The FR-specific signals are therefore the published stack length
/// (exactly the machine's own stack, never that plus the 1024-slot file), the zero `fr_published`
/// and `gc_stats().pinned == 0`.
///
/// `recv_after.beam`'s `wait_forever/0` is `receive X -> X after infinity end` — no local
/// variables, so its Erlang stack is shorter than the FR file — and it leaves no float register
/// set. The parked process's published stack must therefore be exactly the machine's own stack,
/// never that plus the FR file: the whole 1024-slot file is `NIL`, and the published region is
/// bounded by `fr_mark() == 0`.
#[test]
fn a_float_free_park_does_not_publish_the_fr_file() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );

    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks the receive");

    let (machine_stack_len, fr_mark) = {
        let engine = state.engines.get_mut(&pid).expect("the parked engine");
        let stack_len = engine.machine.with_roots(RootSet::ALL_LIVE, |roots| roots.stack.len());
        (stack_len, engine.machine.fr_mark())
    };
    assert_eq!(fr_mark, 0, "the float-free fixture leaves no live FR slot");
    assert!(
        machine_stack_len < beam_engine::machine::MAX_FLOAT_REGISTER as usize,
        "the fixture's `wait_forever/0` has no Y slots, so its stack is shorter than the FR file \
         (got {machine_stack_len}); a `+ FR file` publication would be distinguishable below"
    );
    assert_eq!(
        park_publish_counters().published,
        1,
        "a float-free `receive` park PUBLISHES — the hand-off is not skipped"
    );
    assert_eq!(
        park_publish_counters().skipped,
        0,
        "and nothing skipped it"
    );
    assert!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .parked_published,
        "the engine records that its roots really were handed over"
    );
    let published = table
        .get(pid)
        .expect("the parked process")
        .registers()
        .stack()
        .len();
    assert_eq!(
        published, machine_stack_len,
        "the published stack region is exactly the machine's own Erlang stack \
         ({machine_stack_len} words) — never that plus the 1024-slot FR file and never that plus \
         the live prefix — so the `fr_mark` bound contributes no FR word at all"
    );
    assert!(
        table
            .get(pid)
            .expect("the parked process")
            .registers()
            .x()
            .live()
            > 0,
        "the publication really wrote the x file, so the equality above is a *bounded* \
         publication rather than an absent one"
    );
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .fr_published,
        0,
        "the park records a zero-length FR prefix on the engine"
    );
    assert_eq!(
        rt.gc_stats().pinned,
        0,
        "a process whose FR prefix is empty is not pinned"
    );
    assert!(
        !table.is_relocation_exempt(pid).expect("the pid is live"),
        "a dead FR prefix opens no exemption: the window is collectable, so a delivery into this \
         receiver copies straight into its nursery"
    );
}

// ---------------------------------------------------------------------------------------
// The engine's host-held families: published, relocated by a collection, adopted back
// ---------------------------------------------------------------------------------------
/// **A term held ONLY by the parked engine's host-held families survives a moving collection.**
///
/// The machine's `x`/stack/FR files are not the whole parked root set: an engine whose first run
/// has not happened yet also holds the spawn arguments ([`ProcessEngine::args`]) and a fun entry's
/// fun term ([`ProcessEntry::Fun::fun`]), and *nothing else* names them — they are `Eterm` fields
/// of a Rust struct, invisible to the collector. [`publish_parked_roots`] therefore appends them
/// after the machine's own roots ([`ProcessEngine::append_parked_host_terms`]) and the resume
/// writes the relocated words back ([`ProcessEngine::adopt_relocated_parked_roots`]).
///
/// The proof is by address **and by value**: both terms are two-element small-integer tuples built
/// on the process's own young heap, so a collection that did not publish them would leave the
/// fields pointing into the abandoned from-space (a wrong term, not merely a stale one). The
/// nursery is then filled with unreachable tuples so the collection is a genuine evacuation.
#[test]
fn a_parked_engines_host_held_terms_survive_a_moving_collection() {
    let mut table = table();
    let mut state = RunnerState::default();
    let pid = table.spawn_default().expect("a fresh process");
    let (arg, fun) = {
        let process = table.get_mut(pid).expect("the spawned process");
        let arg = process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds {7,8}");
        let fun = process
            .heap_mut()
            .tuple(&[make_small(9), make_small(10)])
            .expect("the young heap builds {9,10}");
        (arg, fun)
    };
    // A fun-bodied engine that has not run its first entry: that is the one shape in which the
    // engine's own families are live (`!args_consumed`), i.e. exactly what
    // `append_parked_host_terms` exists for.
    state.spawn_fun(pid, fun, vec![arg]);
    assert!(
        !state
            .engines
            .get(&pid)
            .expect("the engine is registered")
            .args_consumed,
        "a never-run engine holds its arguments and fun term as live roots"
    );

    {
        let engine = state
            .engines
            .get_mut(&pid)
            .expect("the engine is registered");
        publish_parked_roots(&mut table, pid, engine, 0);
    }
    let extra = table
        .get(pid)
        .expect("the process")
        .registers()
        .extra_roots()
        .to_vec();
    assert_eq!(
        extra,
        [arg, fun][..],
        "the publication carries both host-held families, in order: the arguments first, then the \
         fun entry's fun term"
    );

    // A genuinely full nursery: unreachable tuples, so the collection below must evacuate.
    {
        let process = table.get_mut(pid).expect("the process is in the table");
        let filler = [make_small(1)];
        while process.heap().available() >= 16 {
            process
                .heap_mut()
                .tuple(&filler)
                .expect("the nursery has room for a two-word tuple");
        }
    }
    table
        .get_mut(pid)
        .expect("the process is in the table")
        .collect()
        .expect("the collection runs");

    let relocated = table
        .get(pid)
        .expect("the process")
        .registers()
        .extra_roots()
        .to_vec();
    assert_eq!(
        relocated.len(),
        2,
        "both published host-held words survive the collection, got {relocated:?}"
    );
    assert_tuple_relocated(
        relocated[0],
        arg,
        [7, 8],
        "the parked engine's published argument",
    );
    assert_tuple_relocated(
        relocated[1],
        fun,
        [9, 10],
        "the parked engine's published fun term",
    );

    // The resume half: the fields adopt the RELOCATED words, never the from-space ones.
    {
        let engine = state
            .engines
            .get_mut(&pid)
            .expect("the engine is registered");
        engine.adopt_relocated_parked_roots(&relocated);
        assert_eq!(
            engine.args,
            vec![relocated[0]],
            "the engine's arguments adopt the relocated word"
        );
        match &engine.entry {
            ProcessEntry::Fun { fun: adopted, .. } => assert_eq!(
                *adopted, relocated[1],
                "the fun entry's fun term adopts the relocated word"
            ),
            other => panic!("the engine is fun-bodied, got {other:?}"),
        }
    }
}

/// **A `receive`-parked process does NOT hand its roots over unpublished — the non-vacuity guard
/// for every delivery-driven collection test in this module.**
///
/// [`Process::has_unpublished_roots`](beam_process::Process::has_unpublished_roots) is the single
/// predicate [`Process::deliver`](beam_process::Process::deliver) refuses the relocating collection
/// on (it is `relocation_exempt || Running`), and a refusal is exactly what
/// [`deliver_into_mailbox`](crate::context::RuntimeContext::deliver_into_mailbox) turns into the
/// `send_fragmented_message` fallback, so an answer of `true` makes the fragment route (a carrier
/// allocation plus a second full `copy_struct` of the payload) the default path of every ping-pong
/// and server loop.
///
/// The `Wait` arm parks under [`ParkExemption::LiveFloats`] (`exempt = fr_count > 0`), so a
/// dead-FR receiver publishes its whole root set into the process's [`Registers`] — the machine's
/// files plus the engine's host-held families — and answers `false`, which is what makes the
/// delivery-driven collection in [`a_receive_park_publishes_so_a_signal_collects_the_receivers_nursery`]
/// and [`a_delivery_into_a_receive_parked_process_copies_straight_into_its_heap`] reachable at
/// all. Without this assertion those tests could pass for the wrong reason (a fragment route that
/// happens to carry the right value).
#[test]
fn a_receive_parked_process_hands_its_roots_over_published() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks the receive");
    let process = table.get(pid).expect("the parked process");
    assert!(
        !process.is_relocation_exempt(),
        "the `Wait` arm parks under `LiveFloats`, so a dead-FR receiver holds no exemption"
    );
    assert!(
        !process.has_unpublished_roots(),
        "a parked `receive` receiver published its roots: `deliver` may collect and grow its \
         nursery instead of refusing the move"
    );
    assert!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .parked_published,
        "the engine records the hand-off"
    );
}

/// **A delivery into a `receive`-parked process copies straight into its heap: no fragment.**
///
/// The route is the assertion, not just the value. A published receiver passes
/// `has_unpublished_roots` and its roots come from [`Registers`], so [`Process::deliver`]
/// collects and grows the nursery and
/// [`deliver_into_mailbox`](crate::context::RuntimeContext::deliver_into_mailbox) never reaches
/// `send_fragmented_message`. A fragmented payload would instead live in the message's own
/// `Fragments` (`Message::fragments()` is `Some`) and would **not** move with the receiver's heap,
/// so the two observables below pin the route:
///
/// * `fragments().is_none()` — the payload is heap-resident, never in a heap-fragment chain;
/// * the payload's address changes when the receiver's own heap is collected afterwards (and its
///   elements survive by value), which is only true of a payload the receiver's heap owns.
///
/// The payload is larger than the receiver's **whole** nursery, so the delivery really has to
/// collect **and grow**, which is the case a `Send`-side exemption used to refuse.
#[test]
fn a_delivery_into_a_receive_parked_process_copies_straight_into_its_heap() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks the receive");

    // A payload larger than the receiver's whole nursery, built on a pointer-stable arena (the
    // copier reads the source words; only the destination has to grow).
    let capacity = table
        .get(pid)
        .expect("live")
        .collector()
        .young()
        .capacity_words();
    let mut src = beam_alloc::Heap::new(capacity + 8).expect("the source arena allocates");
    let elems: Vec<Eterm> = (0..capacity).map(|i| make_small(i as i64)).collect();
    let payload = src.tuple(&elems).expect("the oversized payload fits the arena");

    let minor_before = table.get(pid).expect("live").collector().minor_gcs();
    let base_before = table.get(pid).expect("live").collector().young().base();
    table
        .deliver_simple(pid, NIL, payload)
        .expect("the published receiver accepts the delivery");

    let process = table.get(pid).expect("the receiver is live");
    assert!(
        process.collector().minor_gcs() > minor_before,
        "the delivery collected the receiver's nursery (was {minor_before}, now {})",
        process.collector().minor_gcs()
    );
    assert_ne!(
        process.collector().young().base(),
        base_before,
        "the payload exceeds the whole nursery, so the delivery had to grow it"
    );
    assert_eq!(process.mailbox().len(), 1, "exactly one message is queued");
    let message = process.mailbox().peek().expect("the queued message");
    assert!(
        message.fragments().is_none(),
        "the queued message carries NO fragment: the payload was copied into the receiver's own \
         heap, so `send_fragmented_message` was never reached"
    );
    let queued = message.payload();
    assert_ne!(
        queued, payload,
        "the queued payload is the receiver's own copy, not the source arena's term"
    );
    assert_eq!(
        terms::tuple_arity(queued).expect("a tuple"),
        capacity,
        "the whole payload arrived"
    );
    assert_eq!(
        terms::tuple_element(queued, 0).expect("in range"),
        make_small(0),
        "the first element survives by value"
    );
    assert_eq!(
        terms::tuple_element(queued, capacity - 1).expect("in range"),
        make_small(capacity as i64 - 1),
        "the last element survives by value"
    );

    // Heap-resident, proven by ownership: a collection of the RECEIVER moves the queued payload
    // (a fragmented payload would not move at all).
    table
        .get_mut(pid)
        .expect("the receiver is live")
        .collect()
        .expect("the collection runs");
    let process = table.get(pid).expect("the receiver is live");
    let after = process
        .mailbox()
        .peek()
        .expect("the message survives the collection")
        .payload();
    assert_ne!(
        after, queued,
        "the queued payload is relocated with the receiver's heap, so that heap owns it"
    );
    assert_eq!(
        terms::tuple_element(after, 0).expect("in range"),
        make_small(0),
        "and it is relocated intact"
    );
    assert_eq!(
        terms::tuple_element(after, capacity - 1).expect("in range"),
        make_small(capacity as i64 - 1),
        "the tail of the payload survives the relocation too"
    );
}

/// **A park whose FR *is* live SKIPS the hand-off and holds the inter-slice window's relocation
/// exemption — never a permanent pin.**
///
/// The live FR prefix is the one thing no publication can carry: the park publishes at most the
/// live `fr_mark` prefix of the file, so a slot above it could be moved out from under the
/// machine. A park that ends with a live prefix therefore skips the publication entirely and holds
/// the process's **relocation exemption** for the window (the next slice's resume clears it), not
/// a permanent runtime-level pin: a float-using process must still be collectable at its own
/// heap-admission points.
///
/// A parked machine is not reset on resume, so an FR slot written through the engine's own
/// `Machine::set_fr` seam while the process is parked survives into the next slice and raises
/// the mark: setting slot 0 stands in for a float computation that left a register set (the
/// exact residue the post-slice `fr_mark` check exists to catch), and the *second* park then
/// observes one live FR slot. This pins the skipping branch and `ProcessEngine::fr_published`
/// (which records the live prefix for the resume's bookkeeping) without needing compiled float
/// code; the publication that *does* carry the live prefix is pinned by
/// [`the_published_fr_prefix_round_trips_through_a_moving_collection`].
#[test]
fn a_live_fr_park_skips_the_hand_off_and_opens_the_window_exemption() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );

    // Slice 1: a dead-FR `receive` park — the hand-off happens, and the process is NOT pinned.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait);
    assert_eq!(rt.gc_stats().pinned, 0, "an empty FR prefix does not pin");

    // Make FR slot 0 live while the machine is parked (a resume does not reset the machine).
    // `set_fr` is the seam the interpreter itself uses, so it raises the mark to 1 — the count
    // the park below records for the resume.
    state
        .engines
        .get_mut(&pid)
        .expect("the parked engine")
        .machine
        .set_fr(0, make_small(42))
        .expect("slot 0 is in the FR file");

    // Slice 2: resuming with an empty mailbox parks again — now with one live FR slot.
    // The structural counter is per-park, so it is zeroed before the measured slice (slice 1
    // above is a publishing `Wait` park and would otherwise be counted too).
    reset_park_publish_counters();
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks again");

    assert_eq!(
        rt.gc_stats().pinned,
        0,
        "a live FR no longer pins the process for the rest of its life"
    );
    assert!(
        table.is_relocation_exempt(pid).expect("the pid is live"),
        "a slice that ends with a live FR prefix opens the inter-slice window's relocation \
         exemption, so the window's own collection sites decline the move"
    );
    let (machine_stack_len, fr_mark) = {
        let engine = state.engines.get_mut(&pid).expect("the parked engine");
        let stack_len = engine.machine.with_roots(RootSet::ALL_LIVE, |roots| roots.stack.len());
        (stack_len, engine.machine.fr_mark())
    };
    assert_eq!(fr_mark, 1, "one FR slot is live, so the mark is one word");
    let published = table
        .get(pid)
        .expect("the parked process")
        .registers()
        .stack()
        .len();
    assert_eq!(
        published, 0,
        "a live-FR park opens a **relocation-exempt** window, so it hands nothing over at all: \
         the parked machine (whose stack is {machine_stack_len} words plus the live FR prefix \
         fr_mark = {fr_mark}) stays the authoritative root set, and neither the machine's stack \
         nor the 1024-slot FR file is copied"
    );
    assert_eq!(
        park_publish_counters().skipped,
        1,
        "the structural counter records the skipped publication"
    );
    assert_eq!(
        park_publish_counters().published,
        0,
        "an exempt park publishes no machine root set"
    );
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .fr_published,
        fr_mark as usize,
        "the engine still records the live prefix length for the resume's bookkeeping"
    );
}

/// **The live-FR window exemption is per slice: a `Yield` window opens it, the next slice entry
/// clears it, and nothing is ever pinned.**
///
/// This is the defect's regression pin. The pre-slice guard used to mark the pid **permanently**
/// un-collectable at the runtime level, so the first slice that ended with a live FR prefix
/// made `Runtime::gc_process_enabled` answer `false` for the rest of the process's life and
/// `Runtime::gc_stats().pinned` never came back down. The window exemption is scoped instead:
/// a slice that ends with `fr_mark() > 0` marks the pid relocation-exempt for the window that
/// follows **and nothing else**, and the very next slice's entry clears it (the resume arm) — so
/// a float-using process still collects at its own heap-admission points. That clear is not
/// observable from outside a slice (`is_relocation_exempt` is only readable *between* slices,
/// where a live FR prefix re-opens the exemption); its **effect** — a float-using loop collecting
/// *more* as N grows instead of plateauing — is the property pinned in `tests/gc_collect.rs` and
/// `../context/tests.rs`, and it is impossible while the pid carries a permanent un-collectable mark.
///
/// `preempt:spin/1` is the non-allocating tail loop: a tiny budget pre-empts it
/// (`ProcOutcome::Yield`) so the slice ends while the process is still runnable — a park arm whose
/// window the FR prefix alone decides, exactly as at the `Wait` arm. FR slot 0 is written through the engine's own `Machine::set_fr`
/// seam between the slices, standing in for a float computation that left a register live.
#[test]
fn the_live_fr_window_exemption_is_opened_per_slice_and_never_pinned() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    state.jit_mode = Some(JitMode::Interpreter);
    let mut table = table();
    let module = load_module(&mut rt, preempt_fixture(), "preempt");
    let pid = table.spawn_default().expect("a fresh process");
    spawn_non_allocating_loop(&mut state, pid, module, NON_ALLOCATING_N);

    // Slice 1: a dead FR prefix — the pre-empted process is NOT exempt and NOT pinned.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 10)
            .expect("the loop's first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Yield, "the tiny budget pre-empts the loop");
    assert!(
        !table.is_relocation_exempt(pid).expect("the pid is live"),
        "a dead FR prefix opens no exemption"
    );
    assert_eq!(rt.gc_stats().pinned, 0, "and pins nothing");

    // A live FR slot on the parked machine (a resume does not reset the FR file).
    state
        .engines
        .get_mut(&pid)
        .expect("the parked engine")
        .machine
        .set_fr(0, make_small(42))
        .expect("slot 0 is in the FR file");

    // Slice 2: the same tiny budget pre-empts the loop again, this time with FR slot 0 live —
    // the post-slice `fr_mark` check opens the inter-slice window's exemption.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 10)
            .expect("the loop's second slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Yield, "the budget pre-empts it again");
    assert!(
        table.is_relocation_exempt(pid).expect("the pid is live"),
        "a slice that ends with a live FR prefix opens the window's relocation exemption"
    );
    assert_eq!(
        rt.gc_stats().pinned,
        0,
        "the window is scoped by the exemption, not by a permanent runtime-level pin"
    );
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .fr_published,
        1,
        "the park published exactly the one live FR slot the resume must scatter back"
    );

    // Slice 3: the resume clears the exemption at entry (the parked window is over and only a
    // window's collection sites are declined), so the running slice is not exempt and can collect
    // at its own heap-admission points. Drive the loop to completion and check nothing is pinned.
    //
    // The exemption left armed at the end of the *terminating* slice is that slice's own
    // re-derivation: FR slot 0 is still live when it ends (a resume never resets the machine and
    // `set_fr` only ever raises the mark), so the post-slice `fr_mark` check re-opens it for a
    // window no further slice ever opens. In production that is inert — `slice_end`'s
    // `ProcessTable::terminate` drops the mark with the dead process — and the runner cannot
    // observe the mid-slice clear directly, so the property asserted here is the one the defect
    // was about: **nothing is ever pinned**.
    let mut outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, DEFAULT_REDUCTIONS_PER_SLICE)
            .expect("the resume slice runs")
    };
    while outcome == ProcOutcome::Yield {
        outcome = {
            let mut runner = RuntimeRunner::new(&mut rt, &mut state);
            runner
                .run(pid, &mut table, DEFAULT_REDUCTIONS_PER_SLICE)
                .expect("a further slice runs")
        };
    }
    assert!(
        matches!(outcome, ProcOutcome::Exit(_)),
        "the loop finished, so the process terminated; got {outcome:?}"
    );
    assert_eq!(
        rt.gc_stats().pinned,
        0,
        "nothing was ever pinned — the defect this test pins"
    );
}

/// **The published FR prefix round-trips through a park → moving collection → resume.**
///
/// The bounded publication must still relocate a live FR term: the FR word sits at the **end**
/// of the process's published stack region, so a collection relocates it with the rest of the
/// roots and the resume must split the same region back into the machine's FR slots. The proof
/// is by address — the FR slot holds a `{7,8}` tuple built on the process's own young heap, so a
/// scatter that lost the FR word would leave the machine's slot on a dead from-space word (a
/// **wrong** term, not merely a stale one).
///
/// The hand-off appends one FR word (`fr_mark() == 1`), the collection moves the tuple, and the
/// resume scatters the relocated word back into FR slot 0.
#[test]
fn the_published_fr_prefix_round_trips_through_a_moving_collection() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );

    // Slice 1 parks the receive with a dead FR.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the first slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait);

    // A `{7,8}` tuple on the PROCESS's own young heap, published as FR slot 0 through the
    // engine's `set_fr` seam (which raises `fr_mark` to 1).
    let original = {
        let process = table.get_mut(pid).expect("the parked process");
        let tuple = process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds {7,8}");
        state
            .engines
            .get_mut(&pid)
            .expect("the parked engine")
            .machine
            .set_fr(0, tuple)
            .expect("slot 0 is in the FR file");
        tuple
    };

    // Slice 2: resume, then park again — an exempt window again (the live FR prefix pins it), so
    // the park itself hands nothing over. Publish the one live FR word **by hand** instead: that
    // is exactly what a park of a window a collection *can* enter does, i.e. `park_machine` with
    // `exempt == false` (a `Yield` park whose slice ended with a dead FR prefix), and it is the
    // hand-off this test pins.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks again");
    {
        let engine = state.engines.get_mut(&pid).expect("the parked engine");
        assert_eq!(
            engine.fr_published, 1,
            "the park recorded the one live FR slot for the resume"
        );
        assert!(
            !engine.parked_published,
            "the exempt park handed nothing over: the machine is still authoritative"
        );
        publish_parked_roots(&mut table, pid, engine, 1);
        engine.parked_published = true;
    }

    // A genuine MOVING collection over the parked process: it gathers the process's stack
    // (which holds the published FR word at its end) and scatters the relocated words back.
    //
    // The `Wait` park arm left the process relocation-exempt for its parked window (and the
    // live FR prefix this slice ended with would in any case have opened the same exemption),
    // and `Process::collect` refuses an exempt process with the typed `relocation_exempt` raise
    // — so clear it first. That is exactly the `Yield` (pre-emption) window's situation once the
    // exemption is cleared: a pre-empted process is not holding its roots unpublished, so its
    // published FR prefix is the only thing that keeps a live FR term across a collection taken
    // while it sits in `RunnerState.engines`.
    table
        .set_relocation_exempt(pid, false)
        .expect("the pid has a process");
    table
        .get_mut(pid)
        .expect("the process is in the table")
        .collect()
        .expect("the collection runs");

    // Resume: the machine re-syncs from the process's authoritative root set, so FR slot 0 must
    // come back holding the RELOCATED tuple.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the third slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the receive parks yet again");
    let resumed = state
        .engines
        .get(&pid)
        .expect("the parked engine")
        .machine
        .get_fr(0)
        .expect("slot 0 is in the FR file");
    assert_tuple_relocated(resumed, original, [7, 8], "the published FR slot");
}

// -- deterministic per-call counters on a growth-shaped workload -----------
//
// The `foreign` benchmark's `emqx_base62:encode` sub-case is recorded as **3.2×–3.8× dearer**
// on the current binary than on the m10 one, and the excess is **N-dependent** (it rises with
// the loop count). This host is shared and loaded, so wall-clock cannot decide whether that is
// a real per-call work increase or a measurement artefact: only **deterministic counters** can.
//
// The counters below are all structural facts of the runner / runtime, never timings:
//
// * the runner's own slice/park accounting (`RUN_SLICE_COUNTERS`, a per-thread mirror of
//   `REGISTERED_PAYLOAD_TOTAL`);
// * the runtime's GC accounting (`Runtime::gc_stats()`);
// * the process's own nursery geometry (`Process::collection_debt` /
//   `AnyCollector::young_space_count` / `mature_capacity_words` / `mature_used_words`), sampled
//   at every slice boundary — the only window a live process's collector is readable from
//   outside the runner, and therefore the reason each measured workload's iteration counts are
//   chosen so that **every** run spans several slices.
//
// The question they answer is exactly the one the regression raises: **does the per-call work
// grow with N** (collections/call, slices/call), or is it flat? A flat per-call counter with a
// rising wall clock is a measurement artefact; a counter that grows with N is the real signal.

/// The allocate-and-abandon **churn** fixture: `run(N)` builds a 256-element list, binary and
/// tuple per iteration (abandoning each), asserts the two `O(1)`-checkable ones and returns `N`.
///
/// It is the closest existing fixture to the `foreign` `emqx_base62:encode` loop — a constant
/// live set with one allocation-heavy call per iteration — and every binary/tuple it keeps comes
/// from a **registered BIF** (`list_to_binary/1`, `list_to_tuple/1`), so it drives the
/// BIF-result copy-out path (`RuntimeContext::move_result_into_process_heap`, whose learned-hint
/// growth gate makes the nursery — through the no-relocation [`Process::grow_nursery`] — large
/// enough for the graph *before* the copy, with the refusal → grow → retry path behind it).
fn churn_fixture() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/churn.beam")
}

/// The **growth** fixture: `run(N)` repeats a single 401-word `test_heap` allocation N times,
/// discarding each tuple — an engine-native growth shape with no BIF-result copy-out at all.
fn heap_growth_fixture() -> &'static str {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/heap_growth.beam")
}

/// A generous wall-clock safety net for one measured run (the loops are pure interpreter work
/// and finish well inside it; this only bounds a pathological hang, it is never measured).
const COUNTER_RUN_DEADLINE: Duration = Duration::from_secs(300);

/// The **deterministic** cost of one scheduled run of a growth-shaped workload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GrowthCounters {
    /// The loop's returned value (it must equal the iteration count `N`).
    value: i64,
    /// `RuntimeRunner::run` calls — one reduction slice each.
    slices: usize,
    /// Slices that ended at the `Yield` (reduction-budget pre-emption) arm.
    yields: usize,
    /// Slices that ended at the `receive` `Wait` arm.
    waits: usize,
    /// The reduction units the run's slices charged — the delta of
    /// `erlang:statistics(reductions)`'s total across the run, i.e. exactly the sum of the
    /// [`BudgetProbe::used`](super::budget::BudgetProbe) figures
    /// [`RuntimeRunner::run`](super::run::RuntimeRunner) hands to `Runtime::note_reductions`.
    /// A **deterministic** count (the seams each instruction reaches are fixed by the
    /// bytecode), never a timing.
    reductions: u64,
    /// Process-heap collections the runtime ran during this run.
    collections: u64,
    /// How many of [`GrowthCounters::collections`] were fullsweeps.
    major: u64,
    /// Engine allocations that fell back to the pointer-stable arena.
    fallbacks: u64,
    /// Collections that failed.
    errors: u64,
    /// The largest retired-nursery chain length observed at a slice boundary.
    peak_young_spaces: usize,
    /// The largest young-nursery capacity (words) observed at a slice boundary.
    peak_young_capacity: usize,
    /// The largest mature-generation capacity (words) observed at a slice boundary.
    peak_mature_capacity: usize,
    /// The largest mature-generation occupancy (words) observed at a slice boundary.
    peak_mature_used: usize,
    /// How many slice boundaries observed an un-settled collection debt.
    debt_boundaries: usize,
}

/// Runs `module:run/1` with argument `n` as a **scheduled process** on `rt`, counting every
/// deterministic counter the runner and the runtime expose.
///
/// The nursery geometry is sampled at every slice boundary (between `run_one_slice` calls),
/// which is the only window a process's collector is readable from outside: a terminated
/// process is removed from the table, so the peak over the run can only be observed *during* it.
fn run_growth_counted(rt: &mut Runtime, module: &str, n: i64) -> GrowthCounters {
    run_growth_function(rt, module, "run", n)
}

/// [`run_growth_counted`] with the **entry function** named by the caller, so a fixture whose
/// loop is exported under another name (or reports something of its own instead of the
/// iteration count) can be driven by the same harness.
///
/// The row's `value` is whatever the entry returned — for a `run/1` fixture the iteration count
/// `n`, for a fixture that reports its own `heap_size` that figure. A boundary sample only
/// exists for a run that **ends a slice**; a shape that completes inside its first slice leaves
/// the geometry peaks at `0` and so has to report any heap figure itself (see
/// [`slice_charging`], which does exactly that).
fn run_growth_function(
    rt: &mut Runtime,
    module: &str,
    function: &str,
    n: i64,
) -> GrowthCounters {
    let before = rt.gc_stats();
    let reductions_before = rt.reductions_statistics(false).0;
    reset_run_slice_counters();
    let pid = rt
        .spawn_process(module, function, vec![make_small(n)])
        .unwrap_or_else(|err| panic!("{module}:{function}/1 must spawn: {err}"));
    let mut peak_young_spaces = 0usize;
    let mut peak_young_capacity = 0usize;
    let mut peak_mature_capacity = 0usize;
    let mut peak_mature_used = 0usize;
    let mut debt_boundaries = 0usize;
    let deadline = Instant::now() + COUNTER_RUN_DEADLINE;
    while rt.process_result(pid).is_none() {
        assert!(
            Instant::now() < deadline,
            "{module}:{function}/1 with N={n} did not terminate within {}s",
            COUNTER_RUN_DEADLINE.as_secs()
        );
        rt.run_one_slice().expect("a scheduler slice runs");
        if let Some(process) = rt.processes().get(pid) {
            let collector = process.collector();
            peak_young_spaces = peak_young_spaces.max(collector.young_space_count());
            peak_young_capacity = peak_young_capacity.max(collector.young().capacity_words());
            peak_mature_capacity = peak_mature_capacity.max(collector.mature_capacity_words());
            peak_mature_used = peak_mature_used.max(collector.mature_used_words());
            if process.collection_debt() {
                debt_boundaries += 1;
            }
        }
    }
    let counters = run_slice_counters();
    let after = rt.gc_stats();
    GrowthCounters {
        value: signed_value(rt.process_result(pid).expect("the loop returned a value")),
        slices: counters.slices,
        yields: counters.yields,
        waits: counters.waits,
        reductions: rt.reductions_statistics(false).0 - reductions_before,
        collections: after.collections - before.collections,
        major: after.major - before.major,
        fallbacks: after.fallbacks - before.fallbacks,
        errors: after.errors - before.errors,
        peak_young_spaces,
        peak_young_capacity,
        peak_mature_capacity,
        peak_mature_used,
        debt_boundaries,
    }
}

/// A fresh runtime with `fixture` loaded into its code server, ready to `spawn_process` from.
///
/// The runtime is **booted and drained** before it is handed back: `Runtime::spawn_process`
/// boots on demand (`Runtime::boot_if_needed`), and the boot script's own processes would
/// otherwise contend with the measured loop for slices — so the measurement must start from a
/// quiescent run queue. The drain runs boundedly until the queue is empty (the booted
/// `{kernelProcess,…}` servers are `Waiting`, so the first empty pass ends it).
fn growth_runtime(fixture: &str) -> Runtime {
    let mut rt = Runtime::with_defaults().expect("the default runtime");
    rt.boot().expect("the boot sequence runs");
    let mut guard = 0u32;
    while rt.run_one_slice().expect("a boot-drain slice runs") > 0 {
        guard += 1;
        assert!(guard < 100_000, "the boot process tree never quiesced");
    }
    rt.load_module(fixture).expect("the growth fixture loads");
    rt
}

/// Runs one growth-shaped workload at each of `counts` (increasing iteration counts, a fresh
/// runtime per row) and prints the counter table; every counter is reported so a caller can read
/// it without the test source.
fn counter_table(
    fixture: &str,
    module: &str,
    counts: &[i64],
) -> Vec<(i64, GrowthCounters)> {
    let rows: Vec<(i64, GrowthCounters)> = counts
        .iter()
        .map(|&n| {
            let mut rt = growth_runtime(fixture);
            let row = run_growth_counted(&mut rt, module, n);
            assert_eq!(row.value, n, "{module}:run({n}) must return its argument");
            assert_eq!(row.errors, 0, "no collection may fail: N={n} {row:?}");
            assert_eq!(row.waits, 0, "{module}:run/1 never parks in a receive: {row:?}");
            (n, row)
        })
        .collect();
    eprintln!(
        "counter_table {module} (N : slices yields waits reductions collections major fallbacks \
         peak_spaces peak_cap peak_mature_cap peak_mature_used debt_bounds | \
         slices/call collections/call)"
    );
    for (n, row) in &rows {
        let calls = (*n as f64) + 1.0;
        eprintln!(
            "counter_table {module} N={n} : slices={} yields={} waits={} reductions={} \
             collections={} major={} fallbacks={} peak_spaces={} peak_cap={} peak_mature_cap={} \
             peak_mature_used={} debt_bounds={} | \
             slices/call={:.6} collections/call={:.6}",
            row.slices,
            row.yields,
            row.waits,
            row.reductions,
            row.collections,
            row.major,
            row.fallbacks,
            row.peak_young_spaces,
            row.peak_young_capacity,
            row.peak_mature_capacity,
            row.peak_mature_used,
            row.debt_boundaries,
            row.slices as f64 / calls,
            row.collections as f64 / calls,
        );
    }
    rows
}

/// The words one `heap_growth:run/1` iteration requests from the heap: the fixture's single
/// `test_heap 1 401` (`tests/fixtures/heap_growth.erl`), i.e. this shape's **per-iteration
/// footprint**. Every collection-cadence derivation below divides by it, so a fixture that
/// changes the request moves the derivation instead of silently breaking a quoted relation.
const HEAP_GROWTH_ITERATION_WORDS: usize = 401;

/// How many iterations one **settled** nursery carries between two collections — the `K` of the
/// `collections == 1 + N / K` relation both growth suites pin, derived from the run's own
/// measured peak nursery capacity.
///
/// The capacity is a *measurement*, not a constant: `beam_gc`'s `HeapPolicy::young_after_collection`
/// **keeps** any capacity inside `[young_minor_target_words(live), YOUNG_SHRINK_FACTOR]` x that
/// live-set-derived target and lands on the target outside it, so
/// what a run settles on is whatever the growth seam installed for this workload's tiny live set.
/// What the cadence follows from is the arithmetic of that capacity: an iteration allocates
/// [`HEAP_GROWTH_ITERATION_WORDS`] words and the words a collection keeps alive here (the loop
/// index and its accumulator — a handful) are far below the `capacity mod footprint` slack, so
/// exactly `floor(capacity / footprint)` iterations fit after a collection and the next one
/// collects. At the measured **843**-word capacity that is `843 / 401 == 2`: two iterations
/// allocate 802 of the 843 words and the third asks for more than is left.
///
/// Floored at one: a capacity below a single footprint still carries one iteration (the
/// admission door collects, then that iteration fits).
fn heap_growth_iterations_per_nursery(peak_young_capacity: usize) -> u64 {
    (peak_young_capacity / HEAP_GROWTH_ITERATION_WORDS).max(1) as u64
}

/// The collections `heap_growth:run/1` takes over `n` iterations under
/// [`heap_growth_iterations_per_nursery`]'s cadence: one collection per `iterations_per_nursery`
/// iterations, plus the one the run takes before its first iteration (the nursery a fresh process
/// starts with is the collector's floor, smaller than a single 401-word request). Measured at
/// N = 1..10 as `1 + N / K` exactly, and at N = 5 000 / 10 000 / 25 000 / 50 000 / 100 000 as
/// 2 501 / 5 001 / 12 501 / 25 001 / 50 001.
fn heap_growth_expected_collections(n: i64, iterations_per_nursery: u64) -> u64 {
    (n as u64) / iterations_per_nursery + 1
}

/// **The headline evidence: on a growth-shaped workload the per-call counters are FLAT in N.**
///
/// `churn:run/1` is driven at three increasing iteration counts on a fresh runtime each time, so
/// its counters are per-run. The `foreign` regression claims the per-call cost *grows* with N;
/// if that were a real per-call work increase rather than a measurement artefact, then at least
/// one of these deterministic counters — collections per call, or slices per call — would grow
/// with N too. This test pins the truth for the churn shape: neither grows — both are flat,
/// because the nursery settles on a live-set-derived capacity (2 585 words on this shape) and every call
/// therefore performs the same allocation and collection work — so on this shape the excess wall
/// clock is **not** extra per-call GC/runner work.
#[test]
fn the_churn_workloads_per_call_counters_are_flat_in_n() {
    let rows = counter_table(churn_fixture(), "churn", &[2_000, 10_000, 40_000]);
    let (n1, r1) = rows[0];
    let (n2, r2) = rows[1];
    let (n3, r3) = rows[2];

    // The collector really ran (the loop is allocation-heavy), so a flat ratio is a measured
    // fact, not the vacuous "nothing happens" of a declined collection.
    assert!(r1.collections > 0, "churn must collect: {r1:?}");
    assert!(r3.collections > r1.collections, "more iterations ⇒ more collections: {r3:?}");

    // The per-call ratios must be flat: the widest run's per-call counters may not exceed the
    // narrowest's by more than a small slack (the nursery settles on its live-set-derived
    // capacity, so the per-call ratios are the same at every N).
    let collections_per_call_ratio =
        (r3.collections as f64 / (n3 + 1) as f64) / (r1.collections as f64 / (n1 + 1) as f64);
    assert!(
        collections_per_call_ratio <= 1.5,
        "collections/call must not grow with N (it is flat or falling): \
         N={n1} {:.6}/call, N={n3} {:.6}/call, ratio {collections_per_call_ratio:.3}",
        r1.collections as f64 / (n1 + 1) as f64,
        r3.collections as f64 / (n3 + 1) as f64,
    );
    let slices_per_call_ratio =
        (r3.slices as f64 / (n3 + 1) as f64) / (r1.slices as f64 / (n1 + 1) as f64);
    assert!(
        slices_per_call_ratio <= 1.5,
        "slices/call must not grow with N (it is flat or falling): \
         N={n1} {:.6}/call, N={n3} {:.6}/call, ratio {slices_per_call_ratio:.3}",
        r1.slices as f64 / (n1 + 1) as f64,
        r3.slices as f64 / (n3 + 1) as f64,
    );
    // The second run's ratios must sit between the two (no non-monotonic blow-up either).
    let mid = r2.collections as f64 / (n2 + 1) as f64;
    assert!(
        mid <= 1.5 * r1.collections as f64 / (n1 + 1) as f64,
        "the middle N must not blow up either: {mid:.6}/call"
    );
}

/// **The mechanism control: the engine-native growth shape is *collected*, not grown, at exactly
/// one collection per settled-nursery's worth of iterations — with flat per-call counters.**
///
/// `heap_growth:run/1` repeats a single 401-word `test_heap` allocation — far above a fresh
/// process's nursery (233 words) — so it reaches the engine's full heap-admission door
/// (`RuntimeContext::ensure_heap_full` → `Context::collect_or_grow`) on every iteration whose
/// request the settled nursery cannot hold, with **no** registered-BIF result copy-out involved.
/// That makes it the control for the `churn` shape: `churn` drives the BIF-result copy path,
/// `heap_growth` drives the pure engine-allocation path, and if the collection-debt /
/// retired-nursery mechanism were the source of an N-dependent per-call cost, this shape is where
/// it would show.
///
/// Its iteration counts are chosen so that **every** run spans several reduction slices (the
/// only window a live process's collector is readable from outside the runner), unlike a small
/// run that the native baseline tier completes inside a single slice.
///
/// # The count is `collections == 1 + N / K`, and it is *derived*
///
/// `K` is how many iterations the shape's **settled** nursery carries between two collections,
/// derived from the run's own measured peak young capacity rather than quoted
/// ([`heap_growth_iterations_per_nursery`]): it measured **2** on every row, i.e.
/// `collections == 1 + N / 2`.
///
/// * **one collection per `K` iterations — the heap-admission door.** A 401-word request fits the
///   settled nursery `K` times (`2 x 401 = 802` of its 843 words) and the next one cannot, so
///   `collect_or_grow` collects on that iteration and `HeapPolicy::young_after_collection`
///   **keeps** the same capacity (it never grows a nursery, so the settled figure comes from the
///   growth seam and the fullsweep, not from the collection). That is also why the boundary-sampled
///   retired chain stays at one space (`peak_young_spaces == 1`) and no boundary observes an
///   un-settled debt (`debt_boundaries == 0`).
/// * **the `+ 1`: one collection before the first iteration.** A fresh process starts on the
///   collector's floor (233 words), smaller than a single 401-word request, and the set of
///   collections is that one plus one at every `K`-th iteration — measured `1 + N / K` exactly
///   from N = 1 to N = 10 (1, 2, 2, 3, 3, 4, 4, 5, 5, 6) as well as on the five rows below.
///
/// The **boundary door adds nothing to the count**: a slice prices exactly 800 iterations
/// (`reductions == 5N + 5` against a 4 000-unit budget) and 800 is a multiple of `K`, so each
/// slice-end boundary falls on a cycle boundary and any collection it takes is one the following
/// admission would have taken anyway. Measured (this host, debug build) at N = 5 000 / 10 000 /
/// 25 000 / 50 000 / 100 000: `collections` = 2 501 / 5 001 / 12 501 / 25 001 / 50 001 against
/// `yields` = 6 / 12 / 31 / 62 / 125, at an 843-word nursery capacity with `major` 0 and
/// `debt_boundaries` 0 on every row.
///
/// # What would make it regress
///
/// * a `beam_gc` sizing change that moved the settled capacity: `K` moves with it and the expected
///   count moves too, so the relation re-derives rather than breaking silently — but a capacity
///   that no longer carries two iterations is a behaviour change worth reading;
/// * a growth arm re-appearing (an iteration taking the no-relocation `grow_nursery` arm instead of
///   collecting): the retired chain would rise above one space and a boundary would observe
///   collection debt;
/// * a change to the fixture's `test_heap` request: `K` moves with the footprint;
/// * a slice-granularity change (`RUN_STEP_SLICES`, `DEFAULT_REDUCTIONS_PER_SLICE`) that made a
///   slice price a non-multiple of `K` iterations — a boundary collection would then land
///   mid-cycle and add to the count.
#[test]
fn the_heap_growth_workloads_per_call_counters_are_flat_in_n() {
    // Five N, spanning a slice boundary count from 2 to 26: the relation is checked on every row
    // rather than at two endpoints, so a change that only bites in the mid range still shows.
    const COUNTS: [i64; 5] = [5_000, 10_000, 25_000, 50_000, 100_000];
    let rows = counter_table(heap_growth_fixture(), "heap_growth", &COUNTS);
    let (n1, r1) = rows[0];
    let (n2, r2) = rows[1];
    let (n3, r3) = rows[3];
    let (_, last) = rows[4];

    // The relation, exact, on every measured row, and *derived*: `K` iterations per settled
    // nursery (from that row's own peak capacity) plus the one collection before the first
    // iteration. Never calibrated to a quoted count.
    for &(n, row) in &rows {
        let iterations_per_nursery = heap_growth_iterations_per_nursery(row.peak_young_capacity);
        assert_eq!(
            iterations_per_nursery, 2,
            "N={n}: a {}-word settled nursery must carry two {}-word iterations — that is what \
             the run actually did (measured at every row): {row:?}",
            row.peak_young_capacity, HEAP_GROWTH_ITERATION_WORDS
        );
        assert_eq!(
            row.collections,
            heap_growth_expected_collections(n, iterations_per_nursery),
            "N={n}: expected 1 + N/K = 1 + {n}/{iterations_per_nursery} = {} collections (one per \
             {iterations_per_nursery} iterations plus the one before the first), got {}: {row:?}",
            heap_growth_expected_collections(n, iterations_per_nursery),
            row.collections
        );
    }
    // ... and the shape really is collected rather than grown: no iteration retires a nursery
    // space through the no-relocation growth arm, so the boundary-sampled chain stays at one
    // space and no boundary observes a debt the door would have to settle. (This is the inverse
    // of what the retired-chain check asserted before `beam_gc`'s fullsweep-nursery-sizing fix:
    // with the C's two-step `adjust_after_fullsweep` rule the door *succeeds* every iteration.)
    for &(n, row) in &rows {
        assert_eq!(
            (row.peak_young_spaces, row.debt_boundaries),
            (1, 0),
            "N={n}: the growth shape must be collected, not grown (no retired space, no \
             un-settled debt): {row:?}"
        );
    }
    assert!(r3.collections > r1.collections, "more iterations ⇒ more collections: {r3:?}");

    let collections_per_call_ratio =
        (r3.collections as f64 / (n3 + 1) as f64) / (r1.collections as f64 / (n1 + 1) as f64);
    assert!(
        collections_per_call_ratio <= 1.5,
        "collections/call must not grow with N: N={n1} {:.6}/call, N={n3} {:.6}/call, ratio \
         {collections_per_call_ratio:.3}",
        r1.collections as f64 / (n1 + 1) as f64,
        r3.collections as f64 / (n3 + 1) as f64,
    );
    let slices_per_call_ratio =
        (last.slices as f64 / (last.value + 1) as f64) / (r1.slices as f64 / (n1 + 1) as f64);
    assert!(
        slices_per_call_ratio <= 1.5,
        "slices/call must not grow with N: N={n1} {:.6}/call, N={} {:.6}/call, ratio {:.3}",
        r1.slices as f64 / (n1 + 1) as f64,
        last.value,
        last.slices as f64 / (last.value + 1) as f64,
        slices_per_call_ratio,
    );
    let mid = r2.collections as f64 / (n2 + 1) as f64;
    assert!(
        mid <= 1.5 * r1.collections as f64 / (n1 + 1) as f64,
        "the middle N must not blow up either: {mid:.6}/call"
    );
}

/// The debt-gated seam decline keeps the retired-nursery **chain** bounded across a long
/// fun-applying workload: ten times the iterations must not buy the process more retained
/// spaces.
///
/// # What this forbids, and why that is the right bound
///
/// `lists:foldl/3` (like every `lists`/`maps` key `beam_bifs::needs_fun_caller` names) is a
/// **host-served** fast path, so the engine reaches it through
/// `RuntimeContext::resolve_external_call`. With that seam always declining, the key ran
/// synchronously as a `beam_bifs` BIF and applied the caller's fun through
/// [`crate::context::bridges`]'s `FunBridge::nested` → `apply_fun` → `pump::run_label_blocking`
/// — a **second machine** whose context is `gc_armed == false` and whose process is marked
/// relocation-exempt for the whole application. Nothing can collect while such a run is live:
/// the nested machine's admission points are unarmed, and the armed doors refuse an exempt
/// process. That nested run's registered-BIF copy-outs meanwhile keep growing the process's
/// young heap through the **no-collect** `Process::grow_nursery` seam, and `beam_gc`'s
/// `Nursery::grow` *retires* the space it replaces instead of freeing it — a chain of retired
/// spaces that no collection ever collapses (the shape that made one in-process
/// `compile:file/2` report 20 retired spaces and 3.0 GB of chain-summed capacity).
///
/// The bound asserted here is therefore that **the chain's length must not be a function of the
/// work done**: `10 × N` iterations must retire no more spaces than `N` (one space of slack for
/// where a slice boundary happens to fall). That is precisely the "non-collecting growth whose
/// refusal nothing ever revisits" mechanism above — should it return, the larger run necessarily
/// retires more spaces, because only a collection can collapse the chain and the exempt nested
/// run forbids one. The workload is real work either way: the row's `value` grows with `N` and
/// its `slices` grow with it, so a fixture that quietly stopped applying its fun would fail the
/// last two assertions instead of passing the bound vacuously.
///
/// # The workload, the N pair, and why the bound is not vacuous at this size
///
/// `nestfold:run/1` folds a fun over `lists:seq(1, N)` that builds one `integer_to_binary/1`
/// per iteration and measures it with `byte_size/1` — a registered-BIF copy-out per iteration
/// (the growth input the mechanism above needs), applied through the nested
/// `FunBridge` run (the relocation-exempt window it happens in). The measured pair is
/// `N = 2 000` vs `N = 20 000`, **not** the original `200` vs `2 000`: at `N = 200` the whole
/// fold is 1 007 reduction units, so it finishes inside the run's **first** slice and no slice
/// boundary exists to sample the chain at (`peak_young_spaces` reads `0`, i.e. *unobserved*);
/// both members of the pair below end **several** slices, so the peak is a real maximum over
/// the run.
///
/// The pair still exercises the mechanism, on measured counters rather than on size:
///
/// * the growth seam is driven **many** times in both rows — the sampled `peak_young_capacity`
///   rises `17 964` → `196 883` words across the pair (`11 ×` on `10 ×` the work), i.e. the
///   process's nursery is repeatedly grown by the nested copy-outs;
/// * each row takes exactly **one** collection (a fullsweep, `collections = 1`, `major = 1`), and
///   that single collapse is what keeps the retained chain at **one** space in both rows
///   (`peak_young_spaces = 1`, no boundary left owing a debt) — so the gate is exercised *and*
///   succeeding, not bypassed by an idle workload;
/// * with the gate declining (the always-exempt nested run, nothing armed inside it), none of
///   those many growths is ever collapsed, so the chain's length tracks the number of growths —
///   which rises with `N`. The pre-gate shape measured 20 retired spaces / 3.0 GB of chain-summed
///   capacity for one in-process `compile:file/2`. The `+ 1` therefore absorbs only where a slice
///   boundary falls, never a second growth regime.
///
/// Measured cost of the whole test in the test profile (`optimized + debuginfo`): **~0.3 s** end
/// to end — `finished in 0.25–0.45 s` over repeated solo runs, of which ~0.2 s is the harness's
/// one `erlc` spawn and ~0.02 s each the two runtime boots, the two measured runs themselves
/// being ~6 ms (`N = 2 000`) and ~22 ms (`N = 20 000`). It therefore runs in the default gate.
/// The reason it was `#[ignore]`d as ">300 s at N = 2000" was its **fixture**, not the
/// fold: `length/1` takes a list, so the original body
/// `Acc + length(integer_to_binary(X * 3))` raised `badarg` on the first iteration (the oracle
/// raises it too), and a raising process never resolves `Runtime::process_result` (a raise is
/// recorded by `Runtime::process_exit`) — so the sampling loop spun on empty slices (measured:
/// 50 million slices in 20 s, the process already gone from the table) until its 300 s
/// `COUNTER_RUN_DEADLINE`. `byte_size/1` is the length measure a binary has.
///
/// The rows, printable with `--nocapture`:
///
/// ```text
/// nestfold_chain small N=2000  : slices=3  reductions=10007   collections=1 major=1 peak_spaces=1 peak_cap=17964   debt_bounds=0 value=7631
/// nestfold_chain big   N=20000 : slices=26 reductions=100007  collections=1 major=1 peak_spaces=1 peak_cap=196883  debt_bounds=0 value=96298
/// ```
#[test]
fn a_long_fun_applying_workload_does_not_chain_retired_nurseries() {
    // `SMALL_N` must end more than one slice (otherwise its chain peak is unobserved, not
    // measured) and both rows must reach their first collection; see the doc comment above.
    const SMALL_N: i64 = 2_000;
    const BIG_N: i64 = 20_000;
    let source = "\
-module(nestfold).
-export([run/1]).
run(N) ->
    lists:foldl(fun(X, Acc) -> Acc + byte_size(integer_to_binary(X * 3)) end, 0, lists:seq(1, N)).
";
    let Some(dir) = compile_modules("nestfold_chain", &[("nestfold", source)]) else {
        eprintln!("SKIP: no system `erlc` on PATH -- the nursery-chain bound test is skipped");
        return;
    };
    let beam = dir.join("nestfold.beam");
    let beam = beam.to_str().expect("the scratch path is utf-8");
    let small = run_growth_counted(&mut growth_runtime(beam), "nestfold", SMALL_N);
    let big = run_growth_counted(&mut growth_runtime(beam), "nestfold", BIG_N);
    for (tag, n, row) in [("small", SMALL_N, small), ("big", BIG_N, big)] {
        eprintln!(
            "nestfold_chain {tag} N={n} : slices={} reductions={} collections={} major={} \
             peak_spaces={} peak_cap={} mature_cap={} mature_used={} debt_bounds={} value={}",
            row.slices,
            row.reductions,
            row.collections,
            row.major,
            row.peak_young_spaces,
            row.peak_young_capacity,
            row.peak_mature_capacity,
            row.peak_mature_used,
            row.debt_boundaries,
            row.value,
        );
    }
    assert!(
        big.value > small.value && big.slices > small.slices,
        "the workload must really run: N={SMALL_N} -> value {} / {} slices, N={BIG_N} -> value \
         {} / {} slices",
        small.value,
        small.slices,
        big.value,
        big.slices,
    );
    assert!(
        big.collections >= 1,
        "the ten-times-larger run must have collected at least once (N={SMALL_N}: {} \
         collections, N={BIG_N}: {})",
        small.collections,
        big.collections,
    );
    assert!(
        big.peak_young_spaces <= small.peak_young_spaces + 1,
        "the retired-nursery chain must not grow with the work: N={SMALL_N} peaked at {} \
         space(s) ({} words) and N={BIG_N} at {} space(s) ({} words), with {} of the sampled \
         slice boundaries still owing a collection at N={BIG_N} -- a chain that lengthens with N \
         is the non-collecting `Process::grow_nursery` growth whose refusal nothing revisits",
        small.peak_young_spaces,
        small.peak_young_capacity,
        big.peak_young_spaces,
        big.peak_young_capacity,
        big.debt_boundaries,
    );
}

// ---------------------------------------------------------------------------------------
// The park invariant: a window that SKIPS the root publication HOLDS the exemption
// ---------------------------------------------------------------------------------------
/// The committed busy-port fixture (see `tests/fixtures/port_suspend.erl`): `command/1` issues one
/// plain `port_command/2`, the one producer of a `BifOutcome::Suspend` on a busy port.
const PORT_SUSPEND_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/port_suspend.beam"
);

/// **The one park entry point keeps both halves of the invariant together.**
///
/// A park either writes the machine's whole root set into the process (`publish_parked_roots`)
/// or it skips that hand-off — and skipping is sound only when nothing may relocate the heap
/// for the window's whole duration, i.e. when the process holds its relocation exemption. So
/// `exempt` and "published" are one decision, and [`park_window`] is where it is made: the
/// exemption is written from the same value that picks the branch, in **both** directions.
///
/// A park arm that passed `exempt = true` to the publication half without taking the exemption
/// — the `Suspend` arm's shape before the entry point existed — left the parked machine (the
/// only holder of the process's live `x`/stack once the slice is over) collectable with its
/// roots unpublished. [`ParkExemption::LiveFloats`] is the whole decision surface, so the two
/// combinations below are the whole set of branches:
///
/// * `LiveFloats` with a **dead** FR prefix → publish, hold nothing;
/// * `LiveFloats` with a **live** prefix → skip, hold the exemption.
///
/// Case (3) pins the *other* half of what a publication covers, on the shape a `receive` park
/// (the overwhelmingly common delivery target) really has: a publishing window hands over the
/// engine's host-held families as well as the machine's root files, so the `extra_roots` family
/// is non-empty afterwards — and, being a publishing window, it holds no exemption, which is what
/// makes a delivery into that receiver collect and grow its nursery.
///
/// And a publishing park after an exempt one **clears** the flag: a stale `true` would make the
/// very collections the publication exists for refuse the move.
#[test]
fn park_window_holds_the_exemption_exactly_when_it_skips_the_publication() {
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    spawn_preempting_loop(&mut state, pid, JitMode::Interpreter);
    let engine = state.engines.get_mut(&pid).expect("the engine is registered");
    reset_park_publish_counters();

    // (1) `LiveFloats` with a **dead** FR prefix: the window publishes its whole root set, so it
    // must hold nothing — a collection may relocate whatever it published.
    assert!(
        park_window(&mut table, pid, engine, 0, ParkExemption::LiveFloats),
        "a dead FR prefix publishes the root set"
    );
    assert!(
        !table.is_relocation_exempt(pid).expect("the pid is live"),
        "a publishing window must not hold the exemption: the collection it exists for would \
         refuse the move"
    );
    assert!(
        engine.parked_published,
        "the engine records that its roots really were handed over"
    );
    assert_eq!(
        park_publish_counters(),
        ParkPublishCounters {
            published: 1,
            skipped: 0
        }
    );

    // (2) `LiveFloats` with a **live** prefix: the machine keeps those FR slots as roots of its
    // own for the window, so nothing may move the heap — the publication is skipped **and** the
    // exemption is held, from the one value that decided the skip.
    assert!(
        !park_window(&mut table, pid, engine, 1, ParkExemption::LiveFloats),
        "a live FR prefix skips the publication"
    );
    assert!(
        table.is_relocation_exempt(pid).expect("the pid is live"),
        "a skipping park MUST hold the exemption for its window — this is the invariant the \
         `Suspend` arm broke when the two halves were separate calls"
    );
    assert!(!engine.parked_published);
    assert_eq!(
        engine.fr_published, 1,
        "the live FR prefix is recorded either way, so the resume replays exactly it"
    );
    assert_eq!(
        park_publish_counters(),
        ParkPublishCounters {
            published: 1,
            skipped: 1
        }
    );

    // (3) Back to a **dead** FR prefix from the exempt window: the flag is CLEARED, never
    // inherited, and the publication covers more than the machine's root files — the engine's
    // own host-held family (its first-run arguments, still live on a never-run engine) lands in
    // the process's `extra_roots`. This is the `receive` park's shape, and holding no exemption
    // is exactly what lets a delivery collect and grow that receiver's nursery.
    assert!(
        park_window(&mut table, pid, engine, 0, ParkExemption::LiveFloats),
        "a dead FR prefix publishes again"
    );
    assert!(
        !table.is_relocation_exempt(pid).expect("the pid is live"),
        "a publishing park clears the stale exemption, so its own collections are not refused"
    );
    assert_eq!(
        table.get(pid).expect("the process").registers().extra_roots(),
        &[make_small(PREEMPT_N)][..],
        "the publication carries the engine's host-held families too: the never-run engine's \
         first-run argument is the one extra root"
    );
    assert_eq!(
        park_publish_counters(),
        ParkPublishCounters {
            published: 2,
            skipped: 1
        }
    );

    // (4) A live FR prefix on that same engine skips again, and holds the exemption again.
    assert!(
        !park_window(&mut table, pid, engine, 1, ParkExemption::LiveFloats),
        "a live FR prefix skips the publication whatever the previous window did"
    );
    assert!(
        table.is_relocation_exempt(pid).expect("the pid is live"),
        "a skipping park always holds the exemption — the invariant, in both directions"
    );
    assert_eq!(
        park_publish_counters(),
        ParkPublishCounters {
            published: 2,
            skipped: 2
        }
    );
}

/// **The `Suspend` (BIF-park) arm holds the window's exemption when it skips the publication.**
///
/// The arm parks the machine on the unapplied BIF instruction and, when the slice ended with a
/// **live** float-register prefix, skips the root hand-off — so the parked machine is the whole
/// root set for the window and every relocation site must refuse the move. The arm used to pass
/// `exempt` to the publication half **alone**, so the very next `Runtime::serve_gc_request` could
/// collect the space the machine's live `x`/stack words name and the resume would run on pointers
/// into the abandoned from-space.
///
/// The FR prefix is what selects the skipping branch, and it is **sticky** for the engine's whole
/// life (`Machine::fr_mark` is cleared only by `Machine::reset`, which only a first entry runs), so
/// any process that ever touched a float register takes it on every later park. The test therefore
/// reaches it the way the runtime does: one slice that pre-empts before the BIF (a budget of one
/// instruction), a float register written between the slices exactly as a float operation leaves
/// it, then a full-budget slice that reaches `port_command/2` on a busy port — the one producer of
/// `BifOutcome::Suspend` — and parks there.
#[test]
fn a_suspend_park_with_a_live_float_prefix_holds_the_window_exemption() {
    /// The verb the registered fixture driver treats as the C driver's `$N` clear.
    const CLEAR: u32 = 78;
    let mut rt = runtime();
    // Busy from the start: the first `port_command/2` attempt meets the C's busy arm.
    let (port, _flag, _writes) = crate::port::test_support::register_busy_control_port(
        &mut rt,
        "suspend_park_exempt",
        vec![crate::port::test_support::ControlVerb::clearing(CLEAR)],
        true,
        false,
    );
    rt.code_server
        .load_path(PathBuf::from(PORT_SUSPEND_BEAM))
        .expect("the committed port_suspend fixture loads");
    let entry = rt
        .resolve_export_entry("port_suspend", "command", 1)
        .expect("port_suspend:command/1 resolves");
    let mut state = RunnerState::default();
    // The interpreter tier charges the budget once per instruction, which is what makes a
    // one-reduction slice end before the `port_command/2` call.
    state.jit_mode = Some(JitMode::Interpreter);
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    state.spawn(pid, entry, vec![port.to_eterm()]);

    // Slice 1: the one-reduction budget pre-empts the machine before it reaches the BIF, so the
    // FR file is still untouched and the park publishes its (empty) prefix.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1)
            .expect("the pre-empting slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Yield,
        "a one-reduction budget pre-empts on the first charge"
    );
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the engine is parked")
            .fr_published,
        0,
        "a float-free first slice parks a dead FR prefix"
    );

    // Between the slices: a live float register, which is exactly the engine state any float
    // operation leaves behind. `fr_mark` is sticky, so from here the engine carries a live prefix.
    {
        let engine = state.engines.get_mut(&pid).expect("the engine is parked");
        engine
            .machine
            .set_fr(0, make_small(42))
            .expect("FR 0 is inside the 1024-slot file");
        assert_eq!(
            engine.machine.fr_mark(),
            1,
            "the float register is live, so the next park must skip the publication"
        );
    }
    reset_run_slice_counters();
    reset_park_publish_counters();

    // Slice 2: the machine resumes and reaches `port_command/2`, which parks on the unapplied
    // instruction with a LIVE FR prefix — the skipping branch.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 10_000)
            .expect("the suspending slice runs")
    };
    assert_eq!(
        outcome,
        ProcOutcome::Yield,
        "a BIF suspend re-enqueues the process instead of letting it sleep"
    );
    assert!(
        run_slice_counters().suspends >= 1,
        "the slice must end at the BIF-suspend arm, not at a pre-emption"
    );
    let engine = state.engines.get(&pid).expect("the parked engine");
    assert!(
        !engine.parked_published,
        "a live FR prefix skips the root publication"
    );
    assert_eq!(
        engine.fr_published, 1,
        "the live FR prefix is recorded so the resume replays exactly it"
    );
    assert_eq!(
        park_publish_counters(),
        ParkPublishCounters {
            published: 0,
            skipped: 1
        },
        "the suspend park really took the skipping branch"
    );
    assert!(
        table.is_relocation_exempt(pid).expect("the pid is live"),
        "a suspend park that SKIPPED the root publication must hold the relocation exemption: \
         the parked machine is the process's whole root set for the window, so a collection here \
         (the very next `serve_gc_request`) would evacuate the words its live x/stack name"
    );
}

/// **A served `garbage_collect` request on a `Yield` park is not also handed to the boundary
/// door.**
///
/// The `Yield` arm parks the machine (publishing its whole root set for a dead FR prefix) and then
/// has two possible collectors: the pending `erlang:garbage_collect/0` request, and the
/// reduction/yield-boundary door. The request runs **first** and relocates the process's registers
/// in place — after which the machine's own `x`/`stack` words are **pre-collection**, reconciled
/// only by the next slice's `resume_from_process_roots`. The door begins by re-publishing exactly
/// those machine words into the process (`publish_roots_full`'s `copy_from_slice`), so running it
/// after a served request would resurrect dead pointers and then relocate over them.
///
/// The structural pin is that the door is never **asked** in that case: one `Yield` park that
/// served the request, zero boundary checks. The control half — the same loop with no pending
/// request — shows the door is still asked exactly once, so the skip is request-driven rather than
/// a blanket disable.
#[test]
fn a_served_gc_request_is_not_also_handed_to_the_boundary_door() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    spawn_preempting_loop(&mut state, pid, JitMode::Interpreter);

    // The request `erlang:garbage_collect/0` records through the runtime-native floor.
    rt.request_gc(pid);
    reset_boundary_door_counters();
    let collections_before = rt.gc_stats().collections;
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 2)
            .expect("the pre-empting slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Yield, "the budget pre-empts the loop");
    assert!(
        rt.gc_stats().collections > collections_before,
        "the pending request really collected over the just-published root set"
    );
    let requested = boundary_door_counters();
    assert_eq!(
        requested.checks, 0,
        "the boundary door was never asked: its opening re-publication would write the machine's \
         PRE-collection words over the registers the request just relocated"
    );
    assert_eq!(
        requested.request_collected, 1,
        "the served request is what decides the door's fate on this park"
    );
    assert_eq!(requested.collections, 0, "the door ran no collection of its own");

    // Control: the same park WITHOUT a pending request does hand the door its check.
    let control = table.spawn_default().expect("a second process");
    spawn_preempting_loop(&mut state, control, JitMode::Interpreter);
    reset_boundary_door_counters();
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(control, &mut table, 2)
            .expect("the control slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Yield);
    let plain = boundary_door_counters();
    assert_eq!(
        plain.request_collected, 0,
        "nothing collected on the control park's behalf"
    );
    assert_eq!(
        plain.checks, 1,
        "a publishing Yield park with no served request still hands the door its check"
    );
}

/// **The park census, and the parked-machine door, are *true* about park state.**
///
/// The two doors are the region-reclaim pass's whole view of a process that is not running: the
/// value/moving enumerations come from
/// [`RunnerState::for_each_parked_machine_root_mut`](crate::runner::RunnerState::for_each_parked_machine_root_mut),
/// and the `BEAM_RT_STORE_TRACE` holder diagnostic resolves a `ParkedMachine[i]` corpus slot back to
/// a pid through
/// [`RunnerState::for_each_engine_census`](crate::runner::RunnerState::for_each_engine_census).
/// Neither had a test, so this one pins all three claims they rest on:
///
/// 1. an engine parked in a window that **skipped** the hand-off (`parked = Some(..)`,
///    `parked_published = false` — a live FR-prefix park) **is** handed over by the
///    parked-machine door, and the words it hands over are exactly the ones the census reports
///    for that pid;
/// 2. an engine that is **not** in a park window (`parked = None`, a process spawned and never run)
///    appears in the census as such — and contributes nothing to the door, whose three slices are
///    empty for it;
/// 3. a window whose roots went to the process's own registers (`parked_published = true`, what a
///    `receive` park with a dead FR prefix does) is **skipped** by the door even though its
///    machine still holds the same words: the published copy is the truth for that window, and
///    handing the machine's copy over as well would be a second, stale root set.
///
/// Claim 2 is why the door's gate is *publication* rather than `parked.is_some()`: a never-run
/// engine is unpublished with nothing to hand over, so it is a no-op rather than a skip. The
/// observable difference between the two is exactly what the counts below assert, and it is the
/// property the move's corpus completeness rests on.
#[test]
fn the_park_census_and_the_parked_machine_door_agree_with_the_park_state() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module = Arc::new(beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam"));
    let function = module
        .atom_table
        .lookup("wait_forever")
        .expect("wait_forever is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module: module.clone(),
            function,
            arity: 0,
        },
        Vec::new(),
    );
    // The control: a registered engine that has **never run**, i.e. not in a park window.
    let idle = table.spawn_default().expect("a second process");
    state.spawn(
        idle,
        ProcessEntry::Export {
            module,
            function,
            arity: 0,
        },
        Vec::new(),
    );

    // One slice parks the receive: `parked = Some(..)` and — a `receive` park with a dead FR
    // prefix — the machine's root set was published, so the process's registers are the truth
    // for that window.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the bare receive parks");
    assert!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .parked_published,
        "a `receive` park whose FR prefix is dead publishes its root set"
    );

    // A live root word on the parked machine, so a visit is **observable**: a `{7,8}` tuple built
    // on the process's own young heap and published as FR slot 0 through the engine's `set_fr`
    // seam (which raises `fr_mark` to 1).
    let live = {
        let process = table.get_mut(pid).expect("the parked process");
        let tuple = process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds {7,8}");
        state
            .engines
            .get_mut(&pid)
            .expect("the parked engine")
            .machine
            .set_fr(0, tuple)
            .expect("slot 0 is in the FR file");
        tuple
    };
    // Re-park the same machine through the production entry point with that live FR prefix: the
    // run's own post-slice reaction would do exactly this, and it is the **un-published** window
    // shape the parked-machine door exists for (a live FR prefix is the one thing no publication
    // can carry, so such a window skips the hand-off and holds the exemption instead).
    {
        let engine = state.engines.get_mut(&pid).expect("the parked engine");
        assert!(
            !park_window(&mut table, pid, engine, 1, ParkExemption::LiveFloats),
            "a live FR prefix skips the publication"
        );
    }

    // ---- 1. the census reports the park state, per engine ------------------------------
    let mut parked_row = None;
    let mut idle_row = None;
    state.for_each_engine_census(&mut |row| {
        if row.pid == pid {
            parked_row = Some((row.parked, row.parked_published, row.fr_words, row.roots));
        } else if row.pid == idle {
            idle_row = Some((row.parked, row.parked_published, row.roots.len()));
        }
    });
    let (parked, published, fr_words, roots) =
        parked_row.expect("the parked engine is in the census");
    assert!(
        parked.is_some(),
        "the receive park is a park window, and the census says at which (func, pc)"
    );
    assert!(
        !published,
        "a live-FR park skipped the hand-off, so the machine is authoritative for the window"
    );
    assert_eq!(fr_words, 1, "the live FR prefix the park would replay is one word");
    assert!(
        roots.contains(&live),
        "the census reports the machine's live root words, the FR slot included"
    );
    let (idle_parked, _idle_published, idle_roots) =
        idle_row.expect("the never-run engine is in the census too");
    assert_eq!(
        idle_parked, None,
        "an engine that never ran is not in a park window"
    );
    assert_eq!(idle_roots, 0, "and it hands over no root word");

    // ---- 2. the door hands over exactly the parked engine's words ----------------------
    let mut handed = 0usize;
    let mut seen = false;
    state
        .for_each_parked_machine_root_mut(
            &mut |words| {
                handed += words.len();
                seen = seen || words.contains(&live);
                Ok(())
            },
            |candidate| candidate == pid || candidate == idle,
        )
        .expect("the parked-machine door completes");
    assert!(seen, "the parked machine's live FR word is handed over");
    assert_eq!(
        handed,
        roots.len(),
        "the door's word count is the census's for the parked engine, so the never-run engine \
         contributed nothing"
    );

    // ---- 3. a publishing window is skipped by the door ---------------------------------
    {
        let engine = state.engines.get_mut(&pid).expect("the parked engine");
        assert!(
            park_window(&mut table, pid, engine, 0, ParkExemption::LiveFloats),
            "a Yield park with a dead FR prefix publishes the machine's root set"
        );
    }
    let mut published_handed = 0usize;
    state
        .for_each_parked_machine_root_mut(
            &mut |words| {
                published_handed += words.len();
                Ok(())
            },
            |_| true,
        )
        .expect("the door completes over a publishing window");
    assert_eq!(
        published_handed, 0,
        "a window whose roots went to the process's registers is not handed over again — the \
         machine's copy is stale the moment a collection relocates the published one"
    );
    // The machine still holds the word, so the zero above is the gate's answer and not an empty
    // machine.
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .machine
            .get_fr(0)
            .expect("slot 0 is in the FR file"),
        live,
        "the machine still names the word the door declined to hand over"
    );
}
