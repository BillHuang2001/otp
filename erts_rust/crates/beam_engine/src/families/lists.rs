//! List instruction family.
//!
//! Owns the cons-cell instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::PutList`] — build a cons cell
//! - [`Instr::GetList`] — split a cons cell into head and tail
//! - [`Instr::GetHd`] — fetch the head (car) of a cons cell
//! - [`Instr::GetTl`] — fetch the tail (cdr) of a cons cell
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

/// Execute a list-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a list opcode this family executes,
/// `Ok(None)` when `instr` is not this family's variant, and `Err` on a raise.
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
