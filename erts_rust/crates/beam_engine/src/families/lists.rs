//! List instruction family.
//!
//! Owns the cons-cell instructions (per `../beam_loader/CONTEXT.md`, "The
//! instruction vocabulary"):
//! - [`Instr::PutList`] — build a cons cell
//! - [`Instr::GetList`] — split a cons cell into head and tail
//! - [`Instr::GetHd`] — fetch the head (car) of a cons cell
//! - [`Instr::GetTl`] — fetch the tail (cdr) of a cons cell
//!
//! Ground truth (read-only): `emu/instrs.tab` `put_list` (`:732`), `get_list`
//! (`:389`), `get_hd` (`:398`) and `get_tl` (`:403`).
//!
//! ## Semantics
//! - `PutList { head, tail, dst }` — build `[Head | Tail]` and store it in `dst`.
//!   `Tail` need not be a list: an improper list is a legal term.
//! - `GetList { source, head, tail }` / `GetHd { source, head }` /
//!   `GetTl { source, tail }` — `source` must be a cons cell (a non-empty list;
//!   `[]` is *not* one). The C opcodes trust a preceding `is_nonempty_list` test
//!   and read the cell unchecked, so this is the **safety net**: a non-cons source
//!   raises `{badmatch, Source}` (class `error`).
//!
//! Every failure is a typed [`beam_common::BeamError`]: `write_operand`/
//! `read_operand` propagate, an unsupported operand is `error::unsupported`, and
//! the `{badmatch, V}` raise is `crate::terms::badmatch_reason` +
//! [`Machine::throw_error`] (a `Step::Goto` into a live catch frame, or a typed
//! `Err` when there is none). Never a panic.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::terms;
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
    let _ = (labels, func, pc);
    match instr {
        Instr::PutList { head, tail, dst } => {
            let h = read_operand(machine, head)?;
            let t = read_operand(machine, tail)?;
            let cons = terms::make_cons(ctx, h, t)?;
            write_operand(machine, dst, cons)?;
            Ok(Some(Step::Next))
        }

        Instr::GetList { source, head, tail } => {
            let value = read_operand(machine, source)?;
            match (terms::cons_head(value), terms::cons_tail(value)) {
                (Ok(h), Ok(t)) => {
                    write_operand(machine, head, h)?;
                    write_operand(machine, tail, t)?;
                    Ok(Some(Step::Next))
                }
                // `[]` or a non-list is not a cons cell.
                _ => machine
                    .throw_error(terms::badmatch_reason(ctx, module, value)?)
                    .map(Some),
            }
        }

        Instr::GetHd { source, head } => {
            let value = read_operand(machine, source)?;
            match terms::cons_head(value) {
                Ok(h) => {
                    write_operand(machine, head, h)?;
                    Ok(Some(Step::Next))
                }
                Err(_) => machine
                    .throw_error(terms::badmatch_reason(ctx, module, value)?)
                    .map(Some),
            }
        }

        Instr::GetTl { source, tail } => {
            let value = read_operand(machine, source)?;
            match terms::cons_tail(value) {
                Ok(t) => {
                    write_operand(machine, tail, t)?;
                    Ok(Some(Step::Next))
                }
                Err(_) => machine
                    .throw_error(terms::badmatch_reason(ctx, module, value)?)
                    .map(Some),
            }
        }

        _ => Ok(None),
    }
}
