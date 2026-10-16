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

//! Bitstring **matching**: match entry, the fixed reads, the position ops and the
//! flattened `bs_match` command interpreter.
//!
//! Ground truth (read-only): `emu/bs_instrs.tab` (`i_bs_start_match3` `:1199`,
//! `i_bs_get_tail` `:1481`, `bs_get_position`/`bs_set_position` `:1085-1153`,
//! `i_bs_get_fixed_integer` `:1420`, `i_bs_get_fixed_binary` `:1447`, `i_bs_skip`
//! `:1512`, `i_bs_match_string` `:1004`, `i_bs_get_float2` `:227`, `i_bs_get_utf8`
//! `:948`, `i_bs_get_utf16` `:983`, `i_bs_eq` `:1366`, `bs_test_tail_imm2` `:753`),
//! the `bs_match` command lowering `emu/generators.tab:1102-1409` with the flag table
//! `erl_bits.h:461-465`, and the decoders `erl_bits.c:erts_bs_get_float_2` `:550`,
//! `erts_bs_get_utf8` `:2129`, `erts_bs_get_utf16` `:2224`, `f16_to_f32` `:508`.
//!
//! # Half-open restore pattern
//!
//! The compiler brackets a match sequence with `bs_get_position` **before** and a
//! failure handler that runs `bs_set_position` then `bs_get_tail` then raises
//! (`first8/1` in the fixture). A read opcode therefore must **not** advance the
//! context when it *fails* — it only commits the new position on success — so the
//! handler's `bs_set_position` always sees the pre-match position.
//!
//! # Command model
//!
//! `bs_match`'s `commands` operand is the raw flattened list the loader preserved
//! (`instr.rs:730`); decoding the records is this module's job. A `pos` cursor
//! advances locally through the commands and is written back into the context once
//! the whole list has succeeded. `ensure_at_least`/`ensure_exactly` only test; the
//! reads snapshot a value into a register.
//!
//! # Module layout
//!
//! The work is split by responsibility: this file holds [`exec`] (the dispatch);
//! [`match_ctx`] holds match entry and match-context handling; [`commands`] holds the
//! `bs_match` command interpreter; [`reads`] holds the fixed bit/float/UTF decoders
//! and the match-side integer/float/UTF readers.
use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{BsGetArgs, Instr, Module};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand, read_operand_slack, write_operand};
use crate::Step;

use super::{extract_bits, fail_step, is_all, make_small_checked, raw, read_size};

mod commands;
mod match_ctx;
mod reads;
#[cfg(test)]
mod rooted;
#[cfg(test)]
mod utf_tests;
#[cfg(test)]
mod tests;

// Every moved item is re-exported here so its existing
// `crate::families::bitstrings::matching::…` path keeps resolving. Some are only
// reached from inside their own submodule (which is why this explicit re-export is
// not itself "used"), hence the `allow`.
#[allow(unused_imports)]
pub(crate) use commands::{cmd_at, ensure_at_least, run_commands};
#[allow(unused_imports)]
pub(crate) use match_ctx::{enter_match, position_of, read_ctx, reread_ctx, resolve_match4_fail};
#[allow(unused_imports)]
pub(crate) use reads::{
    bit_at, bits_equal, decode_utf16, decode_utf32, decode_utf8, f16_to_f32, read_float_field,
    read_integer_field, read_uint16, read_uint_le_bits, read_wide_integer_field, skip_utf,
    trailing_bytes_for_utf8,
};
/// Execute a bitstring **matching** instruction.
pub(super) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    instr: &Instr,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let _ = (func, pc);
    match instr {
        Instr::BsStartMatch3 {
            fail,
            bin,
            live: _,
            dst,
        } => {
            match enter_match(machine, ctx, bin)? {
                Some(context) => {
                    write_operand(machine, dst, context)?;
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, bin, ctx),
            }
        }
        Instr::BsStartMatch4 {
            fail,
            live: _,
            src,
            dst,
        } => {
            let (fail_label, resume) = resolve_match4_fail(module, fail)?;
            let subject = read_operand(machine, ctx, src)?;
            if resume {
                // `bs_start_match4 resume Ctx Dst` reduces to `move Ctx Dst`
                // (`emu/ops.tab:1251-1252`): the source is already a match context,
                // so rebinding must keep the *same* mutable context, not a copy.
                if raw::ctx_parts(subject).is_none() {
                    return fail_step(machine, module, labels, fail_label, src, ctx);
                }
                write_operand(machine, dst, subject)?;
                return Ok(Some(Step::Next));
            }
            match enter_match(machine, ctx, src)? {
                Some(context) => {
                    write_operand(machine, dst, context)?;
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, fail_label, src, ctx),
            }
        }
        Instr::BsGetPosition {
            ctx: ctx_op,
            dst,
            live: _,
        } => {
            let (_, _, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_position")?;
            // Reserve the snapshot first, then re-read the context operand so the
            // snapshot's `orig` is the post-collection inner subject of the *current*
            // context.
            let extra = operand_slack_words(ctx_op);
            let snapshot = raw::make_ctx(machine, ctx, start, end, extra, |m, slack| {
                let value = read_operand_slack(m, ctx_op, slack)?;
                raw::inner_orig(value).ok_or_else(|| {
                    crate::error::unsupported("bs_get_position: operand is not a match context")
                })
            })?;
            write_operand(machine, dst, snapshot)?;
            Ok(Some(Step::Next))
        }
        Instr::BsSetPosition { ctx: ctx_op, pos } => {
            let (term, _, _, _) = read_ctx(machine, ctx, ctx_op, "bs_set_position")?;
            let value = read_operand(machine, ctx, pos)?;
            let new_start = position_of(value)?;
            raw::set_ctx_start(term, new_start);
            Ok(Some(Step::Next))
        }
        Instr::BsGetTail {
            ctx: ctx_op,
            dst,
            live: _,
        } => {
            // `i_bs_get_tail` builds the remaining bits from `start` to `end` and
            // deliberately leaves the position unchanged. The C build is
            // `erts_build_sub_bitstring` over an arbitrary bit count (no 64-byte
            // limit), so the result goes through `raw::make_binary`, which picks the
            // frozen representation (on-heap `ErlHeapBits` at/below
            // `ERL_ONHEAP_BINARY_LIMIT`, off-heap refc binary above).
            let (_, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_tail")?;
            let nbits = end.saturating_sub(start);
            let (bytes, off) = raw::subject_bytes(orig)?;
            let out = extract_bits(bytes, start + off, nbits)?;
            let value = raw::make_binary(machine, ctx, &out, nbits)?;
            write_operand(machine, dst, value)?;
            Ok(Some(Step::Next))
        }
        Instr::BsTestTail2 {
            fail,
            ctx: ctx_op,
            bits,
        } => {
            let (_, _, start, end) = read_ctx(machine, ctx, ctx_op, "bs_test_tail2")?;
            if end.saturating_sub(start) == *bits as usize {
                Ok(Some(Step::Next))
            } else {
                fail_step(machine, module, labels, *fail, ctx_op, ctx)
            }
        }
        Instr::BsGetInteger2(b) => {
            let BsGetArgs {
                fail,
                ctx: ctx_op,
                live: _,
                size,
                unit,
                flags,
                dst,
            } = &**b;
            let (_, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_integer2")?;
            let size = read_size(machine, ctx, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            };
            if nbits > end.saturating_sub(start) {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            }
            let (bytes, off) = raw::subject_bytes(orig)?;
            let value = read_integer_field(machine, ctx, bytes, start + off, nbits, *flags)?;
            // `read_integer_field` allocates (a bignum) through the rooted seam, so a
            // moving collector may have relocated the context read above: re-read the
            // context operand and commit the new position **before** binding `Dst`.
            // The C advances `sb->start` inside `erts_bs_get_integer_2` *before* the
            // instruction's `$Dst = _result`, and `Dst` may be the context's own
            // operand (`beam_asm:filter_essentials/1` compiles to `ctx == dst == x(3)`),
            // so binding first would clobber the context the commit re-reads (the frozen
            // reserve, then read, then write ordering).
            let term = reread_ctx(machine, ctx, ctx_op, "bs_get_integer2")?;
            raw::set_ctx_start(term, start + nbits);
            write_operand(machine, dst, value)?;
            Ok(Some(Step::Next))
        }
        Instr::BsGetBinary2(b) => {
            let BsGetArgs {
                fail,
                ctx: ctx_op,
                live: _,
                size,
                unit,
                flags: _,
                dst,
            } = &**b;
            let (_, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_binary2")?;
            let remaining = end.saturating_sub(start);
            let nbits = if is_all(module, size) {
                remaining
            } else {
                let size = read_size(machine, ctx, size)?;
                match size.checked_mul(*unit as usize) {
                    Some(nbits) => nbits,
                    None => return fail_step(machine, module, labels, *fail, ctx_op, ctx),
                }
            };
            if nbits > remaining {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            }
            let (bytes, off) = raw::subject_bytes(orig)?;
            let out = extract_bits(bytes, start + off, nbits)?;
            // `i_bs_get_binary_2` builds the window through
            // `erts_build_sub_bitstring` for an arbitrary bit count (no 64-byte
            // limit), so the result goes through `raw::make_binary`, which picks the
            // frozen on-heap/off-heap representation by size.
            let value = raw::make_binary(machine, ctx, &out, nbits)?;
            // `raw::make_binary` reserves through the rooted seam, so a moving collector
            // may have relocated the context read above: re-read the context operand and
            // commit the new position **before** binding `Dst`. The C advances `sb->start`
            // inside `erts_bs_get_binary_2` *before* the instruction's `$Dst = _result`,
            // and `Dst` may be the context's own operand (`beam_asm:filter_essentials/1`
            // compiles to `ctx == dst == x(3)`), so binding first would clobber the context
            // the commit re-reads (the frozen reserve, then read, then write ordering).
            let term = reread_ctx(machine, ctx, ctx_op, "bs_get_binary2")?;
            raw::set_ctx_start(term, start + nbits);
            write_operand(machine, dst, value)?;
            Ok(Some(Step::Next))
        }
        Instr::BsSkipBits2 {
            fail,
            ctx: ctx_op,
            size,
            unit,
            flags: _,
        } => {
            let (term, _, start, end) = read_ctx(machine, ctx, ctx_op, "bs_skip_bits2")?;
            let size = read_size(machine, ctx, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            };
            let Some(next) = start.checked_add(nbits) else {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            };
            if next > end {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            }
            raw::set_ctx_start(term, next);
            Ok(Some(Step::Next))
        }
        Instr::BsMatch {
            fail,
            ctx: ctx_op,
            commands,
        } => run_commands(machine, module, labels, *fail, ctx_op, commands, ctx),
        // `bs_match_string Fail Ctx Bits Offset` — compare the next `Bits` bits of
        // the context against the `StrT` string at byte `Offset`
        // (`i_bs_match_string`, `bs_instrs.tab:1004`). The comparison succeeds only
        // when the whole string matches; a short subject or a mismatch is an ordinary
        // match failure, and the position advances **only on success**.
        Instr::BsMatchString {
            fail,
            ctx: ctx_op,
            bits,
            offset,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_match_string")?;
            let size = *bits as usize;
            if end.saturating_sub(start) < size {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            }
            // The `StrT` offset must name a byte **inside** the module's string
            // table. `get(offset..)` is `Some(&[])` exactly at `offset == len`
            // (a legitimately empty pattern), so a single `ok_or_else` tells the
            // two cases apart. C has no such check (it reads an already-resolved
            // absolute pointer into its `StrT` copy, `emu_load.c:469-485`), so this
            // is a deliberate stricter-than-C typed error rather than a silent
            // degradation to an empty pattern that would merely fail the match.
            let pattern = module
                .strings
                .get(*offset as usize..)
                .ok_or_else(|| {
                    crate::error::unsupported("bs_match_string: string table offset out of range")
                })?;
            let (bytes, off) = raw::subject_bytes(orig)?;
            if size > pattern.len() * 8 || !bits_equal(bytes, start + off, pattern, 0, size) {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            }
            raw::set_ctx_start(term, start + size);
            Ok(Some(Step::Next))
        }
        // `bs_get_float2 Fail Ctx Live Sz Flags Dst` — read a `Sz * Unit`-bit float
        // (`i_bs_get_float2`, `bs_instrs.tab:227` into `erts_bs_get_float_2`). Only
        // 0/16/32/64 bits are valid; a short subject or any other size is a match
        // failure.
        Instr::BsGetFloat2(b) => {
            let BsGetArgs {
                fail,
                ctx: ctx_op,
                live: _,
                size,
                unit,
                flags,
                dst,
            } = &**b;
            let (_, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_float2")?;
            let size = read_size(machine, ctx, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            };
            if !matches!(nbits, 0 | 16 | 32 | 64) || nbits > end.saturating_sub(start) {
                return fail_step(machine, module, labels, *fail, ctx_op, ctx);
            }
            let (bytes, off) = raw::subject_bytes(orig)?;
            let value = read_float_field(bytes, start + off, nbits, *flags)?;
            let float = crate::terms::make_float_rooted(machine, ctx, value)?;
            // `make_float` reserves through the rooted seam, so a moving collector may
            // have relocated the context read above: re-read the context operand and
            // commit the new position **before** binding `Dst`. The C advances `sb->start`
            // inside `erts_bs_get_float_2` *before* the instruction's `$Dst = _result`,
            // and `Dst` may be the context's own operand (the frozen reserve, then read,
            // then write ordering).
            let term = reread_ctx(machine, ctx, ctx_op, "bs_get_float2")?;
            raw::set_ctx_start(term, start + nbits);
            write_operand(machine, dst, float)?;
            Ok(Some(Step::Next))
        }
        Instr::BsGetUtf8 {
            fail,
            ctx: ctx_op,
            live: _,
            flags: _,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_utf8")?;
            let (bytes, off) = raw::subject_bytes(orig)?;
            match decode_utf8(bytes, start + off, end + off) {
                Some((value, len)) => {
                    let t = make_small_checked(value as i64)?;
                    write_operand(machine, dst, t)?;
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, ctx_op, ctx),
            }
        }
        Instr::BsGetUtf16 {
            fail,
            ctx: ctx_op,
            live: _,
            flags,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_utf16")?;
            let (bytes, off) = raw::subject_bytes(orig)?;
            match decode_utf16(bytes, start + off, end + off, *flags) {
                Some((value, len)) => {
                    let t = make_small_checked(value as i64)?;
                    write_operand(machine, dst, t)?;
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, ctx_op, ctx),
            }
        }
        // `bs_get_utf32` is rewritten by `emu/ops.tab` into
        // `bs_get_integer2 … 32 1 Flags` + `i_bs_validate_unicode_retract`, so the
        // 32-bit field honours the segment flags (`little`/`native` select the byte
        // order, `big` is the default) and the result must be a valid Unicode scalar.
        Instr::BsGetUtf32 {
            fail,
            ctx: ctx_op,
            live: _,
            flags,
            dst,
        } => {
            let flags = *flags;
            let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_utf32")?;
            let (bytes, off) = raw::subject_bytes(orig)?;
            match decode_utf32(bytes, start + off, end + off, flags) {
                Some((value, len)) => {
                    let t = make_small_checked(value as i64)?;
                    write_operand(machine, dst, t)?;
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, ctx_op, ctx),
            }
        }
        Instr::BsSkipUtf8 {
            fail,
            ctx: ctx_op,
            live: _,
            flags: _,
        } => skip_utf(machine, module, labels, ctx, ctx_op, *fail, decode_utf8),
        Instr::BsSkipUtf16 {
            fail,
            ctx: ctx_op,
            live: _,
            flags,
        } => {
            let flags = *flags;
            let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_skip_utf16")?;
            let (bytes, off) = raw::subject_bytes(orig)?;
            match decode_utf16(bytes, start + off, end + off, flags) {
                Some((_, len)) => {
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, ctx_op, ctx),
            }
        }
        // `bs_skip_utf32` is `bs_get_integer2 … 32 1 Flags` +
        // `i_bs_validate_unicode_retract` with the value discarded
        // (`emu/ops.tab:1305-1307`), so it honours the flags exactly as `bs_get_utf32`
        // does and advances the context by the same 32 bits.
        Instr::BsSkipUtf32 {
            fail,
            ctx: ctx_op,
            live: _,
            flags,
        } => {
            let flags = *flags;
            let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_skip_utf32")?;
            let (bytes, off) = raw::subject_bytes(orig)?;
            match decode_utf32(bytes, start + off, end + off, flags) {
                Some((_, len)) => {
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, ctx_op, ctx),
            }
        }
        _ => Ok(None),
    }
}
