//! Residual [`beam_loader::Instr::Generic`] opcodes the interpreter executes.
//!
//! The loader preserves every opcode it does not model as a typed IR variant as
//! [`beam_loader::Instr::Generic`] (`beam_loader`'s "fallback" section). The
//! interpreter handles the few whose semantics are trivial and whose absence
//! would make ordinary compiler output unrunnable, and reports every other one
//! as the documented typed `unsupported` error carrying the opcode's name — the
//! "never a wrong result" invariant is unchanged.
//!
//! Current coverage:
//! - `is_any_native_record Fail Src` — the `is_record/1` guard on native records
//!   (OTP 29).
//!
//! Handoff for the loader (not this crate's to make — the loader IR is frozen
//! this round): `is_any_native_record` has the fixed `f xy` operand shape
//! (`emu/ops.tab:1634`) and a `Test`-style failure branch, exactly like the
//! `is_*` guards that *are* lowered to [`beam_loader::Instr::Test`]. It is
//! written below so its body can back such a typed variant verbatim once the
//! loader starts emitting one.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Label, Opcode, Operand, NO_FAIL};

use crate::error;
use crate::interpreter::resolve;
use crate::machine::Machine;
use crate::operand::read_operand;
use crate::predicates;
use crate::Step;

/// Execute a residual [`beam_loader::Instr::Generic`] opcode, or report it as
/// unsupported.
///
/// `Err(error::unsupported(op.name()))` is the documented typed error every
/// opcode this module has no implementation for keeps — never a panic and never
/// a silently wrong result.
pub(crate) fn exec(
    machine: &Machine,
    labels: &BTreeMap<Label, (u32, u32)>,
    op: Opcode,
    operands: &[Operand],
) -> Result<Step> {
    match op {
        Opcode::is_any_native_record => exec_is_any_native_record(machine, labels, operands),
        other => Err(error::unsupported(other.name())),
    }
}

/// `is_any_native_record Fail Src` — `Src` is a native record.
///
/// The C body (`emu/instrs.tab:1323`) is
/// `is_any_native_record(Fail, Src) { if (!is_record($Src)) { $FAIL($Fail); } }`:
/// the test **succeeds** (falls through to the next instruction) iff `Src` is a
/// native record, and otherwise **fails** (branches to `Fail`). Operand order is
/// the file's `f xy` (`emu/ops.tab:1634`; the disassembler's `[Location, Src]`,
/// `beam_disasm.erl:1400`), i.e. `operands[0]` = `Fail` label, `operands[1]` =
/// `Src`.
///
/// `is_record(x)` is `is_boxed(x) && is_record_header(*boxed_val(x))`
/// (`erl_term.h:437-438`) — the `RECORD_SUBTAG` boxed thing. It is **not** a
/// tuple test: a tuple's header carries the `ARITYVAL` subtag, so
/// `is_record({a,b,c})` is `false` (`erlang:is_record/1`).
fn exec_is_any_native_record(
    machine: &Machine,
    labels: &BTreeMap<Label, (u32, u32)>,
    operands: &[Operand],
) -> Result<Step> {
    let fail = match operands.first() {
        Some(Operand::Label(label)) => *label,
        _ => return Err(error::unsupported("is_any_native_record: fail operand")),
    };
    let src = match operands.get(1) {
        Some(operand) => read_operand(machine, operand)?,
        None => return Err(error::unsupported("is_any_native_record: missing source")),
    };
    if predicates::is_record(src) {
        Ok(Step::Next)
    } else if fail == NO_FAIL {
        // No failure label means the compiler promised the test can never fail;
        // it did, so report it rather than falling through with a wrong result.
        Err(error::unsupported("is_any_native_record/no_fail"))
    } else {
        let (func, pc) = resolve(labels, fail)?;
        Ok(Step::Goto { func, pc })
    }
}
