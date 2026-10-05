//! Exception / raise instruction family.
//!
//! Owns the in-VM exception instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::Badmatch`] — raise `{badmatch,Value}`
//! - [`Instr::CaseEnd`] — raise `{case_clause,Value}`
//! - [`Instr::IfEnd`] — raise `if_clause`
//! - [`Instr::Raise`] — raise `Value` with a raw stacktrace
//! - [`Instr::Try`] / [`Instr::Catch`] — push a catch frame
//! - [`Instr::TryCase`] / [`Instr::TryEnd`] / [`Instr::TryCaseEnd`] / [`Instr::CatchEnd`]
//! - [`Instr::Badrecord`] — raise `{badrecord,Value}`
//! - [`Instr::BuildStacktrace`] / [`Instr::RawRaise`]
//!
//! The shared machine state this family uses is [`Machine::raise`] (record a
//! pending exception and return the typed `BeamError`) plus the catch-frame stack
//! ([`Machine::push_catch_frame`]/[`Machine::pop_catch_frame`]); unwinding into a
//! handler is a `Step::Goto`, so implementing this family never requires editing
//! `interpreter.rs`.
//!
//! **Milestone status: stub.** [`exec`] claims nothing yet (`Ok(None)`), so the
//! dispatcher reports these opcodes as unsupported — the same observable behavior
//! as before this seam existed.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::Step;

/// Execute an exception-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is an exception opcode this family
/// executes, `Ok(None)` when `instr` is not this family's variant, and `Err` on a
/// raise.
pub(crate) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (machine, module, labels, instr, func, pc, ctx);
    Ok(None)
}
