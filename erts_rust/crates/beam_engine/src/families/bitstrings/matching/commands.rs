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
//! decoding the records is this module's job. A local `pos` cursor walks the subject
//! bits, and every command that advances it mirrors the advance into the context's own
//! position field **before** binding its destination ([`commit`]) — the C order
//! (`i_bs_read_bits`/`i_bs_get_fixed_integer`/`i_bs_get_fixed_binary`/`i_bs_skip` all do
//! `sb->start += …` before `$Dst = result`), which has to hold even when the destination
//! *is* the register the context lives in (`get_tail` binds the tail into the context's
//! own register). `ensure_at_least`/`ensure_exactly` only test; the reads bind a value
//! into a register.
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
use super::match_ctx::{read_ctx, reread_ctx};
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
    // `pos` is a *local* cursor over the subject bits. Every command that advances it
    // mirrors the advance into the context's own position field straight away
    // ([`commit`]) — never in one batch after the loop: a command binds its destination
    // *after* the C has advanced `sb->start`, and that destination may be the very
    // operand the context lives in (`get_tail` in `bits/1`), so a trailing write-back
    // would have to re-read a register a command had already overwritten.
    let (_, _, start, _) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
    let mut pos = start;
    let mut i = 0usize;
    while i < commands.len() {
        // Re-derive the context and its byte view at the top of **every** iteration:
        // the previous iteration's `integer`/`binary`/`get_tail` command allocated
        // through the rooted seam (`read_integer_field` -> `raw::make_big`,
        // `raw::make_bits`), whose moving collector may have relocated the context
        // block, so a `term`/`bytes` captured before that allocation is stale
        // (the frozen reserve, then read, then write ordering).
        let (_, orig, _, end) = read_ctx(machine, ctx, ctx_op, "bs_match")?;
        let (bytes, off) = raw::subject_bytes(orig)?;
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
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                i += 3;
            }
            // `ensure_exactly Stride` — `bs_test_tail_imm2` (`generators.tab:1210`).
            "ensure_exactly" => {
                let stride = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
                if end.saturating_sub(pos) != stride {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                i += 2;
            }
            // `skip Stride` — advance without binding (`i_bs_skip`, which does
            // `sb->start += size`).
            "skip" => {
                let stride = read_size(machine, ctx, cmd_at(commands, i + 1)?)?;
                let Some(next) = pos.checked_add(stride) else {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                };
                if next > end {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                pos = next;
                commit(machine, ctx, ctx_op, pos)?;
                i += 2;
            }
            // `integer Live Flags Size Unit Dst` (`i_bs_get_fixed_integer`).
            "integer" => {
                let flags = decode_flags(module, cmd_at(commands, i + 2)?)?;
                let size = read_size(machine, ctx, cmd_at(commands, i + 3)?)?;
                let unit = read_size(machine, ctx, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let Some(nbits) = size.checked_mul(unit) else {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                let value = read_integer_field(machine, ctx, bytes, pos + off, nbits, flags)?;
                pos += nbits;
                // The C advances `sb->start` (inside `erts_bs_get_integer_2`) *before*
                // it binds `$Dst`, so the advance is mirrored first: `Dst` may be the
                // context's own operand.
                commit(machine, ctx, ctx_op, pos)?;
                write_operand(machine, dst, value)?;
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
                        None => return fail_step(machine, module, labels, fail, ctx_op, ctx),
                    }
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                let out = extract_bits(bytes, pos + off, nbits)?;
                let value = raw::make_bits(machine, ctx, &out, nbits)?;
                pos += nbits;
                // `i_bs_get_fixed_binary` does `sb->start += size;` *before*
                // `$Dst = result;` — mirror the advance first (`Dst` may be the
                // context's own operand).
                commit(machine, ctx, ctx_op, pos)?;
                write_operand(machine, dst, value)?;
                i += 6;
            }
            // `get_tail Live Unit Dst` — bind the whole remaining bits
            // (`i_bs_get_tail`); the position is left unchanged (it is the last
            // command, `bs_translate_fixup`).
            "get_tail" => {
                let dst = cmd_at(commands, i + 3)?;
                let nbits = end.saturating_sub(pos);
                let out = extract_bits(bytes, pos + off, nbits)?;
                let value = raw::make_bits(machine, ctx, &out, nbits)?;
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
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                if read_uint_be(bytes, pos + off, bits)? != want as u64 {
                    return fail_step(machine, module, labels, fail, ctx_op, ctx);
                }
                pos += bits;
                // The C consumes `Bits` through the `i_bs_read_bits` that feeds
                // `i_bs_eq`, which advances `sb->start` by `Bits`.
                commit(machine, ctx, ctx_op, pos)?;
                i += 4;
            }
            other => {
                return Err(error::unsupported(&format!("bs_match: command {other}")));
            }
        }
    }
    Ok(Some(Step::Next))
}

/// Mirror the local cursor `pos` into the match context's own position field.
///
/// The context operand is re-read **here**, right before the write: the command that
/// advanced the cursor may have allocated through the rooted seam
/// ([`read_integer_field`] -> [`raw::make_big`], [`raw::make_bits`]), whose moving
/// collector may have relocated the context block, so the term captured at the top of
/// the iteration is stale (the frozen reserve, then read, then write ordering). The
/// operand itself is part of the machine's root set and is rewritten in place by the
/// collector, so re-evaluating it yields the live term.
///
/// Every caller commits **before** binding a command's destination: the C advances
/// `sb->start` first (`i_bs_get_fixed_integer`/`i_bs_get_fixed_binary`/`i_bs_skip`/
/// `i_bs_read_bits`) and `bs_match`'s `get_tail` binds the tail into the context's own
/// register, so a destination binding can make the context operand unreadable.
fn commit(
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
    ctx_op: &Operand,
    pos: usize,
) -> Result<()> {
    let term = reread_ctx(machine, ctx, ctx_op, "bs_match")?;
    raw::set_ctx_start(term, pos);
    Ok(())
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
