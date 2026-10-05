//! The per-instruction-family dispatch seam.
//!
//! Each [`beam_loader::Instr`] variant that the Milestone-1 interpreter does not
//! execute natively belongs to exactly **one** family submodule here. The
//! interpreter's single catch-all arm (`crate::interpreter`) routes such an
//! instruction to [`dispatch`], which tries every family in order and returns the
//! first [`Step`] one claims; an instruction no family claims — including the
//! families' own not-yet-implemented variants, which are stubs returning
//! `Ok(None)` — yields the same documented `unsupported` error the interpreter
//! emitted before this seam existed.
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
//! - [`lists`] — `PutList`/`GetList`/`GetHd`/`GetTl`
//! - [`funs`] — `MakeFun3`/`CallFun`/`CallFun2`/`Apply`/`ApplyLast`
//! - [`exceptions`] — `Badmatch`/`CaseEnd`/`IfEnd`/`Raise`/`Try`/`Catch`/`TryCase`/`TryEnd`/`TryCaseEnd`/`CatchEnd`/`Badrecord`/`BuildStacktrace`/`RawRaise`
//! - [`floats`] — `Fmove`/`Fconv`/`Fadd`/`Fsub`/`Fmul`/`Fdiv`/`Fnegate`
//! - [`maps`] — `GetMapElements`/`HasMapFields`/`PutMapAssoc`/`PutMapExact`
//! - [`bitstrings`] — the `Bs*` family
//!
//! Variants owned by **no** family (`Bif0`/`Bif`, `Jump`, `InitYregs`,
//! `UpdateRecord`, the receive/message opcodes, the debug-line opcodes,
//! `NifStart`/`OnLoad`) are not claimed by any submodule, so [`dispatch`] reports
//! them as unsupported exactly as before.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::Step;

mod bitstrings;
mod exceptions;
mod floats;
mod funs;
mod lists;
mod maps;
mod tuples;

/// Dispatch `instr` to the first instruction family that claims it.
///
/// Families are tried in a fixed order (see the module docs). `Ok(step)` is the
/// control-flow decision of the claiming family; `Err` is either a family failure
/// (a raise) or — when **no** family claims the instruction — the documented
/// `unsupported("widened opcode not yet executed")` error, byte-identical to the
/// message the interpreter emitted before this seam existed.
pub(crate) fn dispatch(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Step> {
    if let Some(step) = tuples::exec(machine, module, labels, instr, func, pc, ctx)? {
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
    Err(error::unsupported("widened opcode not yet executed"))
}
