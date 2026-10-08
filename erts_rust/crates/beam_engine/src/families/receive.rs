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

//! Receive/message instruction family.
//!
//! Owns the message-passing opcodes (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary") — the ninth family the dispatcher tries:
//! - [`Instr::Send`] — `send`: deliver `x(1)` to `x(0)`
//! - [`Instr::LoopRec`] — `loop_rec Fail Dst`: the next candidate, or jump to `Fail`
//! - [`Instr::LoopRecEnd`] — `loop_rec_end Target`: skip the candidate, jump back
//! - [`Instr::Wait`] — `wait Target`: suspend, resuming at `Target`
//! - [`Instr::WaitTimeout`] — `wait_timeout Target Time`: the same, after a timeout
//! - [`Instr::Timeout`] — `timeout`: reset the mailbox scan position
//! - [`Instr::RemoveMessage`] — `remove_message`: unlink the matched candidate
//! - [`Instr::RecvMarkerReserve`]/[`Instr::RecvMarkerBind`]/[`Instr::RecvMarkerUse`]/
//!   [`Instr::RecvMarkerClear`] — the `recv_marker_*` scan bookkeeping opcodes
//!
//! Ground truth (read-only): `emu/msg_instrs.tab` (`i_loop_rec`, `loop_rec_end`,
//! `remove_message`, `timeout`, `wait.int`/`wait.src`/`wait.execute`,
//! `recv_marker_*`), `emu/bif_instrs.tab:425-455` (`send`) and the opcode table
//! `emu/ops.tab:461-494`.
//!
//! ## Semantics
//! - This family **positions and removes** mailbox candidates; it never matches
//!   them. Message *selection* is the compiled code's job: the receive's match
//!   tests (the `is_*`/`select_val`/`get_tuple_element` instructions the compiler
//!   emits between `loop_rec` and `loop_rec_end`) run against the candidate that
//!   [`Instr::LoopRec`] placed in `Dst`. So this module contains **no** pattern
//!   matching and no term inspection of messages.
//! - Every mailbox operation that involves a *time* or a *position* is delegated
//!   to the host through the [`ExecutionContext`] receive seam
//!   ([`ExecutionContext::send_message`], `receive_next`, `receive_skip`,
//!   `receive_remove`, `receive_reset`, `receive_marker_*`). The host owns the
//!   mailbox and the scan position; the engine keeps no mailbox state.
//! - `recv_marker_*` terms are **opaque**: a marker produced by
//!   `recv_marker_reserve` is stored in a register and handed back verbatim, and
//!   only a separate *reference* term reaches `recv_marker_use`/`recv_marker_clear`.
//!   This family never inspects, compares or hashes one.
//! - `wait`/`wait_timeout` (non-zero time) **suspend**: they return
//!   [`Step::Wait`], which the dispatch loop reports as [`crate::RunOutcome::Wait`]
//!   so the host can resume the process at the target address once a message
//!   arrives. There is **no host timer at Milestone 1**, so a non-zero timeout
//!   suspends until a message arrives (the `Time` operand selects between "do not
//!   suspend" and "suspend", exactly as C's `timeout_value == make_small(0)`
//!   test does) — the engine does not invent a timer.
//!
//! Every failure is a typed [`beam_common::BeamError`]: `read_operand`/
//! `write_operand` and the host seam propagate theirs, a missing target label is
//! [`crate::error::unknown_label`], and a `send` failure is raised through
//! [`Machine::raise_bif_error`] — the same catch-aware path a `call_ext` error
//! takes, because `send` carries no fail label (C falls through to
//! `find_func_info`). Never a panic.

use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::{is_small, signed_value};

use crate::context::ExecutionContext;
use crate::interpreter;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

/// Execute a receive/message-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a receive opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    // Every jump/suspend address in this family is the *target label*'s resolved
    // address, so the caller's `func` is not consulted (the `WaitTimeout` case
    // uses the caller's `pc` for the fall-through continuation instead).
    let _ = func;
    match instr {
        // `send` — `erl_send(c_p, x(0), x(1))`: deliver `x(1)` to `x(0)`. It cannot
        // be traced and carries no fail label, so a failure is a raise; on success
        // C stores `erl_send`'s result (the **message itself**) in `x(0)`, which is
        // why `Pid ! Msg` evaluates to `Msg`
        // (`../../erts/emulator/beam/emu/bif_instrs.tab:436-441`).
        Instr::Send => {
            let to = read_operand(machine, ctx, &Operand::X(0))?;
            let message = read_operand(machine, ctx, &Operand::X(1))?;
            match ctx.send_message(to, message) {
                Ok(()) => {
                    machine.set_x(0, message)?;
                    Ok(Some(Step::Next))
                }
                Err(e) => machine.raise_bif_error(module, e).map(Some),
            }
        }

        // `loop_rec Fail Dst` — `x(Dst) = <candidate>`, or jump to `Fail` (the
        // `wait`/`wait_timeout` that follows) when the mailbox scan is exhausted.
        // C also jumps to `*I` with `$SET_I_REL($Dest)`
        // (`../../erts/emulator/beam/emu/msg_instrs.tab:100-103`).
        Instr::LoopRec { fail, dst } => match ctx.receive_next()? {
            Some(candidate) => {
                write_operand(machine, dst, candidate)?;
                Ok(Some(Step::Next))
            }
            None => Ok(Some(goto_label(labels, *fail)?)),
        },

        // `loop_rec_end Target` — the current candidate did not match: advance the
        // scan past it and jump back to the `loop_rec` that picks up the next one
        // (C: `erts_msgq_set_save_next(c_p)` then `goto loop_rec__`).
        Instr::LoopRecEnd { target } => {
            ctx.receive_skip()?;
            Ok(Some(goto_label(labels, *target)?))
        }

        // `wait Target` — suspend (C: `$SET_REL_I(c_p->i, $JumpTarget)` and
        // `goto do_schedule`); the process resumes at `Target` — the `loop_rec`
        // that re-examines the mailbox — once a message arrives.
        Instr::Wait { target } => Ok(Some(wait_at(labels, *target)?)),

        // `wait_timeout Target Time` — the timeout operand decides:
        // * the small integer `0` means "do not suspend": C falls through to the
        //   `timeout` instruction directly after (`$NEXT0()`,
        //   `../../erts/emulator/beam/emu/msg_instrs.tab:317-320`);
        // * anything else (a positive integer or the atom `infinity`) suspends like
        //   `wait`, resuming at `Target`. Milestone 1 has no host timer, so a
        //   non-zero timeout simply suspends until a message arrives — the `Time`
        //   value selects *which* of the two C paths is taken, and the timer half of
        //   the non-zero path is the documented coverage gap.
        Instr::WaitTimeout { target, time } => {
            let value = read_operand(machine, ctx, time)?;
            if is_small(value) && signed_value(value) == 0 {
                Ok(Some(Step::Goto { func, pc: pc + 1 }))
            } else {
                Ok(Some(wait_at(labels, *target)?))
            }
        }

        // `timeout` — the receive's timeout fired: reset the scan position to the
        // first message (C `erts_msgq_set_save_first`) and continue.
        Instr::Timeout => {
            ctx.receive_reset()?;
            Ok(Some(Step::Next))
        }

        // `remove_message` — unlink the matched candidate and reposition the scan at
        // the first message (C `erts_msgq_unlink_msg_set_save_first`).
        Instr::RemoveMessage => {
            ctx.receive_remove()?;
            Ok(Some(Step::Next))
        }

        // `recv_marker_reserve Dst` — stash an opaque marker for the current scan
        // position in `Dst`; the engine never looks at it again.
        Instr::RecvMarkerReserve { marker } => {
            let reserved = ctx.receive_marker_reserve()?;
            write_operand(machine, marker, reserved)?;
            Ok(Some(Step::Next))
        }

        // `recv_marker_bind Marker Reference` — bind the marker to the reference so
        // a later `recv_marker_use` can restore this position.
        Instr::RecvMarkerBind { marker, reference } => {
            let marker = read_operand(machine, ctx, marker)?;
            let reference = read_operand(machine, ctx, reference)?;
            ctx.receive_marker_bind(marker, reference)?;
            Ok(Some(Step::Next))
        }

        // `recv_marker_use Reference` — restore the scan position the reference's
        // marker recorded.
        Instr::RecvMarkerUse { reference } => {
            let reference = read_operand(machine, ctx, reference)?;
            ctx.receive_marker_use(reference)?;
            Ok(Some(Step::Next))
        }

        // `recv_marker_clear Reference` — release the reference's marker.
        Instr::RecvMarkerClear { reference } => {
            let reference = read_operand(machine, ctx, reference)?;
            ctx.receive_marker_clear(reference)?;
            Ok(Some(Step::Next))
        }

        _ => Ok(None),
    }
}

/// Resolve `target` and return the `goto` [`Step`] for it.
///
/// A label the module does not define (including the `NO_FAIL` sentinel) is the
/// shared [`crate::error::unknown_label`] error, never a silent fall-through.
fn goto_label(labels: &LabelMap, target: Label) -> Result<Step> {
    let (func, pc) = interpreter::resolve(labels, target)?;
    Ok(Step::Goto { func, pc })
}

/// Resolve `target` and return the suspend [`Step`] that resumes there.
fn wait_at(labels: &LabelMap, target: Label) -> Result<Step> {
    let (func, pc) = interpreter::resolve(labels, target)?;
    Ok(Step::Wait { func, pc })
}

#[cfg(test)]
mod tests {
    //! Family-level tests over hand-built instructions: the family's observable
    //! behaviour (which register it writes, which [`Step`] it returns, which host
    //! seam method it calls) is asserted with a recording [`ExecutionContext`] stub
    //! — no real module, no loader, no interpreter.

    use super::*;
    use beam_common::BeamError;
    use beam_term::{make_small, Atom, Eterm};

    use crate::machine::CatchFrame;

    /// A recording [`ExecutionContext`]: no mailbox of its own, only the values the
    /// family must read back and the calls it must make.
    #[derive(Default)]
    struct StubCtx {
        /// The seam methods called, in order.
        calls: Vec<&'static str>,
        /// What `receive_next` answers.
        next: Option<Eterm>,
        /// What `send_message` receives.
        sent: Option<(Eterm, Eterm)>,
        /// The error `send_message` fails with, if any.
        send_error: Option<BeamError>,
        /// The opaque marker `receive_marker_reserve` hands back.
        marker: Eterm,
        /// What `receive_marker_bind` receives.
        bound: Option<(Eterm, Eterm)>,
        /// What `receive_marker_use` receives.
        used: Option<Eterm>,
        /// What `receive_marker_clear` receives.
        cleared: Option<Eterm>,
    }

    impl StubCtx {
        /// A stub whose `receive_marker_reserve` returns the small integer `90` (an
        /// arbitrary non-register term, so a mistaken pass-through is visible).
        fn new() -> StubCtx {
            StubCtx {
                marker: make_small(90),
                ..StubCtx::default()
            }
        }
    }

    impl ExecutionContext for StubCtx {
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

        fn send_message(&mut self, to: Eterm, message: Eterm) -> Result<()> {
            self.calls.push("send_message");
            self.sent = Some((to, message));
            match self.send_error.take() {
                Some(err) => Err(err),
                None => Ok(()),
            }
        }

        fn receive_next(&mut self) -> Result<Option<Eterm>> {
            self.calls.push("receive_next");
            Ok(self.next)
        }

        fn receive_skip(&mut self) -> Result<()> {
            self.calls.push("receive_skip");
            Ok(())
        }

        fn receive_remove(&mut self) -> Result<()> {
            self.calls.push("receive_remove");
            Ok(())
        }

        fn receive_reset(&mut self) -> Result<()> {
            self.calls.push("receive_reset");
            Ok(())
        }

        fn receive_marker_reserve(&mut self) -> Result<Eterm> {
            self.calls.push("receive_marker_reserve");
            Ok(self.marker)
        }

        fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
            self.calls.push("receive_marker_bind");
            self.bound = Some((marker, reference));
            Ok(())
        }

        fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
            self.calls.push("receive_marker_use");
            self.used = Some(reference);
            Ok(())
        }

        fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
            self.calls.push("receive_marker_clear");
            self.cleared = Some(reference);
            Ok(())
        }
    }

    /// A label map with `label` at `(func, pc)`.
    fn labels_with(label: Label, func: u32, pc: u32) -> LabelMap {
        LabelMap::from([(label, (func, pc))])
    }

    #[test]
    fn send_stores_the_message_in_x0_and_calls_the_seam() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        machine.set_x(0, make_small(7)).unwrap();
        machine.set_x(1, make_small(42)).unwrap();
        let mut ctx = StubCtx::new();

        let step = exec(&mut machine, &module, &labels, &Instr::Send, 0, 0, &mut ctx).unwrap();

        assert_eq!(step, Some(Step::Next));
        assert_eq!(ctx.calls, vec!["send_message"]);
        assert_eq!(ctx.sent, Some((make_small(7), make_small(42))));
        // `erl_send` returns the message itself: `Pid ! Msg` is `Msg`.
        assert_eq!(machine.x(0), Some(make_small(42)));
    }

    #[test]
    fn send_failure_propagates_without_a_catch_frame() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();
        ctx.send_error = Some(BeamError::BadArg);

        let err = exec(&mut machine, &module, &labels, &Instr::Send, 0, 0, &mut ctx).unwrap_err();

        // `send` carries no fail label, so an error is a raise — and with no live
        // catch frame it propagates unchanged.
        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn send_failure_lands_in_a_live_catch_frame() {
        // The catch-aware raise: a `try ... catch` around a `send` must see the
        // error, which means going through the crate's one `raise_bif_error` path.
        let mut module = Module::default();
        module.atom_table.intern("badarg");
        module.atom_table.intern("error");
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        machine.push_frame(0).unwrap();
        machine.push_catch_frame(CatchFrame::new(0, 9, 0));
        let mut ctx = StubCtx::new();
        ctx.send_error = Some(BeamError::BadArg);

        let step = exec(&mut machine, &module, &labels, &Instr::Send, 0, 0, &mut ctx).unwrap();

        assert_eq!(step, Some(Step::Goto { func: 0, pc: 9 }));
        assert_eq!(machine.catch_frame_depth(), 1, "the handler pops it later");
    }

    #[test]
    fn loop_rec_moves_the_candidate_into_dst() {
        let module = Module::default();
        let labels = labels_with(Label(4), 0, 4);
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();
        ctx.next = Some(make_small(11));

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::LoopRec {
                fail: Label(4),
                dst: Operand::X(1),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Next));
        assert_eq!(machine.x(1), Some(make_small(11)));
        assert_eq!(ctx.calls, vec!["receive_next"]);
    }

    #[test]
    fn loop_rec_with_an_empty_mailbox_jumps_to_the_fail_label() {
        let module = Module::default();
        let labels = labels_with(Label(4), 2, 7);
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();
        ctx.next = None;

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::LoopRec {
                fail: Label(4),
                dst: Operand::X(0),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        // The `wait`/`wait_timeout` the compiler emitted after the tests is the
        // jump target, so the mailbox scan is not restarted here.
        assert_eq!(step, Some(Step::Goto { func: 2, pc: 7 }));
        assert_eq!(machine.x(0), Some(Eterm::default()), "no candidate written");
    }

    #[test]
    fn loop_rec_with_an_unresolved_fail_label_is_a_typed_error() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let err = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::LoopRec {
                fail: Label(4),
                dst: Operand::X(0),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap_err();

        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn loop_rec_end_skips_the_candidate_then_jumps_back() {
        let module = Module::default();
        let labels = labels_with(Label(3), 1, 2);
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::LoopRecEnd { target: Label(3) },
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Goto { func: 1, pc: 2 }));
        assert_eq!(ctx.calls, vec!["receive_skip"]);
    }

    #[test]
    fn wait_suspends_at_its_target() {
        let module = Module::default();
        let labels = labels_with(Label(3), 0, 3);
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::Wait { target: Label(3) },
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Wait { func: 0, pc: 3 }));
        assert!(ctx.calls.is_empty(), "wait touches no mailbox state");
    }

    #[test]
    fn wait_with_an_unresolved_target_is_a_typed_error() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let err = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::Wait { target: Label(3) },
            0,
            0,
            &mut ctx,
        )
        .unwrap_err();

        assert!(matches!(err, BeamError::BadArg));
    }

    #[test]
    fn wait_timeout_with_zero_continues_at_the_following_timeout_instruction() {
        // A zero timeout means "do not suspend": control falls through to the
        // `timeout` instruction that directly follows (C's `$NEXT0()`).
        let module = Module::default();
        let labels = labels_with(Label(3), 0, 3);
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::WaitTimeout {
                target: Label(3),
                time: Operand::Integer(0),
            },
            0,
            5,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Goto { func: 0, pc: 6 }));
        assert!(ctx.calls.is_empty());
    }

    #[test]
    fn wait_timeout_with_a_positive_time_suspends_at_the_target() {
        let module = Module::default();
        let labels = labels_with(Label(3), 0, 3);
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::WaitTimeout {
                target: Label(3),
                time: Operand::Integer(1000),
            },
            0,
            5,
            &mut ctx,
        )
        .unwrap();

        // Milestone 1 has no host timer: a non-zero timeout suspends until a
        // message arrives (no invented timer, no wrong result).
        assert_eq!(step, Some(Step::Wait { func: 0, pc: 3 }));
    }

    #[test]
    fn wait_timeout_with_infinity_or_a_non_integer_suspends_at_the_target() {
        // `infinity` (the atom) and any other non-zero term take the same C path:
        // `wait.src`'s `timeout_value == make_small(0)` test is false, so the
        // process waits. Only the *small integer* `0` falls through.
        let module = Module::default();
        let labels = labels_with(Label(3), 0, 3);
        for time in [Operand::Atom(Atom::from_raw(3)), Operand::Nil] {
            let mut machine = Machine::with_max_register(4);
            let mut ctx = StubCtx::new();
            let step = exec(
                &mut machine,
                &module,
                &labels,
                &Instr::WaitTimeout {
                    target: Label(3),
                    time: time.clone(),
                },
                0,
                5,
                &mut ctx,
            )
            .unwrap();
            assert_eq!(step, Some(Step::Wait { func: 0, pc: 3 }), "{time:?}");
        }
    }

    #[test]
    fn timeout_resets_the_scan_and_continues() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::Timeout,
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Next));
        assert_eq!(ctx.calls, vec!["receive_reset"]);
    }

    #[test]
    fn remove_message_unlinks_and_continues() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::RemoveMessage,
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Next));
        assert_eq!(ctx.calls, vec!["receive_remove"]);
    }

    #[test]
    fn recv_marker_reserve_stores_the_opaque_marker() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::RecvMarkerReserve {
                marker: Operand::X(2),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Next));
        assert_eq!(ctx.calls, vec!["receive_marker_reserve"]);
        // The engine stores the marker verbatim — it has no idea what it is.
        assert_eq!(machine.x(2), Some(make_small(90)));
    }

    #[test]
    fn recv_marker_bind_passes_the_marker_and_reference_through() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        machine.set_x(0, make_small(1)).unwrap();
        machine.set_x(1, make_small(2)).unwrap();
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::RecvMarkerBind {
                marker: Operand::X(0),
                reference: Operand::X(1),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();

        assert_eq!(step, Some(Step::Next));
        assert_eq!(ctx.calls, vec!["receive_marker_bind"]);
        assert_eq!(ctx.bound, Some((make_small(1), make_small(2))));
    }

    #[test]
    fn recv_marker_use_and_clear_pass_the_reference_through() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        machine.set_x(1, make_small(5)).unwrap();
        let mut ctx = StubCtx::new();

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::RecvMarkerUse {
                reference: Operand::X(1),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(ctx.used, Some(make_small(5)));

        let step = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::RecvMarkerClear {
                reference: Operand::X(1),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap();
        assert_eq!(step, Some(Step::Next));
        assert_eq!(ctx.cleared, Some(make_small(5)));
        assert_eq!(
            ctx.calls,
            vec!["receive_marker_use", "receive_marker_clear"]
        );
    }

    #[test]
    fn marker_bind_with_an_out_of_range_reference_is_a_typed_error() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        let err = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::RecvMarkerBind {
                marker: Operand::X(0),
                reference: Operand::X(99),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap_err();

        // The operand is read before the host is consulted, so no seam call happens.
        assert!(matches!(err, BeamError::BadArg));
        assert!(ctx.calls.is_empty());
    }

    #[test]
    fn recv_marker_reserve_with_a_non_destination_is_a_typed_error() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        // A literal names a term in the module's immutable area: no addressable slot.
        let err = exec(
            &mut machine,
            &module,
            &labels,
            &Instr::RecvMarkerReserve {
                marker: Operand::Literal(0),
            },
            0,
            0,
            &mut ctx,
        )
        .unwrap_err();

        match err {
            BeamError::Raise(raise) => assert_eq!(
                raise.reason.as_atom(),
                Some("unsupported_opcode: dst operand: literal")
            ),
            other => panic!("expected the typed destination error, got {other:?}"),
        }
    }

    #[test]
    fn a_variant_outside_the_receive_family_is_declined() {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::with_max_register(4);
        let mut ctx = StubCtx::new();

        // `Ok(None)` — the dispatcher then tries the other families (and, when
        // none claims it, reports its own unclaimed-opcode error).
        assert_eq!(
            exec(
                &mut machine,
                &module,
                &labels,
                &Instr::Return,
                0,
                0,
                &mut ctx
            )
            .unwrap(),
            None
        );
        assert!(ctx.calls.is_empty());
    }
}
