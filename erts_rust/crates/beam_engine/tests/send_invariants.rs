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

//! The `send` opcode's two **live** invariants, driven end to end through the real
//! run/resume entries.
//!
//! `Pid ! Msg` compiles to a bare `send` (`../../erts/emulator/beam/emu/bif_instrs.tab:436-441`):
//! deliver `x(1)` to `x(0)`, store the **message** in `x(0)` on success and — because
//! the opcode carries no fail label — route a delivery failure through
//! [`Machine::raise_bif_error`] exactly like a `call_ext` failure
//! (`src/families/receive.rs`, the `Instr::Send` arm).
//!
//! Both halves are load-bearing, and each is pinned below:
//!
//! 1. **The operands are read live, at execution time.** A `receive` that finds no
//!    message parks the process ([`RunOutcome::Wait`]) and the host resumes it later
//!    with [`Machine::resume_outcome`]. The engine keeps **no** suspended-instruction
//!    state — no cached operand pair, no saved argument vector (the C's
//!    `c_arg_reg`, `../../erts/emulator/beam/beam_common.c:306`/`:336`, is not
//!    reproduced as engine state) — so a `send` in the resumed slice must deliver
//!    whatever `x(0)`/`x(1)` hold *then*. An engine that remembered the pre-park pair
//!    would deliver a stale message to a stale destination: a silent wrong result,
//!    the one failure mode this crate must never produce.
//! 2. **A failure is raised, never swallowed and never reported as a success.** The
//!    host's [`ExecutionContext::send_message_with_off_heap`] may fail — the runtime
//!    answers [`BeamError::TermNotFound`] when the destination pid names no live
//!    process. Folding that into `Ok(())` would make `Pid ! Msg` claim a delivery
//!    that never happened, and writing the message into `x(0)` would make the
//!    expression *look* delivered.
//!
//! Both tests use a synthetic module (the compiled shapes, hand-built: no loader, no
//! fixture `.beam`) over a recording host, so the exact `(to, message, src_binaries)`
//! triple the arm hands the seam is observable.

use std::collections::BTreeMap;

use beam_common::{BeamError, Result};
use beam_engine::{ExecutionContext, Machine, RunOutcome};
use beam_loader::{Export, Function, Instr, Label, Module, Operand};
use beam_term::{make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding (see tests/wait_deadline.rs)
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

/// `run/4` — `receive _ -> NewPid ! NewMsg end` in the compiler's shape, with the
/// post-park destination and message carried in the caller's *other* two arguments
/// (`x(2)`/`x(3)`) so the `send`'s operands provably change after the park.
///
/// ```text
/// 0  label 1
/// 1  loop_rec 2, x4          ; the mailbox is empty on the first slice -> jump to the wait
/// 2  remove_message          ; consume the message that released the park
/// 3  move x2 -> x0           ; the destination the `send` must actually use
/// 4  move x3 -> x1           ; the message the `send` must actually deliver
/// 5  send                    ; x(0) ! x(1)   <-- the instruction under test
/// 6  return
/// 7  label 2
/// 8  wait 1                  ; park: RunOutcome::Wait { func: 0, pc: 0 }
/// ```
fn reread_module() -> (Module, Atom) {
    let mut f = Fixture::new("snd");
    let run = f.add_function(
        "run",
        4,
        vec![
            Instr::Label(Label(1)),
            Instr::LoopRec {
                fail: Label(2),
                dst: Operand::X(4),
            },
            Instr::RemoveMessage,
            Instr::Move {
                src: Operand::X(2),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::X(3),
                dst: Operand::X(1),
            },
            Instr::Send,
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Wait { target: Label(1) },
        ],
    );
    f.add_export("run", 4, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// `run/2` — a bare `send`: `label 1 | send | return`.
fn plain_send_module() -> (Module, Atom) {
    let mut f = Fixture::new("snd");
    let run = f.add_function(
        "run",
        2,
        vec![Instr::Label(Label(1)), Instr::Send, Instr::Return],
    );
    f.add_export("run", 2, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// `run/2` — the same `send` inside a **live** `try`/`catch`, whose handler stores
/// `caught` in `x(0)` (so a landing is observable in the run's value).
///
/// ```text
/// 0  label 1
/// 1  try 3, x9               ; push a catch frame whose handler is label 3
/// 2  send                    ; the instruction under test
/// 3  return
/// 4  label 3
/// 5  move caught -> x0       ; the handler, reached only if the send's raise lands
/// 6  return
/// ```
fn try_send_module() -> (Module, Atom) {
    let mut f = Fixture::new("snd");
    let caught = f.atom("caught");
    let run = f.add_function(
        "run",
        2,
        vec![
            Instr::Label(Label(1)),
            Instr::Try {
                frame: Operand::X(9),
                fail: Label(3),
            },
            Instr::Send,
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::Move {
                src: Operand::Atom(caught),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    f.add_export("run", 2, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

// ---------------------------------------------------------------------------
// The recording host
// ---------------------------------------------------------------------------

/// A host that records every `send` attempt as the exact `(to, message, off_heap)`
/// triple the arm handed the seam, and whose virtual mailbox a test can fill between
/// a park and a resume.
#[derive(Default)]
struct SendHost {
    /// The messages the virtual mailbox holds, in arrival order.
    messages: Vec<Eterm>,
    /// The mailbox scan position (`loop_rec` peeks it, `remove_message` resets it).
    scan: usize,
    /// One entry per `send` the arm attempted: the destination, the message and the
    /// `data_ptr()` of every off-heap payload handle in the forwarded `src_binaries`.
    sends: Vec<(Eterm, Eterm, Vec<usize>)>,
    /// What `send_message_with_off_heap` answers; `None` delivers successfully.
    send_error: Option<BeamError>,
}

impl ExecutionContext for SendHost {
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

    fn send_message_with_off_heap(
        &mut self,
        to: Eterm,
        message: Eterm,
        src_binaries: &[beam_alloc::RefcBinary],
    ) -> Result<()> {
        self.sends.push((
            to,
            message,
            src_binaries.iter().map(|b| b.data_ptr() as usize).collect(),
        ));
        match self.send_error.take() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(self.messages.get(self.scan).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        self.scan += 1;
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        if self.scan < self.messages.len() {
            self.messages.remove(self.scan);
        }
        self.scan = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        self.scan = 0;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Invariant 1: the operands are re-read from the live register file
// ---------------------------------------------------------------------------

#[test]
fn a_send_after_a_park_reads_the_live_register_file_not_the_pre_park_pair() {
    let (module, run) = reread_module();

    // Two distinct destinations and two distinct messages: the pre-park pair lives in
    // `x(0)`/`x(1)` (the call arguments), the post-park pair in `x(2)`/`x(3)` — which
    // the resumed slice moves into `x(0)`/`x(1)` *after* the park and *before* the
    // `send`.
    let (parked_to, parked_msg) = (make_small(1), make_small(11));
    let (live_to, live_msg) = (make_small(2), make_small(22));
    // The message whose arrival releases the park (this host only counts it).
    let arriving = make_small(77);

    let mut machine = Machine::new();
    let mut host = SendHost::default();

    let outcome = machine
        .run_export_outcome(
            &module,
            run,
            4,
            &[parked_to, parked_msg, live_to, live_msg],
            &mut host,
        )
        .expect("the first slice parks in the receive");
    assert_eq!(
        outcome,
        RunOutcome::Wait { func: 0, pc: 0 },
        "an empty mailbox parks the process at the `wait`"
    );
    assert!(
        host.sends.is_empty(),
        "the parked slice must not reach the `send`"
    );

    // The pump: the message arrives between the two slices, exactly as a scheduler
    // would deliver it before resuming the process.
    host.messages.push(arriving);

    let outcome = machine
        .resume_outcome(&module, 0, 0, &mut host)
        .expect("the resumed slice returns");
    assert_eq!(outcome, RunOutcome::Returned(live_msg));

    // THE ASSERTION: the seam received exactly the destination and message that
    // `x(0)`/`x(1)` hold when the `send` executes, plus the live machine's off-heap
    // handle slice (empty here — the message is not a refc binary). An arm that had
    // cached the operand pair before the park would deliver the pre-park pair.
    assert_eq!(
        host.sends,
        vec![(live_to, live_msg, Vec::new())],
        "the `send` must read x(0)/x(1) at execution time"
    );
    assert_ne!(
        host.sends,
        vec![(parked_to, parked_msg, Vec::new())],
        "the pre-park pair must never be delivered"
    );

    // `erl_send` returns the message itself (`bif_instrs.tab:436-441`): `Pid ! Msg` is
    // `Msg`, so a successful `send` leaves the message in `x(0)`.
    assert_eq!(machine.x(0), Some(live_msg));
    // …and the `receive` really did consume the message that released the park, so the
    // resumed slice ran the whole receive-then-send path.
    assert_eq!(machine.x(4), Some(arriving));
}

// ---------------------------------------------------------------------------
// Invariant 2: a failed delivery is surfaced, never swallowed
// ---------------------------------------------------------------------------

#[test]
fn a_failed_send_is_raised_and_never_reported_as_a_success() {
    let (to, message) = (make_small(3), make_small(42));

    // ---- with no live catch frame: the typed error propagates unchanged ----
    let (module, run) = plain_send_module();
    let mut machine = Machine::new();
    let mut host = SendHost::default();
    // The runtime's answer for a destination pid that names no live process.
    host.send_error = Some(BeamError::TermNotFound);

    let err = machine
        .run_export(&module, run, 2, &[to, message], &mut host)
        .expect_err("a failed delivery must be raised, not swallowed");
    assert!(
        matches!(err, BeamError::TermNotFound),
        "the arm must raise the host's own failure, got {err:?}"
    );

    // Exactly one delivery was attempted: no retry, no second send.
    assert_eq!(
        host.sends,
        vec![(to, message, Vec::new())],
        "the arm attempts the delivery exactly once"
    );
    // THE ASSERTION: `x(0)` still holds the destination, NOT the message. An arm that
    // fell through to `set_x(0, message)` after the failure would make `Pid ! Msg`
    // look delivered.
    assert_eq!(
        machine.x(0),
        Some(to),
        "a failed `send` must not write the message into x(0)"
    );
    assert_ne!(machine.x(0), Some(message));

    // ---- inside a live `try`: still never a silent drop ----
    // `TermNotFound` has no BEAM atom (`beam_common::BeamError::as_atom` is `None`), so
    // the shared raise funnel declines to land it rather than substituting an atom
    // (`src/interpreter/bif.rs`, `raise_error`): the typed error propagates out of the
    // run and the handler never runs. Either way the arm must not swallow it.
    let (module, run) = try_send_module();
    let mut machine = Machine::new();
    let mut host = SendHost::default();
    host.send_error = Some(BeamError::TermNotFound);

    let err = machine
        .run_export(&module, run, 2, &[to, message], &mut host)
        .expect_err("a live catch frame must not turn the failure into success");
    assert!(
        matches!(err, BeamError::TermNotFound),
        "the error survives the catch-aware raise path verbatim, got {err:?}"
    );
    assert_eq!(
        host.sends.len(),
        1,
        "still exactly one delivery attempt"
    );
    assert_eq!(
        machine.x(0),
        Some(to),
        "the handler must not run and nothing may be written into x(0)"
    );
}
