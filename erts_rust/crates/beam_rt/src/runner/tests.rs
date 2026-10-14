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
use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_engine::context::RootSet;
use beam_engine::jit::cache::{baseline_cache_len, clear_baseline_cache};
use beam_engine::machine::WaitDeadline;
use beam_engine::{terms, Machine, Mailbox, MinimalContext, RunOutcome as EngineOutcome, Tier};
use beam_loader::{Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand};
use beam_process::{
    ProcessId, ProcessRunner, ProcessTable, RunOutcome as ProcOutcome, SignalAtoms,
    DEFAULT_REDUCTIONS_PER_SLICE,
};
use beam_term::{make_atom, make_small, signed_value, Atom, AtomTable, Eterm, NIL};

use super::budget::{BudgetContext, BudgetProbe};
use super::entry::{ProcessEntry, RunnerState};
use super::perf::record_baseline_code;
use super::reason::{exit_reason, normal};
use super::run::{publish_parked_roots, RuntimeRunner};
use super::tier::{mode_from_value, select_entry_tier, JitMode};
use crate::context::RuntimeContext;
use crate::runtime::Runtime;

fn runtime() -> Runtime {
    Runtime::with_defaults().expect("the default runtime")
}

/// A process table independent of the runtime (what the scheduler owns).
fn table() -> ProcessTable {
    ProcessTable::new(SignalAtoms::from_table(&mut AtomTable::new()))
}

#[test]
fn a_pid_without_an_engine_is_a_typed_error() {
    let mut rt = runtime();
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");

    let mut runner = RuntimeRunner::new(&mut rt, &mut state);
    let error = runner
        .run(pid, &mut table, 1)
        .expect_err("no engine ⇒ a typed error");
    assert_eq!(
        error.to_string(),
        crate::error::unknown_process_entry(pid).to_string()
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

/// **A running process comes to OWN the off-heap refc binaries its engine built.**
///
/// `erlang:process_info(Pid, binary)` reads its roster from `beam_process::Process`'s off-heap
/// table, whose only in-production producer is [`RuntimeRunner`]'s per-slice registration seam:
/// a payload built by compiled bitstring code is held by the machine's `Machine::refc_binaries`
/// reservoir, and the runner copies the whole reservoir onto the process's table at the end of
/// every slice. This pins **both** halves for a process parked at a `receive` **after** building
/// a runtime-sized (off-heap, above the 64-byte on-heap limit) binary: the machine's reservoir is
/// non-empty *and* the process's own roster is non-empty.
///
/// The fixture is deliberately a real compile of `<<0:N/unit:8>>` with a **variable** size, so
/// the compiler lowers it to `bs_create_bin` (unit 8 → `N` **bytes**, not `N` bits) →
/// [`make_binary`](beam_engine) → `alloc_refc_binary`, which is exactly the production route. The
/// reservoir assertion above the roster assertion is load-bearing: it fails loudly if the fixture
/// ever stops building off-heap, so the roster assertion can never be satisfied vacuously.
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
    let mut state = RunnerState::default();
    let mut table = table();
    let pid = table.spawn_default().expect("a fresh process");
    let module =
        Arc::new(beam_loader::load_file(dir.join("ohb.beam")).expect("load the compiled module"));
    let build = module
        .atom_table
        .lookup("build_and_park")
        .expect("build_and_park/1 is interned");
    state.spawn(
        pid,
        ProcessEntry::Export {
            module,
            function: build,
            arity: 1,
        },
        // 256 bytes > `ERL_ONHEAP_BINARY_LIMIT` (64), so the result is an off-heap refc binary.
        vec![make_small(256)],
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
        "the bare `receive` parks the process after it builds the binary"
    );

    // The fixture really builds off-heap: the machine's never-cleared reservoir holds the payload.
    let engine = state.engines.get(&pid).expect("the parked engine");
    assert!(
        !engine.machine.refc_binaries().is_empty(),
        "the fixture must build its binary off-heap (a runtime-sized `<<0:N/unit:8>>`, N = 256)"
    );

    // ...and the per-slice registration seam made the *process* come to own it.
    assert!(
        table
            .get(pid)
            .expect("the parked process")
            .off_heap_binary_count()
            >= 1,
        "the runner registers the machine's reservoir onto the process's own off-heap table"
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
/// callee in the calling machine, and the full GC door publishes that machine's whole FR file,
/// so the float registers it leaves live are relocated rather than dangled (see
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
}
// -- the process-wide default mode ----------------------------------------

/// The effective mode for an **unset** `BEAM_RT_TIER` is `Auto`, which selects the native
/// baseline tier for every process entry. `"baseline"` names that same default explicitly
/// and any unknown value falls back to the always-correct interpreter. Pinned through the
/// **pure** resolution helper ([`mode_from_value`]) rather than by mutating the process
/// environment (the crate is `#![deny(unsafe_code)]`), and the derived [`JitMode::default`]
/// must agree with it so no call site can pick a different tier by omission.
#[test]
fn the_unset_default_resolves_to_auto() {
    assert_eq!(
        mode_from_value(None),
        JitMode::Auto,
        "an unset BEAM_RT_TIER is the native-tier default"
    );    assert_eq!(
        mode_from_value(Some("interpreter")),
        JitMode::Interpreter
    );
    assert_eq!(
        mode_from_value(Some("auto")),
        JitMode::Auto,
        "`auto` names the default explicitly"
    );
    assert_eq!(mode_from_value(Some("baseline")), JitMode::Baseline);
    // An unknown value is the always-correct interpreter, never an error and never the JIT.
    assert_eq!(mode_from_value(Some("jit")), JitMode::Interpreter);
    assert_eq!(mode_from_value(Some("")), JitMode::Interpreter);
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

/// The whole runtime's **scheduler** path (not just one slice) runs `slice:fib/1 10` and
/// records `55` — the tier selection is wired into the real `RuntimeRunner` the scheduler
/// drives, on whatever mode the process environment selects (an unset `BEAM_RT_TIER` ⇒ the
/// baseline tier for `slice`). The test asserts the *result*, so it holds on every tier.
#[test]
fn the_scheduler_path_computes_fib_under_the_selected_tier() {    let mut rt = runtime();
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

/// Assert `relocated` is the two-element small-integer tuple `expected` **at a new address**: the
/// term really moved (a copied term never keeps its from-space address) and the copy is intact.
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
    let pid = table.spawn_default().expect("a fresh process");

    // The reason: a two-element tuple of small ints on the PROCESS's own young heap.
    let reason = {
        let process = table.get_mut(pid).expect("the spawned process");
        process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds {7,8}")
    };

    let mut machine = machine_with_pending_reason(reason);

    // Publish the parked machine's roots: the reason rides the process's extra roots.
    publish_parked_roots(&mut table, pid, &mut machine, 0);
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
        let mut ctx = RuntimeContext::for_process(&mut rt, &mut table, pid);
        ctx.resync_parked_roots(pid, &mut machine, 0);
    }
    assert_eq!(
        machine.pending_exception(),
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
    let pid = table.spawn_default().expect("a fresh process");
    let mut machine = Machine::new();
    assert_eq!(machine.pending_exception(), None, "a fresh machine has no raise");

    publish_parked_roots(&mut table, pid, &mut machine, 0);
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
    let pid = table.spawn_default().expect("a fresh process");
    let reason = {
        let process = table.get_mut(pid).expect("the spawned process");
        process
            .heap_mut()
            .tuple(&[make_small(7), make_small(8)])
            .expect("the young heap builds {7,8}")
    };
    let mut machine = machine_with_pending_reason(reason);

    publish_parked_roots(&mut table, pid, &mut machine, 0);
    assert_eq!(
        table.get(pid).expect("the process").registers().extra_roots(),
        &[reason][..],
        "the first publish roots the live reason"
    );

    // The raise is handled: the machine has no pending exception any more.
    machine.reset();
    assert_eq!(machine.pending_exception(), None, "the raise is gone after a reset");

    publish_parked_roots(&mut table, pid, &mut machine, 0);
    assert!(
        table.get(pid).expect("the process").registers().extra_roots().is_empty(),
        "the stale extra root is cleared, never left as a leftover/NIL placeholder"
    );
}

/// **The park arm keeps a waiting process relocation-exempt for its whole parked window.**
///
/// A signal to a *waiting* receiver whose heap cannot hold the payload must not relocate it: the
/// runner marks the pid relocation-exempt at the park arm (`set_state(Waiting)` +
/// `set_relocation_exempt(true)`) and clears the mark when the process is resumed. This is
/// **additional** to the installed host collection gate
/// ([`crate::context::gc_walk::RuntimeCollectionGate`]), which declines only when an unscanned
/// container is reachable from the receiver's GATHERED roots — here nothing foreign is reachable,
/// so the gate answers `false` and the park window's exemption is the sole decider. Removing the
/// park arm's `set_relocation_exempt(true)` lets the signal collect and grow the parked heap, and
/// this case then fails on its first assertion.
#[test]
fn a_parked_process_stays_relocation_exempt_for_its_parked_window() {
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
        table.is_relocation_exempt(pid).expect("the pid is live"),
        "the park arm marks the parked process relocation-exempt"
    );

    // The gate does NOT decline for this receiver: nothing foreign is reachable from its
    // gathered roots, so the park window's exemption — not the gate — is the deciding mechanism.
    {
        let process = table.get(pid).expect("the process is live");
        let mut gate = crate::context::gc_walk::RuntimeCollectionGate::new();
        assert!(
            !<crate::context::gc_walk::RuntimeCollectionGate as beam_process::CollectionGate>::declines_relocation(
                &mut gate,
                process
            ),
            "no foreign container is reachable from the parked process's gathered roots, so the \
             gate answers `false` — the exemption is what refuses the move"
        );
    }

    // A reason LARGER than the receiver's WHOLE nursery: only a decline keeps the on-enqueue copy
    // from collecting and growing the parked heap.
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
    assert_eq!(
        r.collector().minor_gcs(),
        minor_before,
        "the park window's exemption declined the signal-driven relocation"
    );
    assert_eq!(
        r.collector().young().base(),
        base_before,
        "a declined signal must not move or grow the parked process's nursery"
    );
    assert_eq!(r.mailbox().len(), 1, "the notification is NOT lost");
    assert!(
        r.is_relocation_exempt(),
        "the process stays relocation-exempt for the whole parked window"
    );

    // The resume clears the exemption, so the process's own admission point collects again.
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
        "the resume cleared the park window's exemption, so the process collects normally again"
    );
}

/// **A park of a float-free process does NOT publish any FR word.**
///
/// `publish_roots_full` *can* publish the machine's float-register file by appending its
/// `Eterm` slots to the process's Erlang stack — real cost on every park and every collection
/// that follows it. The runner bounds that to the machine's **live leading prefix**, read once
/// per slice from `Machine::fr_mark()`. For a process whose FR is entirely `NIL` (the
/// overwhelming majority: it never touched a float) `fr_mark() == 0`, so the park appends no FR
/// word at all and the same count pins nothing.
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
         (got {machine_stack_len}); the equality below would otherwise be vacuous"
    );
    let published = table
        .get(pid)
        .expect("the parked process")
        .registers()
        .stack()
        .len();
    assert_eq!(
        published, machine_stack_len,
        "a float-free park publishes only the machine's Erlang stack, never any FR word"
    );
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .fr_published,
        0,
        "the park published a zero-length FR prefix and recorded that count for the resume"
    );
    assert_eq!(
        rt.gc_stats().pinned,
        0,
        "a process whose FR prefix is empty is not pinned"
    );
}

/// **A park whose FR *is* live publishes exactly the live prefix (`fr_mark`) and pins the
/// process.**
///
/// The bounded-FR path must not weaken the live case: a process whose FR holds a term is
/// pinned (its FR can point into the heap, so its heap must never move) **and** its live FR
/// prefix must ride the process stack so a collection that does run — a `Yield`-window one, or
/// a delivery-driven one, neither gated by the pin — relocates it rather than leaving it on the
/// abandoned from-space.
///
/// A parked machine is not reset on resume, so an FR slot written through the engine's own
/// `Machine::set_fr` seam while the process is parked survives into the next slice and raises
/// the mark: setting slot 0 stands in for a float computation that left a register set (the
/// exact residue the post-slice `fr_mark` check exists to catch), and the *second* park then
/// observes one live FR slot. This pins both halves of the design — the published FR length is
/// the **one**-word live prefix, not the 1024-slot file, and `ProcessEngine::fr_published`
/// records that same count for the resume — without needing compiled float code.
#[test]
fn a_live_fr_is_published_by_the_park_and_pins_the_process() {
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

    // Slice 1: park with a dead FR — no FR word published, and the process is NOT pinned.
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
    // the park below must publish and record.
    state
        .engines
        .get_mut(&pid)
        .expect("the parked engine")
        .machine
        .set_fr(0, make_small(42))
        .expect("slot 0 is in the FR file");

    // Slice 2: resuming with an empty mailbox parks again — now with one live FR slot.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks again");

    assert_eq!(
        rt.gc_stats().pinned,
        1,
        "a live FR pins the process, so its heap is never moved again"
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
        published,
        machine_stack_len + 1,
        "a live-FR park publishes the machine's stack plus exactly the live FR prefix \
         (fr_mark = {fr_mark}), never the whole 1024-slot file"
    );
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .fr_published,
        fr_mark as usize,
        "the engine records the published prefix length, so the resume scatters exactly the region \
         the park published"
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
/// The park publishes one FR word (`fr_mark() == 1`), the collection moves the tuple, and the
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

    // Slice 2: resume (scattering slice 1's empty prefix), then park again — this time with the
    // one live FR word published onto the process stack.
    let outcome = {
        let mut runner = RuntimeRunner::new(&mut rt, &mut state);
        runner
            .run(pid, &mut table, 1_000)
            .expect("the resume slice runs")
    };
    assert_eq!(outcome, ProcOutcome::Wait, "the empty mailbox parks again");
    assert_eq!(
        state
            .engines
            .get(&pid)
            .expect("the parked engine")
            .fr_published,
        1,
        "the park published the one live FR slot"
    );

    // A genuine MOVING collection over the parked process: it gathers the process's stack
    // (which holds the published FR word at its end) and scatters the relocated words back.
    //
    // The `Wait` park arm left the process relocation-exempt for its parked window, and
    // `Process::collect` refuses an exempt process with the typed `relocation_exempt` raise — so
    // clear it first. That is exactly the `Yield` (pre-emption) window's situation: a pre-empted
    // process is *not* exempt, so its published FR prefix is the only thing that keeps a live
    // FR term across a collection taken while it sits in `RunnerState.engines`.
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
