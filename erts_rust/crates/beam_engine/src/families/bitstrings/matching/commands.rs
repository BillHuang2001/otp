// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! The flattened `bs_match` command interpreter.
//!
//! `bs_match`'s `commands` operand is the raw flattened list the loader preserved;
//! decoding the records is this module's job. A `pos` cursor advances locally through
//! the commands and is written back into the context once the whole list has
//! succeeded. `ensure_at_least`/`ensure_exactly` only test; the reads snapshot a
//! value into a register.
use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::super::{
    decode_flags, extract_bits, fail_step, is_all, raw, read_size, read_uint_be, small_value,
};
use super::match_ctx::read_ctx;
use super::reads::read_integer_field;
// ---------------------------------------------------------------------------
// The `bs_match` command interpreter
// ---------------------------------------------------------------------------
/// Execute a `bs_match` command list against the context in `ctx_op`.
pub(crate) fn run_commands(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    fail: Label,
    ctx_op: &Operand,
    commands: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
    let bytes = raw::subject_bytes(orig)?;
    let mut pos = start;
    let mut i = 0usize;
    while i < commands.len() {
        let Operand::Atom(command) = &commands[i] else {
            return Err(error::unsupported(
                "bs_match: command list does not start with an atom",
            ));
        };
        let name = module
            .atom_name(*command)
            .ok_or_else(|| error::unsupported("bs_match: unknown command atom"))?;
        match name {
            // `ensure_at_least Size Unit` (`generators.tab:1164`).
            "ensure_at_least" => {
                let size = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
                let unit = read_size(machine, ctx, cmd_at(commands, i + 2)?)?;
                if !ensure_at_least(end.saturating_sub(pos), size, unit) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                i += 3;
            }
            // `ensure_exactly Stride` — `bs_test_tail_imm2` (`generators.tab:1210`).
            "ensure_exactly" => {
                let stride = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
                if end.saturating_sub(pos) != stride {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                i += 2;
            }
            // `skip Stride` — advance without binding (`i_bs_skip`).
            "skip" => {
                let stride = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
                let Some(next) = pos.checked_add(stride) else {
                    return fail_step(machine, module, labels, fail, term, ctx);
                };
                if next > end {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                pos = next;
                i += 2;
            }
            // `integer Live Flags Size Unit Dst` (`i_bs_get_fixed_integer`).
            "integer" => {
                let flags = decode_flags(module, cmd_at(commands, i + 2)?)?;
                let size = read_size(machine, ctx, cmd_at(commands, i + 3)?)?;
                let unit = read_size(machine, ctx, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let Some(nbits) = size.checked_mul(unit) else {
                    return fail_step(machine, module, labels, fail, term, ctx);
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                let value = read_integer_field(ctx, bytes, pos, nbits, flags)?;
                write_operand(machine, dst, value)?;
                pos += nbits;
                i += 6;
            }
            // `binary Live Flags Size Unit Dst` (`i_bs_get_fixed_binary`).
            "binary" => {
                let size_op = cmd_at(commands, i + 3)?;
                let unit = read_size(machine, ctx, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let nbits = if is_all(module, size_op) {
                    end.saturating_sub(pos)
                } else {
                    let size = read_size(machine, ctx, size_op)?;
                    match size.checked_mul(unit) {
                        Some(nbits) => nbits,
                        None => return fail_step(machine, module, labels, fail, term, ctx),
                    }
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                let out = extract_bits(bytes, pos, nbits)?;
                let value = raw::make_bits(ctx, &out, nbits)?;
                write_operand(machine, dst, value)?;
                pos += nbits;
                i += 6;
            }
            // `get_tail Live Unit Dst` — bind the whole remaining bits
            // (`i_bs_get_tail`); the position is left unchanged (it is the last
            // command, `bs_translate_fixup`).
            "get_tail" => {
                let dst = cmd_at(commands, i + 3)?;
                let nbits = end.saturating_sub(pos);
                let out = extract_bits(bytes, pos, nbits)?;
                let value = raw::make_bits(ctx, &out, nbits)?;
                write_operand(machine, dst, value)?;
                i += 4;
            }
            // `'=:=' Nil Bits Value` (`i_bs_eq`, `bs_instrs.tab:1366`): the next
            // `Bits` bits must equal the literal unsigned `Value` exactly (no sign
            // extension). `Value` is emitted as a plain integer up to 64 bits.
            //
            // The atom *name* is `=:=` (the quotes `beam_disasm` prints around
            // `'=:='` are just the non-alphanumeric-atom notation and are not part
            // of the name).
            "=:=" => {
                let bits = read_size(machine, ctx, cmd_at(commands, i + 2)?)?;
                let want_term = read_operand(machine, ctx, cmd_at(commands, i + 3)?)?;
                let Some(want) = small_value(want_term) else {
                    return Err(error::unsupported("bs_match '=:=': non-integer value"));
                };
                if bits > 64 || bits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                if read_uint_be(bytes, pos, bits)? != want as u64 {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                pos += bits;
                i += 4;
            }
            other => {
                return Err(error::unsupported(&format!("bs_match: command {other}")));
            }
        }
    }
    // Commit the advanced position only once the whole command list succeeded.
    raw::set_ctx_start(term, pos);
    Ok(Some(Step::Next))
}
/// The `i`-th command operand, or a typed error when the list is truncated.
pub(crate) fn cmd_at(commands: &[Operand], index: usize) -> Result<&Operand> {
    commands
        .get(index)
        .ok_or_else(|| error::unsupported("bs_match: truncated command list"))
}
/// The `ensure_at_least` test (`generators.tab:1164-1206`):
/// * `Size != 0`, `Unit == 1` ⇒ `i_bs_ensure_bits` (`remaining >= Size`);
/// * `Size != 0`, `Unit != 1` ⇒ `i_bs_ensure_bits_unit` (`remaining >= Size` **and**
///   the surplus is a whole number of units);
/// * `Size == 0`, `Unit == 8` ⇒ `i_bs_test_unit8` (byte alignment only);
/// * `Size == 0`, `Unit != 1` ⇒ `i_bs_test_unit` (alignment to `Unit`);
/// * `Size == 0`, `Unit == 1` ⇒ a redundant no-op.
pub(crate) fn ensure_at_least(remaining: usize, size: usize, unit: usize) -> bool {
    if size == 0 {
        if unit == 8 {
            remaining % 8 == 0
        } else if unit == 1 {
            true
        } else {
            remaining % unit == 0
        }
    } else if unit == 1 {
        remaining >= size
    } else {
        remaining >= size && (remaining - size) % unit == 0
    }
}
