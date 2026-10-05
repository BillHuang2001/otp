//! Record instruction family.
//!
//! Owns the native record-update instruction (per `../beam_loader/CONTEXT.md`,
//! "The instruction vocabulary"):
//! - [`Instr::UpdateRecord`] — copy a record tuple with fields replaced
//!
//! Ground truth (read-only): `emu/instrs.tab` `i_update_record_copy` (`:785`) /
//! `i_update_record_in_place` (`:804`) / `i_update_record_continue` (`:836`), and
//! the `update_record` transform + its generator in `emu/ops.tab:1611` and
//! `emu/generators.tab:1028`.
//!
//! ## Semantics
//! `UpdateRecord { hint, size, src, dst, updates }` is the OTP-26 `update_record`
//! instruction the compiler emits for `#r{field = Value}` record updates. `src`
//! names a tuple of arity `size` (the tagged record tuple), `updates` is the
//! `(offset, value)` list in file order and `dst` receives the updated tuple.
//!
//! - **`hint` is the compiler's destructive-update reuse hint** (`copy`/`reuse`/
//!   `inplace`, `lib/compiler/src/genop.tab`). Milestone 1 has **no** in-place
//!   reuse, so the hint is ignored and the result is *always* a fresh copy. This is
//!   observationally equivalent to C's in-place path: the compiler only emits
//!   `inplace` for a `src` tuple that is unique/dead at that point
//!   (`beam_ssa_destructive_update.erl`), so no live term ever observes the
//!   difference — while a copy can never clobber a tuple that is still shared.
//! - **`offset` is 1-based** (`HTOP[$Offset]` with `HTOP[0]` the arityval header,
//!   `emu/generators.tab:1043-1047`), i.e. tuple element `offset - 1`.
//! - **Copy-then-patch.** C copies the whole tuple up-front and then overwrites
//!   each replaced field (`i_update_record_copy`); that order matters because an
//!   update value operand may alias `dst` — which is exactly why the compiler
//!   stashes the old destination in `SCRATCH_X_REG` and rewrites later update
//!   operands to it (`emu/generators.tab:1064-1081`). Reading every source element
//!   before writing `dst` reproduces that order without a scratch register:
//!   an update value that reads `dst` still sees the *original* tuple.
//!
//! ## Failure model (never a panic, never a wrong result)
//! `update_record` is only emitted behind an `is_tagged_tuple` test of the same
//! fail label (`{test,is_tagged_tuple,{f,N},[…]}` + `badrecord` at `N`), so the C
//! opcode reads the tuple unchecked. This family is the **safety net**: a source
//! that is not a tuple, a `size` that does not match the tuple's arity, a
//! non-integer update offset, or an offset outside `1..=size` is a documented
//! typed [`beam_common::BeamError`] (`error::unsupported`) — the same convention
//! as `select_tuple_arity`'s non-tuple path. `read_operand`/`write_operand` and the
//! `terms` readers/writers propagate their own typed errors.

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::terms;
use crate::Step;

/// Execute a record-family instruction.
///
/// Returns `Ok(Some(step))` when `instr` is a record opcode this family executes,
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
    let _ = (module, labels, func, pc);
    match instr {
        Instr::UpdateRecord {
            // The reuse hint is intentionally ignored: M1 never updates in place
            // (see the module docs).
            hint: _,
            size,
            src,
            dst,
            updates,
        } => {
            let source = read_operand(machine, src)?;
            let arity = terms::tuple_arity(source)
                .map_err(|_| error::unsupported("update_record: non-tuple source"))?;
            if arity != *size as usize {
                return Err(error::unsupported("update_record: size/arity mismatch"));
            }

            // Copy the source tuple first (arities may exceed a fixed-size array,
            // and this is the one place the family reads raw element slots).
            let mut elements = Vec::with_capacity(arity);
            for index in 0..arity {
                elements.push(terms::tuple_element(source, index)?);
            }

            for (offset, value) in updates {
                let offset = match offset {
                    Operand::Integer(offset) => *offset,
                    _ => return Err(error::unsupported("update_record: non-integer index")),
                };
                // `Offset` is 1-based: element `Offset - 1` of the tuple.
                if offset < 1 || offset as usize > arity {
                    return Err(error::unsupported("update_record: index out of range"));
                }
                elements[offset as usize - 1] = read_operand(machine, value)?;
            }

            let result = terms::make_tuple(ctx, &elements)?;
            write_operand(machine, dst, result)?;
            Ok(Some(Step::Next))
        }

        _ => Ok(None),
    }
}
