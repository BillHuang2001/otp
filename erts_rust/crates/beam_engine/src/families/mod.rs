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

//! The per-instruction-family dispatch seam.
//!
//! Each [`beam_loader::Instr`] variant that the Milestone-1 interpreter does not
//! execute natively belongs to exactly **one** family submodule here. The
//! interpreter's single catch-all arm (`crate::interpreter`) routes such an
//! instruction to [`dispatch`], which **routes by the instruction's own variant**
//! to the one family that owns it (family ownership is disjoint — see the list
//! below); an instruction **no** family owns — including the families' own
//! not-yet-implemented variants, which are stubs returning `Ok(None)` — yields the
//! documented `unsupported` error, with the unclaimed instruction's `ops.tab`
//! spelling appended so the gap is diagnosable ([`instr_name`]).
//!
//! Ownership being **disjoint** is what lets the routing `match` — a jump table on
//! the [`Instr`] discriminant — hand a dispatched instruction **straight** to its
//! owning family: no non-owning family is ever consulted, so a dispatched
//! instruction costs one jump and the most heavily exercised one, `get_list`,
//! lands in its real owner `lists::exec` on the first hop. Each variant is claimed
//! by the single family the ordering below names for it, so the route is
//! unambiguous.
//!
//! The point of the split is **edit locality**: a later agent implements a whole
//! instruction family inside its own file (e.g. `tuples.rs`) and never has to
//! touch the dispatcher (`interpreter.rs`) again. Every family exposes the same
//! [`exec`](tuples::exec)-shaped signature, so adding a family is one pattern arm
//! here.
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
//! the debug-line opcodes) fall to the routing `match`'s last arm, so [`dispatch`]
//! reports them as unsupported, naming the instruction when [`instr_name`] knows
//! its `ops.tab` spelling.
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
pub(crate) mod funs;
mod lists;
mod maps;
mod receive;
mod records;
mod tuples;

/// Route `instr` to the single instruction family that owns it.
///
/// Ownership is disjoint (see the module docs), so the routing `match` — a jump
/// table on the [`Instr`] discriminant — names exactly one family per variant and
/// the old "probe every family, first `Ok(Some)` wins" loop is gone.
///
/// `Ok(step)` is the owning family's control-flow decision; `Err` is either that
/// family's failure (a raise) or — for a variant **no** family owns, or one a
/// family owns but declines with `Ok(None)` — the documented
/// `unsupported("<UNCLAIMED_PREFIX>: <name>")` error, whose prefix is byte-identical
/// to the message the interpreter emitted before this seam existed and whose
/// `<name>` is [`instr_name`]'s `ops.tab` spelling of the offending instruction (so
/// the unresolvable variant is diagnosable from the error alone).
///
/// `#[inline]`-marked: the router is the single call site of every family, and the
/// interpreter reaches it from its catch-all arm, so inlining it into the dispatch
/// loop lets the optimizer fold the (now redundant) per-family `match` into the
/// loop's own control flow instead of a second out-of-line dispatch.
#[inline]
pub(crate) fn dispatch(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    let claimed = match instr {
        // tuples
        Instr::PutTuple2 { .. }
        | Instr::GetTupleElement { .. }
        | Instr::SetTupleElement { .. }
        | Instr::SelectTupleArity { .. } => {
            tuples::exec(machine, module, labels, instr, func, pc, ctx)
        }
        // records (tagged-tuple update + the native-record set)
        Instr::UpdateRecord(_)
        | Instr::IsAnyNativeRecord { .. }
        | Instr::IsNativeRecord { .. }
        | Instr::GetRecordElements { .. }
        | Instr::GetRecordElementsId { .. }
        | Instr::PutRecord(_)
        | Instr::IsRecordAccessible { .. }
        | Instr::GetRecordField(_)
        | Instr::UpdateRecordId(_) => {
            records::exec(machine, module, labels, instr, func, pc, ctx)
        }
        // lists
        Instr::PutList { .. } | Instr::GetList { .. } | Instr::GetHd { .. } | Instr::GetTl { .. } => {
            lists::exec(machine, module, labels, instr, func, pc, ctx)
        }
        // funs
        Instr::MakeFun3 { .. }
        | Instr::CallFun { .. }
        | Instr::CallFun2 { .. }
        | Instr::Apply { .. }
        | Instr::ApplyLast { .. } => funs::exec(machine, module, labels, instr, func, pc, ctx),
        // exceptions
        Instr::Try { .. }
        | Instr::Catch { .. }
        | Instr::Badmatch { .. }
        | Instr::CaseEnd { .. }
        | Instr::TryCaseEnd { .. }
        | Instr::Badrecord { .. }
        | Instr::IfEnd
        | Instr::Raise { .. }
        | Instr::TryCase { .. }
        | Instr::TryEnd { .. }
        | Instr::CatchEnd { .. }
        | Instr::BuildStacktrace
        | Instr::RawRaise => exceptions::exec(machine, module, labels, instr, func, pc, ctx),
        // floats
        Instr::Fmove { .. }
        | Instr::Fconv { .. }
        | Instr::Fadd { .. }
        | Instr::Fsub { .. }
        | Instr::Fmul { .. }
        | Instr::Fdiv { .. }
        | Instr::Fnegate { .. } => floats::exec(machine, module, labels, instr, func, pc, ctx),
        // maps
        Instr::GetMapElements { .. }
        | Instr::HasMapFields { .. }
        | Instr::PutMapAssoc(_)
        | Instr::PutMapExact(_) => maps::exec(machine, module, labels, instr, func, pc, ctx),
        // bitstrings (the whole `Bs*` family)
        Instr::BsStartMatch3 { .. }
        | Instr::BsStartMatch4 { .. }
        | Instr::BsGetPosition { .. }
        | Instr::BsSetPosition { .. }
        | Instr::BsGetTail { .. }
        | Instr::BsTestTail2 { .. }
        | Instr::BsGetInteger2(_)
        | Instr::BsGetBinary2(_)
        | Instr::BsSkipBits2 { .. }
        | Instr::BsMatch { .. }
        | Instr::BsMatchString { .. }
        | Instr::BsGetFloat2(_)
        | Instr::BsGetUtf8 { .. }
        | Instr::BsGetUtf16 { .. }
        | Instr::BsGetUtf32 { .. }
        | Instr::BsSkipUtf8 { .. }
        | Instr::BsSkipUtf16 { .. }
        | Instr::BsSkipUtf32 { .. }
        | Instr::BsInitWritable
        | Instr::BsCreateBin { .. } => {
            bitstrings::exec(machine, module, labels, instr, func, pc, ctx)
        }
        // receive / message
        Instr::Send
        | Instr::LoopRec { .. }
        | Instr::LoopRecEnd { .. }
        | Instr::Wait { .. }
        | Instr::WaitTimeout { .. }
        | Instr::Timeout
        | Instr::RemoveMessage
        | Instr::RecvMarkerReserve { .. }
        | Instr::RecvMarkerBind { .. }
        | Instr::RecvMarkerUse { .. }
        | Instr::RecvMarkerClear { .. } => {
            receive::exec(machine, module, labels, instr, func, pc, ctx)
        }
        // A variant owned by no family (a core arm's instruction, or the metadata
        // no-ops): fall through to the documented sentinel below.
        _ => Ok(None),
    };
    match claimed? {
        Some(step) => Ok(step),
        None => Err(error::unsupported(&format!(
            "{UNCLAIMED_PREFIX}: {}",
            instr_name(instr)
        ))),
    }
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
        // family's own error — here the receive seam's typed `unsupported`, which
        // the family translates into a rejection naming the opcode (the bare
        // `MinimalContext` below has no mailbox, so it does not serve markers).
        let labels = LabelMap::new();
        let cases: [(Instr, &str); 4] = [
            (
                Instr::RecvMarkerReserve {
                    marker: Operand::X(0),
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
                Instr::RecvMarkerClear {
                    reference: Operand::X(1),
                },
                "recv_marker_clear",
            ),
            (
                Instr::RecvMarkerUse {
                    reference: Operand::X(1),
                },
                "recv_marker_use",
            ),
        ];
        for (instr, opcode) in cases {
            let reason = error_reason(dispatch_step(&instr, &labels));
            assert_eq!(
                reason,
                format!("unsupported_opcode: {opcode}: the host does not serve receive markers")
            );
            assert!(
                !reason.contains(UNCLAIMED_PREFIX),
                "{opcode} is claimed, so the unclaimed sentinel must be gone"
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
