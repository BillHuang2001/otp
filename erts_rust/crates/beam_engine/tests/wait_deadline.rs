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

//! The parked-`receive` **deadline** ([`Machine::wait_deadline`]/
//! [`Machine::timeout_delay`] and [`WaitDeadline`]) plus the named `recv_marker_*`
//! rejection.
//!
//! # The parked `receive … after T`
//!
//! A `receive … after T` compiles to the shape
//!
//! ```text
//! ScanLabel:  loop_rec Fail, Dst
//!             … match tests, loop_rec_end ScanLabel …
//! Fail:       wait_timeout ScanLabel, Time   ; <-- current_code while parked
//!             timeout                         ; <-- the timed-out continuation
//! ```
//!
//! A suspension is a *blocking* instruction (never inlined by a native tier), so
//! [`Machine::current_code`] is exact at the park. [`Machine::wait_deadline`] reads
//! the parked `wait_timeout`'s `Time` operand — a compile-time [`Operand::Integer`], a
//! materialized [`Operand::Literal`], or a runtime [`Operand::X`]/[`Operand::Y`]
//! register — and reports the finite delay (or the honest absence of one) together
//! with the `timeout` continuation the host re-enters when its timer fires.
//!
//! # The `recv_marker_*` opcodes
//!
//! The `recv_marker_*` family is a boost-only optimisation served through the mailbox
//! seam. A host that does not serve markers at all must be rejected **by opcode name**
//! (`unsupported_opcode: recv_marker_bind: the host does not serve receive markers`)
//! rather than leaving a marker that a later `recv_marker_use` could silently ignore; a
//! host that *does* serve markers and fails has the real reason, which survives
//! verbatim.

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::{BeamError, Result};
use beam_engine::{Machine, Mailbox, MinimalContext, RunOutcome};
use beam_engine::machine::WaitDeadline;
use beam_loader::{
    Export, Function, HeapNeed, Instr, Label, LiteralArea, Module, Operand,
};
use beam_term::{make_atom, make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding (see tests/external_call.rs)
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for one synthetic module with its own atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// `recv/1`: the compiled `receive … after Time` shape with `Time` supplied by the
/// caller. The `timeout` continuation is at code index `6`.
///
/// ```text
/// 0  label 1
/// 1  loop_rec 2, x0
/// 2  remove_message
/// 3  return
/// 4  label 2
/// 5  wait_timeout 2, Time     ; <-- parked here on a non-zero time
/// 6  timeout                  ; <-- the timed-out continuation (pc + 1)
/// 7  return
/// ```
fn wait_timeout_module(time: Operand) -> (Module, Atom) {
    let mut f = Fixture::new("recv");
    let run = f.add_function(
        "recv",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::LoopRec {
                fail: Label(2),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::WaitTimeout {
                target: Label(1),
                time,
            },
            Instr::Timeout,
            Instr::Return,
        ],
    );
    f.add_export("recv", 1, run);
    let recv_atom = f.atom("recv");
    (f.finish(), recv_atom)
}

/// `recv/1`: a bare `receive` — the `wait` the compiler emits for `after infinity`.
fn wait_module() -> (Module, Atom) {
    let mut f = Fixture::new("recv");
    let run = f.add_function(
        "recv",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::LoopRec {
                fail: Label(2),
                dst: Operand::X(0),
            },
            Instr::RemoveMessage,
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Wait { target: Label(1) },
        ],
    );
    f.add_export("recv", 1, run);
    let recv_atom = f.atom("recv");
    (f.finish(), recv_atom)
}

/// `recv/1`: a `receive … after Y0` where the delay is stored in a **Y register**
/// (a runtime variable), so the parked `wait_timeout`'s `Time` is `Operand::Y(0)`.
/// The `timeout` continuation is at code index `9`.
///
/// ```text
/// 0  label 1
/// 1  allocate 1
/// 2  move 20, y0
/// 3  label 2
/// 4  loop_rec 5, x0
/// 5  remove_message
/// 6  return
/// 7  label 3
/// 8  wait_timeout 2, y0
/// 9  timeout
/// 10 return
/// ```
fn y_delay_module() -> (Module, Atom) {
    let mut f = Fixture::new("recv");
    let run = f.add_function(
        "recv",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(20),
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
        ],
    );
    f.add_export("recv", 1, run);
    let recv_atom = f.atom("recv");
    (f.finish(), recv_atom)
}

/// `run/0`: `label 1 | <instr> | return` — one `recv_marker_*` opcode then a return.
fn marker_module(instr: Instr) -> (Module, Atom) {
    let mut f = Fixture::new("marker");
    let run = f.add_function("run", 0, vec![Instr::Label(Label(1)), instr, Instr::Return]);
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// A mailbox that never holds a message, so a `receive` suspends.
struct EmptyMailbox;

impl Mailbox for EmptyMailbox {
    fn send_message(&mut self, _to: Eterm, _message: Eterm) -> Result<()> {
        Ok(())
    }
    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(None)
    }
    fn receive_skip(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_remove(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_reset(&mut self) -> Result<()> {
        Ok(())
    }
    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
        Err(BeamError::BadArg)
    }
    fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
        Err(BeamError::BadArg)
    }
}

/// A context that IS marker-capable: `receive_marker_reserve` either fails with its
/// **own** typed error (which must survive verbatim) or succeeds, leaving the
/// destination register written.
struct MarkerCtx {
    reserve: Reserve,
}

enum Reserve {
    RealError,
    Succeed(Eterm),
}

impl MarkerCtx {
    fn real_error() -> MarkerCtx {
        MarkerCtx {
            reserve: Reserve::RealError,
        }
    }

    fn succeeding(value: Eterm) -> MarkerCtx {
        MarkerCtx {
            reserve: Reserve::Succeed(value),
        }
    }
}

impl beam_engine::ExecutionContext for MarkerCtx {
    fn call_bif(
        &mut self,
        _module: &str,
        _function: &str,
        _arity: u32,
        _args: &[Eterm],
    ) -> Result<Eterm> {
        Err(BeamError::BadArg)
    }

    fn ensure_heap(
        &mut self,
        _heap_need: u32,
        _live: u32,
        _x_regs: &mut [Eterm],
        _stack: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        match self.reserve {
            Reserve::RealError => Err(BeamError::BadArg),
            Reserve::Succeed(value) => Ok(value),
        }
    }

    fn receive_marker_bind(&mut self, _marker: Eterm, _reference: Eterm) -> Result<()> {
        Ok(())
    }

    fn receive_marker_use(&mut self, _reference: Eterm) -> Result<()> {
        Ok(())
    }

    fn receive_marker_clear(&mut self, _reference: Eterm) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The `.n` of a typed engine raise.
fn reason(error: BeamError) -> String {
    match error {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("a typed engine error names its reason")
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}

/// Run `recv/1` on an empty mailbox, returning the machine and context.
fn run_recv(module: &Module, entry: Atom, arg: Eterm) -> (RunOutcome, Machine, MinimalContext) {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_export_outcome(module, entry, 1, &[arg], &mut ctx)
        .expect("run recv/1");
    (outcome, machine, ctx)
}

// ---------------------------------------------------------------------------
// wait_deadline: the parked `receive … after T`
// ---------------------------------------------------------------------------

#[test]
fn wait_timeout_with_a_literal_delay_reports_the_deadline_and_the_timeout_continuation() {
    let (module, run) = wait_timeout_module(Operand::Integer(20));
    let (outcome, machine, _ctx) = run_recv(&module, run, make_small(7));
    // The non-zero delay suspends, resuming at the scan label (`Label(1)` = pc 0).
    assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 0 });

    let deadline = machine
        .wait_deadline(&module)
        .expect("a parked wait_timeout has a deadline");
    assert_eq!(
        deadline,
        WaitDeadline {
            delay_ms: Some(20),
            resume: (0, 6),
        }
    );
    // The `resume` is the `timeout` instruction directly after the parked
    // `wait_timeout` — exactly where a fired timer re-enters the process.
    assert_eq!(module.functions[0].code[6], Instr::Timeout);
}

#[test]
fn wait_timeout_with_a_zero_delay_falls_through_without_suspending() {
    // `after 0` is the compiled "do not suspend": C falls through to the `timeout`
    // instruction directly, so there is no suspension at all and the function returns
    // the value the timeout path reaches (here the argument, which nothing overwrote).
    let (module, run) = wait_timeout_module(Operand::Integer(0));
    let (outcome, machine, _ctx) = run_recv(&module, run, make_small(7));
    assert_eq!(outcome, RunOutcome::Returned(make_small(7)));
    // Nothing parked, so there is no deadline to report.
    assert_eq!(machine.wait_deadline(&module), None);
}

#[test]
fn wait_timeout_with_an_infinity_literal_has_no_delay_but_a_resume() {
    // The atom `infinity` means "never": the delay is unreadable as milliseconds, so
    // `delay_ms` is `None` — the honest absence of a deadline, never a guess. The
    // `resume` is still reported (it is the `timeout` continuation).
    let (mut module, run) = wait_timeout_module(Operand::Literal(0));
    // term_to_binary(infinity).
    module_fixture_literals(&mut module, &[vec![131, 119, 8, 105, 110, 102, 105, 110, 105, 116, 121]]);
    let (outcome, machine, _ctx) = run_recv(&module, run, make_small(7));
    // A non-integer time still suspends (it is not the small integer 0).
    assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 0 });

    let deadline = machine
        .wait_deadline(&module)
        .expect("a parked wait_timeout has a resume even without a readable delay");
    assert_eq!(
        deadline,
        WaitDeadline {
            delay_ms: None,
            resume: (0, 6),
        }
    );
}

/// Set `module`'s static-literal area from raw ETF payloads (the fixture's
/// `set_literals` needs the module still owned by the builder, so this re-decodes in
/// place for a module already built).
fn module_fixture_literals(module: &mut Module, raw: &[Vec<u8>]) {
    let area = LiteralArea::decode(raw, &mut module.atom_table).expect("decode the literals");
    module.literals = Arc::new(area);
}

#[test]
fn wait_timeout_with_an_infinity_register_has_no_delay() {
    // The delay as a runtime variable in x0: `read_operand` resolves it, and a
    // non-integer (`infinity`) value is the absence of a deadline.
    let (mut module, run) = wait_timeout_module(Operand::X(0));
    let infinity = make_atom(module.atom_table.intern("infinity"));
    let (outcome, machine, _ctx) = run_recv(&module, run, infinity);
    assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 0 });
    let deadline = machine
        .wait_deadline(&module)
        .expect("the read is still a parked wait_timeout");
    assert_eq!(deadline.delay_ms, None);
    assert_eq!(deadline.resume, (0, 6));
}

#[test]
fn a_bare_wait_has_no_deadline() {
    let (module, run) = wait_module();
    let (outcome, machine, _ctx) = run_recv(&module, run, make_small(7));
    assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 0 });
    // `after infinity` compiles to a bare `wait`; there is no deadline at all.
    assert_eq!(machine.wait_deadline(&module), None);
}

#[test]
fn a_y_register_delay_is_read_from_the_frame_and_y_is_bounded() {
    let (module, run) = y_delay_module();
    let mut machine = Machine::new();
    // A fresh machine has no frame, so no Y slot is addressable.
    assert_eq!(machine.y(0), None);
    assert!(machine.y_mut(0).is_none());

    let mut ctx = MinimalContext::with_mailbox(Box::new(EmptyMailbox));
    let outcome = machine
        .run_export_outcome(&module, run, 1, &[make_small(7)], &mut ctx)
        .expect("run recv/1");
    assert_eq!(outcome, RunOutcome::Wait { func: 0, pc: 3 });

    let deadline = machine
        .wait_deadline(&module)
        .expect("a parked wait_timeout has a deadline");
    assert_eq!(
        deadline,
        WaitDeadline {
            delay_ms: Some(20),
            resume: (0, 9),
        }
    );

    // The delay lived in Y(0) of the live frame; `Machine::y` reads it.
    assert_eq!(machine.y(0), Some(make_small(20)));
    // Bounds: the frame owns exactly one Y slot.
    assert_eq!(machine.y(1), None, "the frame has one Y slot");
    assert_eq!(machine.y(99), None, "out of range");
    assert!(machine.y_mut(1).is_none(), "y_mut is bounded like y");

    // A write through `y_mut` is visible through `y`.
    *machine.y_mut(0).expect("Y(0) is in range") = make_small(21);
    assert_eq!(machine.y(0), Some(make_small(21)));
}

#[test]
fn wait_deadline_is_none_before_any_run() {
    let (module, _run) = wait_timeout_module(Operand::Integer(20));
    let machine = Machine::new();
    assert_eq!(machine.wait_deadline(&module), None);
}

// ---------------------------------------------------------------------------
// recv_marker_*: a host that does not serve markers is rejected by opcode name
// ---------------------------------------------------------------------------

#[test]
fn a_host_that_does_not_serve_markers_rejects_each_recv_marker_opcode_by_name() {
    // `MinimalContext::new()` installs no mailbox, so every marker opcode hits the
    // seam's own `unsupported_opcode: receive_marker_<op>` default — which the engine
    // must translate into a rejection naming the `ops.tab` opcode.
    let cases: Vec<(Instr, &str)> = vec![
        (
            Instr::RecvMarkerReserve {
                marker: Operand::X(1),
            },
            "recv_marker_reserve",
        ),
        (
            Instr::RecvMarkerBind {
                marker: Operand::X(0),
                reference: Operand::X(1),
            },
            "recv_marker_bind",
        ),
        (
            Instr::RecvMarkerUse {
                reference: Operand::X(1),
            },
            "recv_marker_use",
        ),
        (
            Instr::RecvMarkerClear {
                reference: Operand::X(1),
            },
            "recv_marker_clear",
        ),
    ];

    for (instr, opcode) in cases {
        let (module, run) = marker_module(instr);
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        let err = machine
            .run_export_outcome(&module, run, 0, &[], &mut ctx)
            .expect_err("a host without markers rejects the opcode");
        // The rejection names the `ops.tab` opcode and what is missing. Like every
        // engine diagnostic it is a `BeamError::Raise` built by `error::unsupported`,
        // so the reason atom carries the crate-wide `unsupported_opcode: ` prefix.
        assert_eq!(
            reason(err),
            format!("unsupported_opcode: {opcode}: the host does not serve receive markers")
        );
    }
}

#[test]
fn a_real_marker_host_error_survives_verbatim() {
    // A host that DOES serve markers but fails has the real reason: the engine must
    // not translate it (only the seam's own default is translated).
    let (module, run) = marker_module(Instr::RecvMarkerReserve {
        marker: Operand::X(1),
    });
    let mut machine = Machine::new();
    let mut ctx = MarkerCtx::real_error();
    let err = machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect_err("the marker host's own error propagates");
    assert!(
        matches!(err, BeamError::BadArg),
        "the host's own error must survive verbatim, got {err:?}"
    );
}

#[test]
fn a_marker_host_that_succeeds_leaves_the_register_written() {
    // A successful `recv_marker_reserve` stores the marker in the destination register
    // (`Step::Next`) and the run continues to the following `return`.
    let (module, run) = marker_module(Instr::RecvMarkerReserve {
        marker: Operand::X(1),
    });
    let mut machine = Machine::new();
    let mut ctx = MarkerCtx::succeeding(make_small(77));
    let outcome = machine
        .run_export_outcome(&module, run, 0, &[], &mut ctx)
        .expect("a marker-capable host lets the run continue");
    assert!(
        matches!(outcome, RunOutcome::Returned(_)),
        "the run continues past the marker opcode, got {outcome:?}"
    );
    assert_eq!(
        machine.x(1),
        Some(make_small(77)),
        "the reserved marker is written into the destination register"
    );
}
