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
            // deliberately leaves the position unchanged.
            let (_, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_tail")?;
            let nbits = end.saturating_sub(start);
            let (bytes, off) = raw::subject_bytes(orig)?;
            let out = extract_bits(bytes, start + off, nbits)?;
            let value = raw::make_bits(machine, ctx, &out, nbits)?;
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
            write_operand(machine, dst, value)?;
            // `read_integer_field` allocates (a bignum) through the rooted seam, so a
            // moving collector may have relocated the context read above: re-read the
            // context operand before committing the new position (the frozen
            // reserve, then read, then write ordering).
            let term = reread_ctx(machine, ctx, ctx_op, "bs_get_integer2")?;
            raw::set_ctx_start(term, start + nbits);
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
            let value = raw::make_bits(machine, ctx, &out, nbits)?;
            write_operand(machine, dst, value)?;
            // `raw::make_bits` reserves through the rooted seam, so a moving collector
            // may have relocated the context read above: re-read the context operand
            // before committing the new position (reserve, then read, then write).
            let term = reread_ctx(machine, ctx, ctx_op, "bs_get_binary2")?;
            raw::set_ctx_start(term, start + nbits);
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
            let pattern = module.strings.get(*offset as usize..).unwrap_or(&[]);
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
            write_operand(machine, dst, float)?;
            // `make_float` reserves through the rooted seam, so a moving collector may
            // have relocated the context read above: re-read the context operand
            // before committing the new position (reserve, then read, then write).
            let term = reread_ctx(machine, ctx, ctx_op, "bs_get_float2")?;
            raw::set_ctx_start(term, start + nbits);
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
        // `bs_get_integer2 … 32 1` + `i_bs_validate_unicode_retract`; the observable
        // result is a 32-bit big-endian read that must be a valid Unicode scalar.
        Instr::BsGetUtf32 {
            fail,
            ctx: ctx_op,
            live: _,
            flags: _,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_get_utf32")?;
            let (bytes, off) = raw::subject_bytes(orig)?;
            match decode_utf32(bytes, start + off, end + off) {
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
        Instr::BsSkipUtf32 {
            fail,
            ctx: ctx_op,
            live: _,
            flags: _,
        } => skip_utf(machine, module, labels, ctx, ctx_op, *fail, decode_utf32),
        _ => Ok(None),
    }
}
#[cfg(test)]
mod tests {
    //! Unit tests for the pure decoders the `bs_*` match opcodes call — their expected
    //! values are the system `erl` oracle's answers for the same bit patterns — plus
    //! [`empty_binary_bs_match_commands_and_the_writable_accumulator`], the
    //! instruction-level regression for the **empty-binary match** path itself.
    use super::super::BSF_LITTLE;
    use super::*;

    // The instruction-level regression drives whole instructions through the
    // **family** dispatch ([`family_exec`], which routes to this module's [`exec`]
    // and to `create::exec`), so it needs the machine host and the raw bitstring
    // writers beside the decoders above.
    use super::super::exec as family_exec;
    use crate::context::MinimalContext;
    use crate::families::bitstrings::raw;
    use crate::interpreter::LabelMap;
    use crate::machine::Machine;
    use crate::terms::binary_bytes;
    use crate::Step;
    use beam_loader::{Instr, Label, Module, Operand, NO_FAIL};

    // -- decode_utf8 (erts_bs_get_utf8) -------------------------------------
    #[test]
    fn utf8_decodes_each_sequence_length() {
        assert_eq!(decode_utf8(&[0x41], 0, 8), Some((0x41, 8)));
        assert_eq!(decode_utf8(&[0xC2, 0xA9], 0, 16), Some((0xA9, 16)));
        assert_eq!(decode_utf8(&[0xE2, 0x82, 0xAC], 0, 24), Some((0x20AC, 24)));
        assert_eq!(
            decode_utf8(&[0xF0, 0x9F, 0x98, 0x80], 0, 32),
            Some((0x1F600, 32))
        );
    }
    #[test]
    fn utf8_accepts_the_range_boundaries() {
        // U+007F (1 byte), U+0080/U+07FF (2), U+0800/U+FFFF (3), U+10000/U+10FFFF (4).
        assert_eq!(decode_utf8(&[0x7F], 0, 8), Some((0x7F, 8)));
        assert_eq!(decode_utf8(&[0xC2, 0x80], 0, 16), Some((0x80, 16)));
        assert_eq!(decode_utf8(&[0xDF, 0xBF], 0, 16), Some((0x7FF, 16)));
        assert_eq!(decode_utf8(&[0xE0, 0xA0, 0x80], 0, 24), Some((0x800, 24)));
        assert_eq!(decode_utf8(&[0xEF, 0xBF, 0xBF], 0, 24), Some((0xFFFF, 24)));
        assert_eq!(
            decode_utf8(&[0xF0, 0x90, 0x80, 0x80], 0, 32),
            Some((0x10000, 32))
        );
        assert_eq!(
            decode_utf8(&[0xF4, 0x8F, 0xBF, 0xBF], 0, 32),
            Some((0x10FFFF, 32))
        );
    }
    #[test]
    fn utf8_reads_at_a_non_byte_aligned_start() {
        // The decoder works at a *bit* offset (`read_uint_be`), so start need not be
        // byte-aligned: skip one byte of a 2-byte buffer.
        assert_eq!(decode_utf8(&[0x00, 0x41], 8, 16), Some((0x41, 8)));
    }
    #[test]
    fn utf8_rejects_every_malformed_class() {
        assert_eq!(decode_utf8(&[], 0, 0), None); // bounded out
        assert_eq!(decode_utf8(&[0xC2], 0, 8), None); // truncated 2-byte
        assert_eq!(decode_utf8(&[0xE2, 0x82], 0, 16), None); // truncated 3-byte
        assert_eq!(decode_utf8(&[0x80], 0, 8), None); // continuation lead
        assert_eq!(decode_utf8(&[0xFF], 0, 8), None); // reserved lead
        assert_eq!(decode_utf8(&[0xE2, 0x28, 0xAC], 0, 24), None); // bad continuation
        assert_eq!(decode_utf8(&[0xC0, 0x80], 0, 16), None); // overlong U+0000
        assert_eq!(decode_utf8(&[0xC1, 0xBF], 0, 16), None); // overlong U+007F
        assert_eq!(decode_utf8(&[0xE0, 0x80, 0x80], 0, 24), None); // overlong U+0000
        assert_eq!(decode_utf8(&[0xF0, 0x80, 0x80, 0x80], 0, 32), None); // overlong
        assert_eq!(decode_utf8(&[0xED, 0xA0, 0x80], 0, 24), None); // surrogate D800
        assert_eq!(decode_utf8(&[0xED, 0xBF, 0xBF], 0, 24), None); // surrogate DFFF
        assert_eq!(decode_utf8(&[0xF4, 0x90, 0x80, 0x80], 0, 32), None); // >U+10FFFF
    }
    // -- decode_utf16 (erts_bs_get_utf16) -----------------------------------
    #[test]
    fn utf16_decodes_bmp_and_surrogate_pairs() {
        assert_eq!(decode_utf16(&[0x00, 0x41], 0, 16, 0), Some((0x41, 16)));
        assert_eq!(decode_utf16(&[0x20, 0xAC], 0, 16, 0), Some((0x20AC, 16)));
        // D7FF/E000 sit just outside the surrogate range and are plain scalars.
        assert_eq!(decode_utf16(&[0xD7, 0xFF], 0, 16, 0), Some((0xD7FF, 16)));
        assert_eq!(decode_utf16(&[0xE0, 0x00], 0, 16, 0), Some((0xE000, 16)));
        // D83D DE00 is U+1F600.
        assert_eq!(
            decode_utf16(&[0xD8, 0x3D, 0xDE, 0x00], 0, 32, 0),
            Some((0x1F600, 32))
        );
        // Little-endian words select `BSF_LITTLE`.
        assert_eq!(
            decode_utf16(&[0xAC, 0x20], 0, 16, BSF_LITTLE),
            Some((0x20AC, 16))
        );
        assert_eq!(
            decode_utf16(&[0x3D, 0xD8, 0x00, 0xDE], 0, 32, BSF_LITTLE),
            Some((0x1F600, 32))
        );
    }
    #[test]
    fn utf16_rejects_lone_and_malformed_surrogates() {
        assert_eq!(decode_utf16(&[0xD8], 0, 8, 0), None); // bounded out
        assert_eq!(decode_utf16(&[0xD8, 0x3D], 0, 16, 0), None); // lone high, no low
        assert_eq!(decode_utf16(&[0xDE, 0x00], 0, 16, 0), None); // lone low surrogate
        assert_eq!(decode_utf16(&[0xD8, 0x3D, 0x00, 0x41], 0, 32, 0), None); // bad low
    }
    // -- decode_utf32 (i_bs_validate_unicode_retract) -----------------------
    #[test]
    fn utf32_decodes_valid_scalars_and_rejects_the_rest() {
        assert_eq!(
            decode_utf32(&[0x00, 0x00, 0x20, 0xAC], 0, 32),
            Some((0x20AC, 32))
        );
        assert_eq!(
            decode_utf32(&[0x00, 0x10, 0xFF, 0xFF], 0, 32),
            Some((0x10FFFF, 32))
        );
        assert_eq!(decode_utf32(&[0x00, 0x00, 0x00], 0, 24), None); // bounded out
        assert_eq!(decode_utf32(&[0x00, 0x00, 0xD8, 0x00], 0, 32), None); // surrogate
        assert_eq!(decode_utf32(&[0x00, 0x11, 0x00, 0x00], 0, 32), None); // >U+10FFFF
    }
    // -- f16_to_f32 (erl_bits.c:508) ----------------------------------------
    #[test]
    fn f16_to_f32_widens_every_class() {
        assert_eq!(f16_to_f32(0x0000).to_bits(), 0x0000_0000); // +0
        assert_eq!(f16_to_f32(0x8000).to_bits(), 0x8000_0000); // -0
        assert_eq!(f16_to_f32(0x3C00).to_bits(), 0x3F80_0000); // 1.0
        assert_eq!(f16_to_f32(0x7C00).to_bits(), 0x7F80_0000); // +Inf
        assert_eq!(f16_to_f32(0xFC00).to_bits(), 0xFF80_0000); // -Inf
        assert_eq!(f16_to_f32(0x7E00).to_bits(), 0x7FC0_0000); // NaN
        assert_eq!(f16_to_f32(0x0001).to_bits(), 0x3380_0000); // min subnormal
        assert_eq!(f16_to_f32(0x03FF).to_bits(), 0x387F_C000); // max subnormal
        assert_eq!(f16_to_f32(0x0400).to_bits(), 0x3880_0000); // min normal (2^-14)
        assert_eq!(f16_to_f32(0x7BFF).to_bits(), 0x477F_E000); // 65504.0 (max)
    }
    // -- read_float_field (erts_bs_get_float_2) -----------------------------
    #[test]
    fn read_float_field_handles_every_valid_size_and_endianness() {
        assert_eq!(read_float_field(&[], 0, 0, 0).unwrap(), 0.0);
        assert_eq!(read_float_field(&[62, 0], 0, 16, 0).unwrap(), 1.5);
        assert_eq!(read_float_field(&[0, 62], 0, 16, BSF_LITTLE).unwrap(), 1.5);
        assert_eq!(read_float_field(&[63, 192, 0, 0], 0, 32, 0).unwrap(), 1.5);
        assert_eq!(
            read_float_field(&[0, 0, 192, 63], 0, 32, BSF_LITTLE).unwrap(),
            1.5
        );
        let pi = [64, 9, 33, 251, 84, 68, 45, 24];
        assert_eq!(
            read_float_field(&pi, 0, 64, 0).unwrap(),
            std::f64::consts::PI
        );
        let mut little = pi;
        little.reverse();
        assert_eq!(
            read_float_field(&little, 0, 64, BSF_LITTLE).unwrap(),
            std::f64::consts::PI
        );
    }
    #[test]
    fn read_float_field_rejects_an_unsupported_size_and_an_unaligned_le_field() {
        // Only 0/16/32/64 are valid; anything else is the typed `unsupported`.
        assert!(read_float_field(&[0, 0, 0, 0], 0, 8, 0).is_err());
        // A little-endian field is read byte-wise, so it must be byte-aligned.
        assert!(read_float_field(&[0, 0, 0, 0], 4, 32, BSF_LITTLE).is_err());
    }
    // -- ensure_at_least (the `bs_match` command) ---------------------------
    #[test]
    fn ensure_at_least_covers_every_documented_case() {
        assert!(ensure_at_least(8, 8, 1)); // >= Size
        assert!(!ensure_at_least(7, 8, 1));
        assert!(ensure_at_least(16, 8, 8)); // whole units after Size
        assert!(!ensure_at_least(15, 8, 8));
        assert!(ensure_at_least(16, 0, 8)); // byte alignment only
        assert!(!ensure_at_least(15, 0, 8));
        assert!(ensure_at_least(15, 0, 1)); // redundant no-op
        assert!(ensure_at_least(15, 0, 5)); // alignment to Unit
        assert!(!ensure_at_least(14, 0, 5));
    }

    // -- the empty-binary match (the `base64:encode(<<>>)` shape) -----------
    /// The **empty-binary match**: the two `bs_match` command lists the installed
    /// `erlc` emits for `base64:encode_binary/4`'s first and empty clauses, driven as
    /// whole instructions over a 0-bit subject, plus the 0-bit `bs_init_writable`
    /// accumulator the empty clause returns.
    ///
    /// `base64:encode/2` opens its accumulator with `move {integer,256} -> x(0)` +
    /// `bs_init_writable` and tail-calls `encode_binary/4` with that accumulator in
    /// `x(3)`; `beam_disasm:file/1` shows `encode_binary/4` (base64.erl:218) as
    ///
    /// ```text
    /// {label,19},
    /// {bs_start_match4,{atom,no_fail},4,{x,2},{x,2}},
    /// {bs_get_position,{x,2},{x,4},4},
    /// {bs_match,{f,20},{x,2},
    ///     {commands,[{ensure_at_least,48,1},
    ///                {integer,4,{literal,[]},6,1,{x,4}},
    ///                … {integer,11,{literal,[]},6,1,{x,11}}]}},
    /// …
    /// {label,20},                                        % <fail>: the empty clause
    /// {bs_set_position,{x,2},{x,4}},
    /// {bs_match,{f,21},{x,2},{commands,[{ensure_exactly,0}]}},
    /// {move,{x,3},{x,0}},
    /// return
    /// ```
    ///
    /// The loop body appends with a `bs_create_bin` whose first segment is
    /// `{atom,private_append},1,1,nil,{tr,{x,3},{t_bs_matchable,1}},{atom,all}` (the
    /// accumulator in `x(3)` again).
    ///
    /// On `<<>>` the first `bs_match` must take the fail label (that is the first
    /// clause refusing an empty subject) and the second must fall through — which is
    /// what makes the accumulator `bs_init_writable` built the value returned. The
    /// oracle prints that accumulator as `<<>>`:
    ///
    /// ```text
    /// $ erl -noshell -eval 'io:format("~p~n",[base64:encode(<<>>)]),halt().'
    /// <<>>
    /// ```
    ///
    /// The end-to-end counterpart (the real installed `base64:encode/1` over the whole
    /// sweep host) lives in `tests/stdlib_sweep.rs`.
    #[test]
    fn empty_binary_bs_match_commands_and_the_writable_accumulator() {
        let mut machine = Machine::with_max_register(4);
        let mut ctx = MinimalContext::new();

        // The command/segment atoms the family reads from the **running module** (a
        // real module's atom table carries them; a synthetic one must intern them).
        let mut module = Module::default();
        for name in [
            "no_fail",
            "ensure_exactly",
            "ensure_at_least",
            "private_append",
            "all",
        ] {
            module.atom_table.intern(name);
        }
        let atom = |module: &Module, name: &str| {
            Operand::Atom(
                module
                    .atom_table
                    .lookup(name)
                    .unwrap_or_else(|| panic!("atom {name:?} interned above")),
            )
        };

        // The empty subject `<<>>` `encode_binary/4` is called on: a 0-bit on-heap
        // binary, exactly what `terms::make_heap_binary(ctx, &[])` builds.
        let subject = raw::make_bits(&mut machine, &mut ctx, &[], 0).expect("a 0-bit binary");
        machine.set_x(0, subject).expect("root the subject");

        // `bs_start_match4 {atom,no_fail},4,{x,0},{x,1}` — the match entry. Nothing can
        // fail here (`no_fail`), and `<<>>` still enters a (0..0) match context.
        let step = family_exec(
            &mut machine,
            &module,
            &LabelMap::new(),
            &Instr::BsStartMatch4 {
                fail: atom(&module, "no_fail"),
                live: 4,
                src: Operand::X(0),
                dst: Operand::X(1),
            },
            0,
            0,
            &mut ctx,
        )
        .expect("bs_start_match4 reports no raise")
        .expect("the family claims bs_start_match4");
        assert_eq!(step, Step::Next, "`<<>>` enters a match with `no_fail`");

        // The live fail label both `bs_match`es branch to (the empty clause's entry).
        const FAIL: Label = Label(1);
        let mut labels = LabelMap::new();
        labels.insert(FAIL, (0, 7));

        // (1) The empty clause: `{commands,[{ensure_exactly,0}]}` MATCHES `<<>>` — the
        // engine reports `Next` (the instruction after `bs_match`), never the fail
        // label. (Both clauses share the context at `x(1)`; `ensure_exactly` only
        // tests, so it does not advance it.)
        let step = family_exec(
            &mut machine,
            &module,
            &labels,
            &Instr::BsMatch {
                fail: FAIL,
                ctx: Operand::X(1),
                commands: vec![atom(&module, "ensure_exactly"), Operand::Integer(0)],
            },
            0,
            0,
            &mut ctx,
        )
        .expect("bs_match reports no raise")
        .expect("the family claims bs_match");
        assert_eq!(step, Step::Next, "`ensure_exactly 0` matches `<<>>`");

        // (2) The first clause: `{commands,[{ensure_at_least,48,1}]}` FAILS on `<<>>`
        // (there are 0 of the 48 required bits), so the engine branches to the live
        // fail label instead of falling through.
        let step = family_exec(
            &mut machine,
            &module,
            &labels,
            &Instr::BsMatch {
                fail: FAIL,
                ctx: Operand::X(1),
                commands: vec![
                    atom(&module, "ensure_at_least"),
                    Operand::Integer(48),
                    Operand::Integer(1),
                ],
            },
            0,
            0,
            &mut ctx,
        )
        .expect("bs_match reports no raise")
        .expect("the family claims bs_match");
        assert_eq!(
            step,
            Step::Goto { func: 0, pc: 7 },
            "`ensure_at_least 48` refuses `<<>>` and takes the fail label"
        );

        // (3) The accumulator the empty clause returns: `move {integer,256} -> x(0)`
        // followed by `bs_init_writable` (the C `erts_bs_init_writable` keeps the
        // capacity hint in `x(0)` and reports a **zero-length** window).
        machine
            .set_x(0, beam_term::make_small(256))
            .expect("the capacity hint");
        let step = family_exec(
            &mut machine,
            &module,
            &labels,
            &Instr::BsInitWritable,
            0,
            0,
            &mut ctx,
        )
        .expect("bs_init_writable reports no raise")
        .expect("the family claims bs_init_writable");
        assert_eq!(step, Step::Next);
        let accumulator = machine.x(0).expect("the accumulator");
        assert_eq!(
            raw::heap_bits(accumulator).map(|(_, bits, _)| bits),
            Some(0),
            "`bs_init_writable` returns a 0-bit bitstring"
        );
        assert_eq!(
            binary_bytes(accumulator).expect("a byte binary"),
            &[] as &[u8],
            "the accumulator the empty clause returns reads as `<<>>`"
        );

        // `call_last 4 {base64,encode_binary,4}` hands that accumulator to the next
        // clause in `x(3)` (the C argument-register convention), which is where the
        // loop body's `bs_create_bin` keeps it.
        machine.set_x(3, accumulator).expect("the accumulator in x(3)");

        // The loop body's `{atom,private_append},1,1,nil,{x,3},{atom,all}` segment
        // seeded from that accumulator appends nothing, so the built result is still
        // `<<>>` and the frozen readers accept it. (`alloc`/`live`/`unit` are the
        // operands `base64.beam` emits; the private_append arm reads neither.)
        let step = family_exec(
            &mut machine,
            &module,
            &labels,
            &Instr::BsCreateBin {
                fail: NO_FAIL,
                alloc: 0,
                live: 12,
                unit: 32,
                dst: Operand::X(3),
                segments: vec![
                    atom(&module, "private_append"),
                    Operand::Integer(1), // Index (a compiler-side identifier)
                    Operand::Integer(1), // Unit
                    Operand::Nil,        // Flags (no little/signed/native)
                    Operand::X(3),       // the `bs_init_writable` accumulator
                    atom(&module, "all"), // Size
                ],
            },
            0,
            0,
            &mut ctx,
        )
        .expect("bs_create_bin reports no raise")
        .expect("the family claims bs_create_bin");
        assert_eq!(step, Step::Next);
        let built = machine.x(3).expect("the built binary");
        assert_eq!(
            raw::heap_bits(built).map(|(_, bits, _)| bits),
            Some(0),
            "a `private_append` of nothing onto `<<>>` is `<<>>`"
        );
        assert_eq!(
            binary_bytes(built).expect("a byte binary"),
            &[] as &[u8],
            "the built result reads as `<<>>`"
        );
    }
}
