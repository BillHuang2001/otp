//! The per-instruction-family dispatch seam.
//!
//! Each [`beam_loader::Instr`] variant that the Milestone-1 interpreter does not
//! execute natively belongs to exactly **one** family submodule here. The
//! interpreter's single catch-all arm (`crate::interpreter`) routes such an
//! instruction to [`dispatch`], which tries every family in order and returns the
//! first [`Step`] one claims; an instruction no family claims — including the
//! families' own not-yet-implemented variants, which are stubs returning
//! `Ok(None)` — yields the same documented `unsupported` error the interpreter
//! emitted before this seam existed, with the unclaimed instruction's `ops.tab`
//! spelling appended so the gap is diagnosable ([`instr_name`]).
//!
//! The point of the split is **edit locality**: a later agent implements a whole
//! instruction family inside its own file (e.g. `tuples.rs`) and never has to
//! touch the dispatcher (`interpreter.rs`) again. Every family exposes the same
//! [`exec`](tuples::exec)-shaped signature, so adding a family is a one-line
//! change here.
//!
//! Family ownership (per `../beam_loader/CONTEXT.md`, "The instruction
//! vocabulary"):
//! - [`tuples`] — `PutTuple2`/`GetTupleElement`/`SelectTupleArity`
//! - [`records`] — `UpdateRecord`/`IsAnyNativeRecord` plus the native-record set `IsNativeRecord`/`GetRecordElements`/`GetRecordElementsId`/`PutRecord`/`IsRecordAccessible`/`GetRecordField`/`UpdateRecordId`
//! - [`lists`] — `PutList`/`GetList`/`GetHd`/`GetTl`
//! - [`funs`] — `MakeFun3`/`CallFun`/`CallFun2`/`Apply`/`ApplyLast`
//! - [`exceptions`] — `Badmatch`/`CaseEnd`/`IfEnd`/`Raise`/`Try`/`Catch`/`TryCase`/`TryEnd`/`TryCaseEnd`/`CatchEnd`/`Badrecord`/`BuildStacktrace`/`RawRaise`
//! - [`floats`] — `Fmove`/`Fconv`/`Fadd`/`Fsub`/`Fmul`/`Fdiv`/`Fnegate`
//! - [`maps`] — `GetMapElements`/`HasMapFields`/`PutMapAssoc`/`PutMapExact`
//! - [`bitstrings`] — the `Bs*` family
//! - [`receive`] — `Send`/`LoopRec`/`LoopRecEnd`/`Wait`/`WaitTimeout`/`Timeout`/`RemoveMessage`/`RecvMarkerReserve`/`RecvMarkerBind`/`RecvMarkerUse`/`RecvMarkerClear`
//!
//! Variants owned by **no** family (`Bif0`/`Bif`, `Jump`, `InitYregs`,
//! the debug-line opcodes) are not claimed by any submodule, so [`dispatch`]
//! reports them as unsupported exactly as before — naming the instruction when
//! [`instr_name`] knows its `ops.tab` spelling.
//! (`NifStart`/`OnLoad` never reach [`dispatch`]: the interpreter executes them
//! as no-ops in its metadata arm, and the residual `Instr::Generic` opcodes are
//! handled by `crate::generic` before the catch-all arm too.)

use beam_common::Result;
use beam_loader::{Instr, Module};

use crate::context::ExecutionContext;
use crate::error;
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::Step;

/// The stable prefix of the error [`dispatch`] reports for an instruction **no**
/// family claims.
///
/// It is the exact sentinel the interpreter emitted before the family seam
/// existed, so callers that match on it keep working; [`dispatch`] appends
/// `": <name>"` (the instruction's `ops.tab` spelling, see [`instr_name`]).
pub(crate) const UNCLAIMED_PREFIX: &str = "widened opcode not yet executed";
mod bitstrings;
pub(crate) mod exceptions;
mod floats;
mod funs;
mod lists;
mod maps;
mod receive;
mod records;
mod tuples;

/// Dispatch `instr` to the first instruction family that claims it.
///
/// Families are tried in a fixed order (see the module docs). `Ok(step)` is the
/// control-flow decision of the claiming family; `Err` is either a family failure
/// (a raise) or — when **no** family claims the instruction — the documented
/// `unsupported("<UNCLAIMED_PREFIX>: <name>")` error,
/// whose prefix is byte-identical to the message the interpreter emitted before
/// this seam existed and whose `<name>` is [`instr_name`]'s `ops.tab` spelling of
/// the offending instruction (so the unresolvable variant is diagnosable from the
/// error alone).
pub(crate) fn dispatch(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    if let Some(step) = tuples::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = records::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = lists::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = funs::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = exceptions::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = floats::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = maps::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = bitstrings::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    if let Some(step) = receive::exec(machine, module, labels, instr, func, pc, ctx)? {
        return Ok(step);
    }
    Err(error::unsupported(&format!(
        "{UNCLAIMED_PREFIX}: {}",
        instr_name(instr)
    )))
}

/// The `ops.tab`/C spelling of the instruction [`dispatch`] could not claim.
///
/// [`beam_loader::Instr`] carries no name (unlike [`beam_loader::Opcode::name`],
/// which the residual [`Instr::Generic`] path already uses in `crate::generic`),
/// so the spelling is mapped here for the variants the interpreter can actually
/// reach the sentinel with: the metadata/`bif` variants the core loop executes but
/// a family never claims. Anything else reports the generic `"instruction"`.
///
/// The spellings are the `ops.tab` opcode names (`../../erts/emulator/beam/emu/ops.tab`),
/// i.e. exactly what `beam_disasm` prints, so an operator can search the compiler
/// output for it.
fn instr_name(instr: &Instr) -> &'static str {
    match instr {
        Instr::Bif0 { .. } => "bif0",
        Instr::Bif { .. } => "bif",
        Instr::InitYregs { .. } => "init_yregs",
        Instr::ExecutableLine { .. } => "executable_line",
        Instr::DebugLine { .. } => "debug_line",
        _ => "instruction",
    }
}

#[cfg(test)]
mod tests {
    //! The diagnostics contract of the family seam: an unclaimed instruction must
    //! name itself in the error (the objective of this module's `instr_name`), and
    //! the long-standing sentinel prefix must stay byte-identical. The receive
    //! family's tests are the converse: a variant `families::receive` owns must
    //! **never** reach the sentinel again.

    use beam_loader::{Atom, ImportRef, Label, Opcode, Operand, NO_FAIL};

    use super::*;
    use crate::context::MinimalContext;

    /// Dispatch `instr` through the real seam (every family declines it) and return
    /// the typed error. Machine/module/labels/context are never consulted by a
    /// declining family, so defaults suffice.
    fn dispatch_error(instr: &Instr) -> beam_common::BeamError {
        let module = Module::default();
        let labels = LabelMap::new();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        dispatch(&mut machine, &module, &labels, instr, 0, 0, &mut ctx)
            .expect_err("no family claims this instruction")
    }

    /// The reason atom of the unclaimed-instruction error.
    fn dispatch_reason(instr: &Instr) -> String {
        match dispatch_error(instr) {
            beam_common::BeamError::Raise(raise) => raise
                .reason
                .as_atom()
                .expect("the reason is an atom")
                .to_string(),
            other => panic!("expected the unclaimed-instruction raise, got {other:?}"),
        }
    }

    /// Dispatch `instr` through the real seam with `labels`, returning whatever it
    /// produced: the claiming family's [`Step`], or a typed error.
    ///
    /// Unlike [`dispatch_error`] this does not require the sentinel — it is how the
    /// "the receive family claims this variant now" tests observe the family's own
    /// decision.
    fn dispatch_step(
        instr: &Instr,
        labels: &LabelMap,
    ) -> beam_common::Result<Step> {
        let module = Module::default();
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::new();
        dispatch(&mut machine, &module, labels, instr, 0, 0, &mut ctx)
    }

    /// The `unsupported_opcode: <name>` reason of a typed error.
    fn error_reason(result: beam_common::Result<Step>) -> String {
        match result.expect_err("expected a typed error") {
            beam_common::BeamError::Raise(raise) => raise
                .reason
                .as_atom()
                .expect("the reason is an atom")
                .to_string(),
            other => panic!("expected a typed raise, got {other:?}"),
        }
    }

    #[test]
    fn the_prefix_is_unchanged() {
        // The sentinel callers (and the differential harness) match on is frozen.
        assert_eq!(UNCLAIMED_PREFIX, "widened opcode not yet executed");
    }

    #[test]
    fn the_receive_marker_opcodes_are_claimed_by_the_receive_family() {
        // The four `recv_marker_*` opcodes are what a real `.beam` (e.g. kernel
        // `code.beam`) hits first. They used to fall through to the unclaimed
        // sentinel; [`super::receive`] owns them now, so `dispatch` reports that
        // family's own error — here the receive seam's typed `unsupported`, since
        // the bare `MinimalContext` below has no mailbox installed.
        let labels = LabelMap::new();
        let cases: [(Instr, &str); 4] = [
            (
                Instr::RecvMarkerReserve {
                    marker: Operand::X(0),
                },
                "receive_marker_reserve",
            ),
            (
                Instr::RecvMarkerBind {
                    marker: Operand::X(0),
                    reference: Operand::X(1),
                },
                "receive_marker_bind",
            ),
            (
                Instr::RecvMarkerClear {
                    reference: Operand::X(1),
                },
                "receive_marker_clear",
            ),
            (
                Instr::RecvMarkerUse {
                    reference: Operand::X(1),
                },
                "receive_marker_use",
            ),
        ];
        for (instr, method) in cases {
            let reason = error_reason(dispatch_step(&instr, &labels));
            assert_eq!(reason, format!("unsupported_opcode: {method}"));
            assert!(
                !reason.contains(UNCLAIMED_PREFIX),
                "{method} is claimed, so the unclaimed sentinel must be gone"
            );
        }
    }

    #[test]
    fn the_rest_of_the_receive_family_is_claimed_too() {
        // `loop_rec`/`loop_rec_end` jump through module labels, so give them one.
        let labels = LabelMap::from([(Label(3), (0u32, 3u32)), (Label(4), (0u32, 4u32))]);

        // The send/mailbox opcodes reach the receive seam, whose typed
        // `unsupported` names the method (no mailbox is installed here).
        let cases: [(Instr, &str); 5] = [
            (Instr::Send, "send_message"),
            (
                Instr::LoopRec {
                    fail: Label(4),
                    dst: Operand::X(0),
                },
                "receive_next",
            ),
            (Instr::LoopRecEnd { target: Label(3) }, "receive_skip"),
            (Instr::Timeout, "receive_reset"),
            (Instr::RemoveMessage, "receive_remove"),
        ];
        for (instr, method) in cases {
            let reason = error_reason(dispatch_step(&instr, &labels));
            assert_eq!(reason, format!("unsupported_opcode: {method}"));
            assert!(!reason.contains(UNCLAIMED_PREFIX));
        }

        // `wait`/`wait_timeout` need no mailbox at all: they execute and report the
        // suspension (or the zero-timeout fall-through) the driver acts on.
        assert_eq!(
            dispatch_step(&Instr::Wait { target: Label(3) }, &labels).unwrap(),
            Step::Wait { func: 0, pc: 3 }
        );
        assert_eq!(
            dispatch_step(
                &Instr::WaitTimeout {
                    target: Label(3),
                    time: Operand::Integer(0),
                },
                &labels
            )
            .unwrap(),
            Step::Goto { func: 0, pc: 1 }
        );
    }

    #[test]
    fn the_unclaimed_bif_and_metadata_variants_are_named() {
        // The core loop executes these, but no family claims them — so if one ever
        // did reach `dispatch` it must still name itself.
        let cases: [(Instr, &str); 5] = [
            (
                Instr::Bif0 {
                    bif: ImportRef(0),
                    dst: Operand::X(0),
                },
                "bif0",
            ),
            (
                Instr::Bif {
                    arity: 2,
                    bif: ImportRef(0),
                    fail: NO_FAIL,
                    args: vec![Operand::X(0), Operand::X(1)],
                    dst: Operand::X(0),
                },
                "bif",
            ),
            (
                Instr::InitYregs {
                    registers: vec![Operand::Y(0)],
                },
                "init_yregs",
            ),
            (
                Instr::ExecutableLine {
                    location: 0,
                    index: 0,
                },
                "executable_line",
            ),
            (
                Instr::DebugLine {
                    kind: Operand::Atom(Atom::MAX),
                    location: 0,
                    index: 0,
                    live: 1,
                },
                "debug_line",
            ),
        ];
        for (instr, name) in cases {
            assert_eq!(instr_name(&instr), name);
            assert_eq!(
                dispatch_reason(&instr),
                format!("unsupported_opcode: {UNCLAIMED_PREFIX}: {name}")
            );
        }
    }

    #[test]
    fn a_variant_without_a_spelling_falls_back_to_instruction() {
        // `Instr::Generic` is normally handled by `crate::generic` (which names its
        // opcode), so it has no entry in `instr_name`; the fallback must still name
        // *something* rather than producing a dangling separator.
        let generic = Instr::Generic {
            op: Opcode::send,
            operands: Vec::new(),
            offset: 4,
        };
        assert_eq!(instr_name(&generic), "instruction");
        assert_eq!(
            dispatch_reason(&generic),
            format!("unsupported_opcode: {UNCLAIMED_PREFIX}: instruction")
        );
    }
}
