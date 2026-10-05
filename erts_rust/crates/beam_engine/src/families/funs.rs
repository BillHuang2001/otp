//! Fun / closure instruction family.
//!
//! Owns the closure and fun-application instructions (per
//! `../beam_loader/CONTEXT.md`, "The instruction vocabulary"):
//! - [`Instr::MakeFun3`] — build a local closure from its `FunT` entry
//! - [`Instr::CallFun`] — call the fun in `x(Arity)`
//! - [`Instr::CallFun2`] — call the fun held in a register
//! - [`Instr::Apply`] — apply a fun/`M,F,A` with `Arity` arguments
//! - [`Instr::ApplyLast`] — a tail `apply`
//!
//! The fun-application opcodes reach the host through
//! [`ExecutionContext::apply_fun`](crate::context::ExecutionContext::apply_fun)
//! (default: a typed `unsupported`).
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

/// Execute a fun-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a fun opcode this family executes,
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
