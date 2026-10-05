//! Floating-point instruction family.
//!
//! Owns the float-register (FR) arithmetic instructions (per
//! `../beam_loader/CONTEXT.md`, "The instruction vocabulary"):
//! - [`Instr::Fmove`] — move between a float register and a value register
//! - [`Instr::Fconv`] — convert a number into a float register
//! - [`Instr::Fadd`] / [`Instr::Fsub`] / [`Instr::Fmul`] / [`Instr::Fdiv`]
//! - [`Instr::Fnegate`]
//!
//! These read/write the machine's FR file ([`Machine::get_fr`]/[`Machine::set_fr`])
//! and use the boxed-float term helpers in `crate::terms`.
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

/// Execute a float-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a float opcode this family executes,
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
