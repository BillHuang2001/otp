//! Map instruction family.
//!
//! Owns the map-access/update instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::GetMapElements`] — fetch map elements (failing when a key is absent)
//! - [`Instr::HasMapFields`] — test for the presence of map keys
//! - [`Instr::PutMapAssoc`] — update/build a map (associating new keys)
//! - [`Instr::PutMapExact`] — update a map (failing on an unknown key)
//!
//! Map terms are read/built with the `crate::terms` flatmap helpers.
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

/// Execute a map-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a map opcode this family executes,
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
