//! Bitstring / binary-syntax instruction family.
//!
//! Owns the whole `Bs*` instruction family (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - match entry: [`Instr::BsStartMatch3`], [`Instr::BsStartMatch4`]
//! - matching: [`Instr::BsMatch`], [`Instr::BsMatchString`], [`Instr::BsGetTail`],
//!   [`Instr::BsGetPosition`], [`Instr::BsSetPosition`], [`Instr::BsGetInteger2`],
//!   [`Instr::BsGetFloat2`], [`Instr::BsGetBinary2`], [`Instr::BsSkipBits2`],
//!   [`Instr::BsTestTail2`]
//! - UTF: [`Instr::BsGetUtf8`], [`Instr::BsGetUtf16`], [`Instr::BsGetUtf32`],
//!   [`Instr::BsSkipUtf8`], [`Instr::BsSkipUtf16`], [`Instr::BsSkipUtf32`]
//! - construction: [`Instr::BsInitWritable`], [`Instr::BsCreateBin`]
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

/// Execute a bitstring-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a `Bs*` opcode this family executes,
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
