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
use std::time::Duration;

use beam_common::{BeamError, ExitReason};
use beam_engine::jit::cache::{baseline_cache_len, clear_baseline_cache};
use beam_engine::{Machine, Mailbox, MinimalContext, RunOutcome as EngineOutcome, Tier};
use beam_loader::{Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand};
use beam_process::{ProcessId, ProcessRunner, ProcessTable, RunOutcome as ProcOutcome, SignalAtoms};
use beam_term::{make_atom, make_small, signed_value, Atom, AtomTable, Eterm};

use super::budget::BudgetProbe;
use super::entry::{ProcessEntry, RunnerState};
use super::perf::record_baseline_code;
use super::reason::{exit_reason, normal, parked_receive_timeout, timeout_delay};
use super::run::RuntimeRunner;
use super::tier::{jit_is_switch_safe, mode_from_value, select_entry_tier, JitMode};
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
    let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
    let machine = Machine::new();
    let (func, wait_index, scan) = finite_receive(&module, "after20");

    let (delay, resume) = parked_receive_timeout(&machine, &module, func, scan)
        .expect("after20 arms a 20 ms deadline");
    assert_eq!(delay, Duration::from_millis(20));
    assert_eq!(
        resume,
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
    let machine = Machine::new();
    let atom = module.atom_table.lookup("wait_forever").expect("wait_forever");
    let function = module.function(atom, 0).expect("wait_forever/0");
    assert!(
        !function.code.iter().any(|i| matches!(i, Instr::WaitTimeout { .. })),
        "a bare wait has no wait_timeout at all"
    );
    let scan = function
        .code
        .iter()
        .find_map(|instr| match instr {
            Instr::Wait { target } => Some(function.labels[target] as u32),
            _ => None,
        })
        .expect("the bare wait's resume label");
    let func = module.functions.iter().position(|f| f.name == atom).unwrap() as u32;
    assert_eq!(
        parked_receive_timeout(&machine, &module, func, scan),
        None,
        "an infinite receive never times out"
    );
}

#[test]
fn a_timeout_delay_must_be_a_statically_known_positive_millisecond_count() {
    let module = beam_loader::load_file(recv_after_fixture()).expect("recv_after.beam");
    let machine = Machine::new();
    // A literal positive delay is a deadline.
    assert_eq!(
        timeout_delay(&machine, &module, &Operand::Integer(20)),
        Some(Duration::from_millis(20))
    );
    // Zero and negative are not delays (the engine handles a bad/`0` time itself), and
    // neither is a register the machine cannot resolve — an honest "no deadline" rather
    // than a guessed one.
    assert_eq!(timeout_delay(&machine, &module, &Operand::Integer(0)), None);
    assert_eq!(timeout_delay(&machine, &module, &Operand::Integer(-1)), None);
    assert_eq!(
        timeout_delay(&machine, &module, &Operand::X(0)),
        None,
        "an unset x register is no delay"
    );
    assert_eq!(
        timeout_delay(&machine, &module, &Operand::Y(0)),
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
    assert_eq!(
        timeout_delay(&machine, &module, &Operand::Y(0)),
        Some(Duration::from_millis(20))
    );
    let (delay, resume) = parked_receive_timeout(&machine, &module, 0, 3)
        .expect("the y-register delay arms a 20 ms deadline");
    assert_eq!(delay, Duration::from_millis(20));
    assert_eq!(resume, (0, 9), "resume at the `timeout` after wait_timeout");
    assert_eq!(module.functions[0].code[9], Instr::Timeout);

    // ...and it fires: resuming at the recovered address runs the timed-out continuation.
    let fired = machine
        .resume_outcome(&module, resume.0, resume.1, &mut ctx)
        .expect("a fired deadline resumes at the timeout instruction");
    assert_eq!(fired, EngineOutcome::Returned(make_small(7)));

    // Bounds: the frame owns exactly one Y slot, and an out-of-range one is no deadline.
    assert_eq!(machine.y(1), None, "the frame has one Y slot");
    assert_eq!(timeout_delay(&machine, &module, &Operand::Y(1)), None);
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
    // parked — and the unreadable slot must therefore stay "no deadline", not become one.
    assert_eq!(outcome, EngineOutcome::Wait { func: 0, pc: 3 });
    assert_eq!(machine.y(0), Some(make_atom(infinity)));
    assert_eq!(timeout_delay(&machine, &module, &Operand::Y(0)), None);
    assert_eq!(parked_receive_timeout(&machine, &module, 0, 3), None);
}
// -- execution-tier selection (`super::tier`) -----------------------------

/// The committed engine harness fixture that calls **non-floor, non-registry** `.beam`
/// modules (`orddict`/`queue`), reused by relative path.
fn stdlib_harness_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_engine/tests/fixtures/stdlib_more_harness.beam"
    )
}

/// The committed engine fixture that uses `apply`/`apply_last`/`call_fun`, reused by
/// relative path.
fn funs_fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../beam_engine/tests/fixtures/funs.beam"
    )
}

/// A one-function synthetic module with the given `{module, function, arity}` imports and
/// body — for the predicate branches no committed fixture exercises. A `.beam` cannot be
/// written from this node, and the committed fixtures only cover the floor/registry/apply
/// cases; the IR is a public `beam_loader` surface, so a crafted module is the honest way
/// to pin the remaining branches.
fn crafted(imports: Vec<(Atom, Atom, u32)>, code: Vec<Instr>) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("crafted");
    for (import_module, import_function, arity) in imports {
        module.imports.push(Import {
            module: import_module,
            function: import_function,
            arity,
        });
    }
    let name = module.atom_table.intern("f");
    let module_atom = module.name;
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module_atom,
        name,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels: BTreeMap::new(),
    });
    module
}

/// A `call_ext_only Arity, ImportRef(0)` body hitting the module's first import.
fn call_ext_only(arity: u32) -> Vec<Instr> {
    vec![Instr::CallExtOnly {
        arity,
        target: ImportRef(0),
    }]
}

/// `slice.beam`'s only external calls are `erlang:get_module_info/1,2` — a floor namespace —
/// so the module is provably switch-free and the auto mode selects the baseline JIT. Its
/// arithmetic is `gc_bif`/`bif` (synchronous `call_bif`), not `call_ext`, so nothing else
/// touches the seam.
#[test]
fn the_slice_module_selects_the_baseline_tier() {
    let rt = runtime();
    let module = beam_loader::load_file(slice_fixture()).expect("slice.beam");
    assert!(
        jit_is_switch_safe(&rt, &module),
        "only the erlang floor is imported ⇒ no module-context switch can occur"
    );
    let mut state = RunnerState::default();
    let entry = ProcessEntry::Export {
        module: Arc::new(module),
        function: Atom::from_raw(0),
        arity: 2,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::BaselineJit
    );
    // The verdict is memoized under the module's name (the entry module is `slice`).
    assert_eq!(state.jit_safety.get("slice"), Some(&true));
}

/// A module that calls a **loadable `.beam`** outside the floor/registry/NIF sets
/// (`orddict`/`queue` here) can have its `call_ext*` entered in-machine by the seam
/// (`Ok(Some)` ⇒ `Step::Enter`), so the auto mode keeps it on the interpreter.
#[test]
fn a_module_calling_a_loadable_beam_keeps_the_interpreter() {
    let rt = runtime();
    let module = beam_loader::load_file(stdlib_harness_fixture()).expect("the harness loads");
    assert!(
        !jit_is_switch_safe(&rt, &module),
        "orddict/queue call targets are not provably synchronous"
    );
    let mut state = RunnerState::default();
    let entry = ProcessEntry::Export {
        module: Arc::new(module),
        function: Atom::from_raw(0),
        arity: 0,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::Interpreter
    );
}

/// A module with a dynamic `apply`/`apply_last` can never be proven switch-free: the
/// target is a register value, so the seam may enter it in-machine.
#[test]
fn a_module_using_apply_keeps_the_interpreter() {
    let rt = runtime();
    let module = beam_loader::load_file(funs_fixture()).expect("funs.beam");
    assert!(!jit_is_switch_safe(&rt, &module), "apply is dynamic");
    let mut state = RunnerState::default();
    let entry = ProcessEntry::Export {
        module: Arc::new(module),
        function: Atom::from_raw(0),
        arity: 0,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::Interpreter
    );
}

/// A `call_ext` into a **registered** `beam_bifs` key (`lists:sort/1`) is synchronous, so a
/// module whose only external call is registered stays switch-free.
#[test]
fn a_registered_call_target_is_provably_synchronous() {
    let rt = runtime();
    assert!(
        rt.registry.contains("lists", "sort", 1),
        "the fixture relies on lists:sort/1 being registered"
    );
    let mut module = crafted(Vec::new(), call_ext_only(1));
    let m = module.atom_table.intern("lists");
    let f = module.atom_table.intern("sort");
    module.imports.push(Import {
        module: m,
        function: f,
        arity: 1,
    });
    assert!(jit_is_switch_safe(&rt, &module));
}

/// The floor namespaces are synchronous too (the `erlang` BIF namespace is the common
/// case), so an import from one keeps the JIT available.
#[test]
fn a_floor_call_target_is_provably_synchronous() {
    let rt = runtime();
    let mut module = crafted(Vec::new(), call_ext_only(2));
    let m = module.atom_table.intern("erlang");
    let f = module.atom_table.intern("get_module_info");
    module.imports.push(Import {
        module: m,
        function: f,
        arity: 2,
    });
    assert!(jit_is_switch_safe(&rt, &module));
}

/// A `call_ext` whose import is **not** registered and **not** a floor name stays unsafe —
/// the seam would load the callee and enter it (`Step::Enter`).
#[test]
fn an_unregistered_import_keeps_the_interpreter() {
    let rt = runtime();
    let mut module = crafted(Vec::new(), call_ext_only(2));
    let m = module.atom_table.intern("proplists");
    let f = module.atom_table.intern("get_value");
    module.imports.push(Import {
        module: m,
        function: f,
        arity: 2,
    });
    assert!(!jit_is_switch_safe(&rt, &module));

    let mut state = RunnerState::default();
    let entry = ProcessEntry::Export {
        module: Arc::new(module),
        function: Atom::from_raw(0),
        arity: 0,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::Interpreter
    );
}

/// An import whose module/function atom is not in the module's own atom table cannot be
/// named, so its synchronicity cannot be proven — conservatively unsafe.
#[test]
fn an_import_with_an_unresolvable_atom_is_unsafe() {
    let rt = runtime();
    // `Atom::MAX` is never in a table, so `module.atom_name` cannot resolve it.
    let module = crafted(
        vec![(Atom::MAX, Atom::MAX, 1)],
        call_ext_only(1),
    );
    assert!(!jit_is_switch_safe(&rt, &module));
}

/// A `call_ext` whose target index is past the import table is unsafe (the loader would
/// report an out-of-range import; the predicate must not treat it as synchronous).
#[test]
fn a_call_ext_past_the_import_table_is_unsafe() {
    let rt = runtime();
    let module = crafted(
        Vec::new(),
        vec![Instr::CallExt {
            arity: 1,
            target: ImportRef(7),
        }],
    );
    assert!(!jit_is_switch_safe(&rt, &module));
}

/// The instruction's `arity` operand must agree with the import's arity — the seam is
/// handed the import's, so a mismatch is not trusted.
#[test]
fn an_arity_mismatch_between_instruction_and_import_is_unsafe() {
    let rt = runtime();
    let mut module = crafted(Vec::new(), vec![Instr::CallExtOnly {
        arity: 3,
        target: ImportRef(0),
    }]);
    let m = module.atom_table.intern("erlang");
    let f = module.atom_table.intern("get_module_info");
    module.imports.push(Import {
        module: m,
        function: f,
        arity: 2,
    });
    assert!(!jit_is_switch_safe(&rt, &module));
}

/// A module with **no** external call and no `apply` is trivially switch-free.
#[test]
fn a_module_with_no_external_call_is_safe() {
    let rt = runtime();
    let module = crafted(Vec::new(), vec![Instr::Return]);
    assert!(jit_is_switch_safe(&rt, &module));
}

/// `JitMode::Interpreter` and `JitMode::Baseline` force the tier regardless of the
/// predicate, so a caller (or a test) can pin either tier explicitly.
#[test]
fn the_mode_forces_the_tier() {
    let rt = runtime();
    // `stdlib_more_harness` is unsafe under Auto, but Baseline forces the JIT anyway.
    let module = Arc::new(beam_loader::load_file(stdlib_harness_fixture()).expect("harness"));
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
        select_entry_tier(&rt, &mut state, &entry, JitMode::Interpreter),
        Tier::Interpreter
    );
    // A forced mode consults no predicate, so nothing was memoized.
    assert!(state.jit_safety.is_empty());
    // `slice` is safe under Auto, but Interpreter still forces the interpreter.
    let slice = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
    let entry = ProcessEntry::Export {
        module: slice,
        function: Atom::from_raw(0),
        arity: 2,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Interpreter),
        Tier::Interpreter
    );
}

/// A fun entry whose local target has not been resolved yet (its module is only known once
/// the context resolves it) is conservatively interpreted — never a module the predicate
/// could not have inspected.
#[test]
fn an_unresolved_fun_entry_is_conservatively_interpreted() {
    let rt = runtime();
    let mut state = RunnerState::default();
    let entry = ProcessEntry::Fun {
        fun: make_small(0),
        target: None,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::Interpreter,
        "an unresolved fun entry reports no module ⇒ the conservative interpreter"
    );
    // Nothing was inspected, so no memo entry was created.
    assert!(state.jit_safety.is_empty());
}

/// The memoized verdict is reused: a second selection for the same module does not re-run
/// the predicate (the memo is present after the first call).
#[test]
fn the_switch_safety_verdict_is_memoized_per_module() {
    let rt = runtime();
    let module = Arc::new(beam_loader::load_file(slice_fixture()).expect("slice.beam"));
    let mut state = RunnerState::default();
    assert!(state.jit_safety.is_empty());
    let entry = ProcessEntry::Export {
        module: Arc::clone(&module),
        function: Atom::from_raw(0),
        arity: 2,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::BaselineJit
    );
    assert_eq!(state.jit_safety.len(), 1, "one entry per module name");
    // A second call (a second process of the same module) hits the memo.
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::BaselineJit
    );
    assert_eq!(state.jit_safety.len(), 1, "no new entry");
}

// -- the process-wide default mode ----------------------------------------

/// The effective mode for an **unset** `BEAM_RT_TIER` is `Auto` — the JIT exactly where the
/// soundness predicate proves the module switch-safe, the interpreter everywhere else.
/// `"baseline"` forces the JIT and any unknown value falls back to the always-correct
/// interpreter. Pinned through the **pure** resolution helper ([`mode_from_value`]) rather
/// than by mutating the process environment (the crate is `#![deny(unsafe_code)]`), and the
/// derived [`JitMode::default`] must agree with it so no call site can pick a different tier
/// by omission.
#[test]
fn the_unset_default_resolves_to_auto() {
    assert_eq!(
        mode_from_value(None),
        JitMode::Auto,
        "an unset BEAM_RT_TIER is the (regression-free) switch-safe-auto default"
    );
    assert_eq!(
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
    // Auto took the JIT on the safe `slice` module (the predicate is exercised in the unit
    // tests above); this test's value is that both tiers agree.
    let mut state = RunnerState::default();
    let rt = runtime();
    let module = beam_loader::load_file(slice_fixture()).expect("slice.beam");
    assert!(jit_is_switch_safe(&rt, &module), "Auto would pick the JIT");
    let entry = ProcessEntry::Export {
        module: Arc::new(module),
        function: Atom::from_raw(0),
        arity: 1,
    };
    assert_eq!(
        select_entry_tier(&rt, &mut state, &entry, JitMode::Auto),
        Tier::BaselineJit
    );
}

/// The whole runtime's **scheduler** path (not just one slice) runs `slice:fib/1 10` and
/// records `55` — the tier selection is wired into the real `RuntimeRunner` the scheduler
/// drives, on whatever mode the process environment selects (an unset `BEAM_RT_TIER` ⇒
/// `auto`, i.e. the JIT for the switch-safe `slice` module). The test asserts the *result*,
/// so it holds on every tier.
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
/// `alloc_loop/1` builds one cons cell per iteration, so the compiler emits a `test_heap` —
/// the runner's one reduction-budget admission point — and a small budget therefore
/// **pre-empts** it part-way through (see the fixture's own docs).
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
