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

use std::collections::BTreeMap;

use beam_common::Result;
use beam_loader::{Instr, Label, Module, Operand};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::operand::{read_operand, write_operand};
use crate::Step;

use super::{
    decode_flags, extract_bits, fail_step, is_all, make_small_checked, raw, read_size,
    read_uint_be, read_uint_le, sign_extend, small_value, BSF_ALIGNED, BSF_LITTLE, BSF_SIGNED,
};

/// Execute a bitstring **matching** instruction.
pub(super) fn exec(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
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
            let subject = read_operand(machine, bin)?;
            match enter_match(ctx, subject)? {
                Some(context) => {
                    write_operand(machine, dst, context)?;
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, subject, ctx),
            }
        }

        Instr::BsStartMatch4 {
            fail,
            live: _,
            src,
            dst,
        } => {
            let (fail_label, resume) = resolve_match4_fail(module, fail)?;
            let subject = read_operand(machine, src)?;
            if resume {
                // `bs_start_match4 resume Ctx Dst` reduces to `move Ctx Dst`
                // (`emu/ops.tab:1251-1252`): the source is already a match context,
                // so rebinding must keep the *same* mutable context, not a copy.
                if raw::ctx_parts(subject).is_none() {
                    return fail_step(machine, module, labels, fail_label, subject, ctx);
                }
                write_operand(machine, dst, subject)?;
                return Ok(Some(Step::Next));
            }
            match enter_match(ctx, subject)? {
                Some(context) => {
                    write_operand(machine, dst, context)?;
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, fail_label, subject, ctx),
            }
        }

        Instr::BsGetPosition {
            ctx: ctx_op,
            dst,
            live: _,
        } => {
            let (_, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_position")?;
            let snapshot = raw::make_ctx(ctx, orig, start, end)?;
            write_operand(machine, dst, snapshot)?;
            Ok(Some(Step::Next))
        }

        Instr::BsSetPosition { ctx: ctx_op, pos } => {
            let (term, _, _, _) = read_ctx(machine, ctx_op, "bs_set_position")?;
            let value = read_operand(machine, pos)?;
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
            let (_, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_tail")?;
            let nbits = end.saturating_sub(start);
            let bytes = raw::subject_bytes(orig)?;
            let out = extract_bits(bytes, start, nbits)?;
            let value = raw::make_bits(ctx, &out, nbits)?;
            write_operand(machine, dst, value)?;
            Ok(Some(Step::Next))
        }

        Instr::BsTestTail2 {
            fail,
            ctx: ctx_op,
            bits,
        } => {
            let (term, _, start, end) = read_ctx(machine, ctx_op, "bs_test_tail2")?;
            if end.saturating_sub(start) == *bits as usize {
                Ok(Some(Step::Next))
            } else {
                fail_step(machine, module, labels, *fail, term, ctx)
            }
        }

        Instr::BsGetInteger2 {
            fail,
            ctx: ctx_op,
            live: _,
            size,
            unit,
            flags,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_integer2")?;
            let size = read_size(machine, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, term, ctx);
            };
            if nbits > end.saturating_sub(start) {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            if flags & BSF_ALIGNED != 0 && start % 8 != 0 {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            if nbits > 64 {
                return Err(error::unsupported(
                    "bs_get_integer2: integer field wider than 64 bits",
                ));
            }
            let bytes = raw::subject_bytes(orig)?;
            let value = read_integer_field(bytes, start, nbits, *flags)?;
            write_operand(machine, dst, value)?;
            raw::set_ctx_start(term, start + nbits);
            Ok(Some(Step::Next))
        }

        Instr::BsGetBinary2 {
            fail,
            ctx: ctx_op,
            live: _,
            size,
            unit,
            flags: _,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_binary2")?;
            let remaining = end.saturating_sub(start);
            let nbits = if is_all(module, size) {
                remaining
            } else {
                let size = read_size(machine, size)?;
                match size.checked_mul(*unit as usize) {
                    Some(nbits) => nbits,
                    None => return fail_step(machine, module, labels, *fail, term, ctx),
                }
            };
            if nbits > remaining {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            let bytes = raw::subject_bytes(orig)?;
            let out = extract_bits(bytes, start, nbits)?;
            let value = raw::make_bits(ctx, &out, nbits)?;
            write_operand(machine, dst, value)?;
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
            let (term, _, start, end) = read_ctx(machine, ctx_op, "bs_skip_bits2")?;
            let size = read_size(machine, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, term, ctx);
            };
            let Some(next) = start.checked_add(nbits) else {
                return fail_step(machine, module, labels, *fail, term, ctx);
            };
            if next > end {
                return fail_step(machine, module, labels, *fail, term, ctx);
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
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_match_string")?;
            let size = *bits as usize;
            if end.saturating_sub(start) < size {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            let pattern = module
                .strings
                .get(*offset as usize..)
                .unwrap_or(&[]);
            let bytes = raw::subject_bytes(orig)?;
            if size > pattern.len() * 8 || !bits_equal(bytes, start, pattern, 0, size) {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            raw::set_ctx_start(term, start + size);
            Ok(Some(Step::Next))
        }

        // `bs_get_float2 Fail Ctx Live Sz Flags Dst` — read a `Sz * Unit`-bit float
        // (`i_bs_get_float2`, `bs_instrs.tab:227` into `erts_bs_get_float_2`). Only
        // 0/16/32/64 bits are valid; a short subject or any other size is a match
        // failure.
        Instr::BsGetFloat2 {
            fail,
            ctx: ctx_op,
            live: _,
            size,
            unit,
            flags,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_float2")?;
            let size = read_size(machine, size)?;
            let Some(nbits) = size.checked_mul(*unit as usize) else {
                return fail_step(machine, module, labels, *fail, term, ctx);
            };
            if !matches!(nbits, 0 | 16 | 32 | 64) || nbits > end.saturating_sub(start) {
                return fail_step(machine, module, labels, *fail, term, ctx);
            }
            let bytes = raw::subject_bytes(orig)?;
            let value = read_float_field(bytes, start, nbits, *flags)?;
            let float = crate::terms::make_float(ctx, value)?;
            write_operand(machine, dst, float)?;
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
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_utf8")?;
            let bytes = raw::subject_bytes(orig)?;
            match decode_utf8(bytes, start, end) {
                Some((value, len)) => {
                    let t = make_small_checked(value as i64)?;
                    write_operand(machine, dst, t)?;
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, term, ctx),
            }
        }

        Instr::BsGetUtf16 {
            fail,
            ctx: ctx_op,
            live: _,
            flags,
            dst,
        } => {
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_utf16")?;
            let bytes = raw::subject_bytes(orig)?;
            match decode_utf16(bytes, start, end, *flags) {
                Some((value, len)) => {
                    let t = make_small_checked(value as i64)?;
                    write_operand(machine, dst, t)?;
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, term, ctx),
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
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_get_utf32")?;
            let bytes = raw::subject_bytes(orig)?;
            match decode_utf32(bytes, start, end) {
                Some((value, len)) => {
                    let t = make_small_checked(value as i64)?;
                    write_operand(machine, dst, t)?;
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, term, ctx),
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
            let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_skip_utf16")?;
            let bytes = raw::subject_bytes(orig)?;
            match decode_utf16(bytes, start, end, flags) {
                Some((_, len)) => {
                    raw::set_ctx_start(term, start + len);
                    Ok(Some(Step::Next))
                }
                None => fail_step(machine, module, labels, *fail, term, ctx),
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

/// Run a UTF skip: advance the position by the decoded length, or fail.
fn skip_utf(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    ctx: &mut dyn ExecutionContext,
    ctx_op: &Operand,
    fail: Label,
    decode: fn(&[u8], usize, usize) -> Option<(u32, usize)>,
) -> Result<Option<Step>> {
    let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_skip_utf")?;
    let bytes = raw::subject_bytes(orig)?;
    match decode(bytes, start, end) {
        Some((_, len)) => {
            raw::set_ctx_start(term, start + len);
            Ok(Some(Step::Next))
        }
        None => fail_step(machine, module, labels, fail, term, ctx),
    }
}

// ---------------------------------------------------------------------------
// Match entry
// ---------------------------------------------------------------------------

/// Start (or resume) a match over `subject`, yielding the match context.
///
/// A heap binary becomes a fresh context at position 0 (`i_bs_start_match3`,
/// `bs_instrs.tab:1199`); a term that is already an `ErlSubBits` is re-wrapped with
/// its current position (the C `reuse` path). Anything else is a *match* failure
/// (`None`), not an error.
fn enter_match(ctx: &mut dyn ExecutionContext, subject: Eterm) -> Result<Option<Eterm>> {
    if let Some((_, bits)) = raw::heap_bits(subject) {
        return Ok(Some(raw::make_ctx(ctx, subject, 0, bits)?));
    }
    if let Some((orig, start, end)) = raw::ctx_parts(subject) {
        return Ok(Some(raw::make_ctx(ctx, orig, start, end)?));
    }
    Ok(None)
}

/// Decode `bs_start_match4`'s `fail` operand: a label, or the atoms
/// `no_fail`/`resume` (`emu/ops.tab:1243-1262`).
fn resolve_match4_fail(module: &Module, fail: &Operand) -> Result<(Label, bool)> {
    match fail {
        Operand::Label(label) => Ok((*label, false)),
        Operand::Atom(atom) => match module.atom_name(*atom) {
            Some("no_fail") => Ok((beam_loader::NO_FAIL, false)),
            Some("resume") => Ok((beam_loader::NO_FAIL, true)),
            Some(other) => Err(error::unsupported(&format!(
                "bs_start_match4: fail atom {other}"
            ))),
            None => Err(error::unsupported("bs_start_match4: unknown fail atom")),
        },
        _ => Err(error::unsupported(
            "bs_start_match4: unsupported fail operand",
        )),
    }
}

/// Read a match-context operand, returning `(term, orig, start, end)`.
fn read_ctx(
    machine: &Machine,
    operand: &Operand,
    what: &str,
) -> Result<(Eterm, Eterm, usize, usize)> {
    let term = read_operand(machine, operand)?;
    let (orig, start, end) = raw::ctx_parts(term)
        .ok_or_else(|| error::unsupported(&format!("{what}: operand is not a match context")))?;
    Ok((term, orig, start, end))
}

/// The bit position a `bs_set_position` operand denotes.
///
/// The engine's `bs_get_position` snapshots the whole context, so the ordinary
/// input is another `ErlSubBits`; the C 64-bit lowering instead stores the raw
/// position as a small integer (`bs_instrs.tab:1125-1129`), so that form is
/// accepted too.
fn position_of(value: Eterm) -> Result<usize> {
    if let Some((_, start, _)) = raw::ctx_parts(value) {
        return Ok(start);
    }
    if let Some(n) = small_value(value) {
        if n >= 0 {
            return Ok(n as usize);
        }
        return Err(error::unsupported("bs_set_position: negative bit position"));
    }
    Err(error::unsupported(
        "bs_set_position: position is neither a context nor a small integer",
    ))
}

/// Extract an integer field of `nbits` bits at bit `start`, honouring the
/// `BSF_LITTLE`/`BSF_SIGNED` flags, and build the resulting term.
fn read_integer_field(bytes: &[u8], start: usize, nbits: usize, flags: u32) -> Result<Eterm> {
    let raw_value = if flags & BSF_LITTLE != 0 {
        read_uint_le(bytes, start, nbits)?
    } else {
        read_uint_be(bytes, start, nbits)?
    };
    let value = if flags & BSF_SIGNED != 0 {
        sign_extend(raw_value, nbits)
    } else {
        raw_value
    };
    make_small_checked(value as i64)
}

// ---------------------------------------------------------------------------
// Bit-level helpers (a *bit* offset, not necessarily byte-aligned)
// ---------------------------------------------------------------------------

/// The bit at absolute index `index` (bit 0 is the MSB of byte 0).
fn bit_at(bytes: &[u8], index: usize) -> u8 {
    (bytes[index / 8] >> (7 - (index % 8))) & 1
}

/// Compare `nbits` bits of `a` starting at bit `astart` with `nbits` bits of `b`
/// starting at bit `bstart`, MSB-first.
///
/// The caller guarantees both ranges are in bounds (a violated precondition would
/// panic, so every call site range-checks first).
fn bits_equal(a: &[u8], astart: usize, b: &[u8], bstart: usize, nbits: usize) -> bool {
    (0..nbits).all(|i| bit_at(a, astart + i) == bit_at(b, bstart + i))
}

/// Read `nbits` bits (a whole number of bytes) starting at bit `start` as a
/// **little-endian** integer: the field's least-significant byte comes first in the
/// stream, each byte taken MSB-first.
fn read_uint_le_bits(bytes: &[u8], start: usize, nbits: usize) -> Result<u64> {
    if nbits % 8 != 0 {
        return Err(error::unsupported(
            "bitstring: little-endian field is not a whole number of bytes",
        ));
    }
    if start.checked_add(nbits).is_none_or(|e| e > bytes.len() * 8) {
        return Err(error::unsupported(
            "bitstring: little-endian field out of range",
        ));
    }
    let mut value = 0u64;
    for k in 0..nbits / 8 {
        let mut byte = 0u8;
        for j in 0..8 {
            byte = (byte << 1) | bit_at(bytes, start + 8 * k + j);
        }
        value |= u64::from(byte) << (8 * k);
    }
    Ok(value)
}

// ---------------------------------------------------------------------------
// Float reads (`erts_bs_get_float_2`, `erl_bits.c:550`)
// ---------------------------------------------------------------------------

/// Read a `nbits`-bit float field at bit `start` and return its `f64` value.
///
/// `nbits` is 0/16/32/64 (validated by the caller). The field's byte order follows
/// `BSF_LITTLE` (`big` is the default); a 16-bit field is a binary16 converted
/// through `f16_to_f32` exactly as C does.
fn read_float_field(bytes: &[u8], start: usize, nbits: usize, flags: u32) -> Result<f64> {
    if nbits == 0 {
        return Ok(0.0);
    }
    let little = flags & BSF_LITTLE != 0;
    let raw = if little {
        read_uint_le_bits(bytes, start, nbits)?
    } else {
        read_uint_be(bytes, start, nbits)?
    };
    Ok(match nbits {
        16 => f64::from(f16_to_f32(raw as u16)),
        32 => f64::from(f32::from_bits(raw as u32)),
        64 => f64::from_bits(raw),
        _ => return Err(error::unsupported("bs_get_float2: unsupported float size")),
    })
}

/// `f16_to_f32` (`erl_bits.c:508`): IEEE-754 binary16 → binary32, exactly as C
/// (subnormals are normalised; Inf/NaN are widened).
fn f16_to_f32(fp: u16) -> f32 {
    let sign = (fp >> 15) & 1;
    let exp = (fp >> 10) & 0x1F;
    let mantissa = fp & 0x3FF;
    let mut res = u32::from(sign) << 31;
    if exp == 0 {
        if mantissa != 0 {
            let mut m = u32::from(mantissa);
            let mut e: i32 = 0;
            while m & 0x400 == 0 {
                m <<= 1;
                e -= 1;
            }
            m &= 0x3FF;
            let exp32 = (127 - 15 + 1 + e) as u32;
            res |= exp32 << 23;
            res |= m << 13;
        }
    } else if exp == 0x1F {
        res |= 0xFF << 23;
        if mantissa != 0 {
            res |= u32::from(mantissa) << 13;
        }
    } else {
        let exp32 = u32::from(exp) + (127 - 15);
        res |= exp32 << 23;
        res |= u32::from(mantissa) << 13;
    }
    f32::from_bits(res)
}

// ---------------------------------------------------------------------------
// UTF reads (`erts_bs_get_utf8`/`_utf16`, `erl_bits.c:2129`/`:2224`)
// ---------------------------------------------------------------------------

/// `erts_trailing_bytes_for_utf8` (`erl_bits.c`): the number of *continuation* bytes
/// a UTF-8 lead byte announces; `9` marks a byte that can never lead a sequence.
fn trailing_bytes_for_utf8(b: u8) -> u8 {
    match b {
        0x00..=0x7F => 0,
        0x80..=0xBF => 9,
        0xC0..=0xDF => 1,
        0xE0..=0xEF => 2,
        0xF0..=0xF4 => 3,
        _ => 9,
    }
}

/// Decode one UTF-8 scalar from bit `start` (byte-aligned), bounded by bit `end`.
///
/// Returns `(code_point, bits_consumed)` or `None` for a malformed/short sequence —
/// exactly the cases `erts_bs_get_utf8` rejects: reserved lead bytes, a bad
/// continuation byte, an overlong encoding, a surrogate, or a value > U+10FFFF.
/// `bs_get_utf8 -little` is not meaningful in ERTS (the flag is ignored), so the
/// byte order is always big-endian here.
fn decode_utf8(bytes: &[u8], start: usize, end: usize) -> Option<(u32, usize)> {
    if end < start + 8 {
        return None;
    }
    let a = read_uint_be(bytes, start, 8).ok()? as u8;
    let trailing = trailing_bytes_for_utf8(a);
    if trailing > 3 {
        return None;
    }
    let trailing = trailing as usize;
    let total_bits = (trailing + 1) * 8;
    if end - start < total_bits {
        return None;
    }
    let mask = if trailing == 0 {
        0x7Fu32
    } else {
        (1u32 << (6 - trailing)) - 1
    };
    let mut result = (u32::from(a) & mask) << (6 * trailing);
    for k in 1..=trailing {
        let b = read_uint_be(bytes, start + 8 * k, 8).ok()? as u8;
        if b & 0xC0 != 0x80 {
            return None;
        }
        result |= u32::from(b & 0x3F) << (6 * (trailing - k));
    }
    // Overlong encodings (the minimum value each length may encode).
    let min = match trailing {
        1 => 0x80,
        2 => 0x800,
        3 => 0x10000,
        _ => 0,
    };
    if result < min || (0xD800..=0xDFFF).contains(&result) || result > 0x10FFFF {
        return None;
    }
    Some((result, total_bits))
}

/// Read a 16-bit field at bit `start` in the field's byte order.
fn read_uint16(bytes: &[u8], start: usize, little: bool) -> Result<u16> {
    if little {
        Ok(read_uint_le_bits(bytes, start, 16)? as u16)
    } else {
        Ok(read_uint_be(bytes, start, 16)? as u16)
    }
}

/// Decode one UTF-16 scalar from bit `start`, bounded by bit `end`
/// (`erts_bs_get_utf16`). `BSF_LITTLE` selects little-endian 16-bit words.
fn decode_utf16(bytes: &[u8], start: usize, end: usize, flags: u32) -> Option<(u32, usize)> {
    let little = flags & BSF_LITTLE != 0;
    if end < start + 16 {
        return None;
    }
    let w1 = u32::from(read_uint16(bytes, start, little).ok()?);
    if !(0xD800..=0xDFFF).contains(&w1) {
        return Some((w1, 16));
    }
    // `w1` is a surrogate: it must be a high surrogate followed by a low surrogate.
    if w1 > 0xDBFF || end - start < 32 {
        return None;
    }
    let w2 = u32::from(read_uint16(bytes, start + 16, little).ok()?);
    if !(0xDC00..=0xDFFF).contains(&w2) {
        return None;
    }
    Some((((w1 & 0x3FF) << 10 | (w2 & 0x3FF)) + 0x10000, 32))
}

/// Decode one UTF-32 scalar from bit `start` (`i_bs_validate_unicode_retract`): a
/// 32-bit big-endian read that must be a valid Unicode scalar
/// (≤ U+10FFFF, not a surrogate).
fn decode_utf32(bytes: &[u8], start: usize, end: usize) -> Option<(u32, usize)> {
    if end < start + 32 {
        return None;
    }
    let value = read_uint_be(bytes, start, 32).ok()? as u32;
    if value > 0x10FFFF || (0xD800..=0xDFFF).contains(&value) {
        return None;
    }
    Some((value, 32))
}

// ---------------------------------------------------------------------------
// The `bs_match` command interpreter
// ---------------------------------------------------------------------------

/// Execute a `bs_match` command list against the context in `ctx_op`.
fn run_commands(
    machine: &mut Machine,
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    fail: Label,
    ctx_op: &Operand,
    commands: &[Operand],
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<Step>> {
    let (term, orig, start, end) = read_ctx(machine, ctx_op, "bs_match")?;
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
                let size = read_size(machine, cmd_at(commands, i + 1)?)?;
                let unit = read_size(machine, cmd_at(commands, i + 2)?)?;
                if !ensure_at_least(end.saturating_sub(pos), size, unit) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                i += 3;
            }

            // `ensure_exactly Stride` — `bs_test_tail_imm2` (`generators.tab:1210`).
            "ensure_exactly" => {
                let stride = read_size(machine, cmd_at(commands, i + 1)?)?;
                if end.saturating_sub(pos) != stride {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                i += 2;
            }

            // `skip Stride` — advance without binding (`i_bs_skip`).
            "skip" => {
                let stride = read_size(machine, cmd_at(commands, i + 1)?)?;
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
                let size = read_size(machine, cmd_at(commands, i + 3)?)?;
                let unit = read_size(machine, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let Some(nbits) = size.checked_mul(unit) else {
                    return fail_step(machine, module, labels, fail, term, ctx);
                };
                if nbits > end.saturating_sub(pos) {
                    return fail_step(machine, module, labels, fail, term, ctx);
                }
                if nbits > 64 {
                    return Err(error::unsupported(
                        "bs_match integer: field wider than 64 bits",
                    ));
                }
                let value = read_integer_field(bytes, pos, nbits, flags)?;
                write_operand(machine, dst, value)?;
                pos += nbits;
                i += 6;
            }

            // `binary Live Flags Size Unit Dst` (`i_bs_get_fixed_binary`).
            "binary" => {
                let size_op = cmd_at(commands, i + 3)?;
                let unit = read_size(machine, cmd_at(commands, i + 4)?)?;
                let dst = cmd_at(commands, i + 5)?;
                let nbits = if is_all(module, size_op) {
                    end.saturating_sub(pos)
                } else {
                    let size = read_size(machine, size_op)?;
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
            "'=:='" => {
                let bits = read_size(machine, cmd_at(commands, i + 2)?)?;
                let want_term = read_operand(machine, cmd_at(commands, i + 3)?)?;
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
fn cmd_at(commands: &[Operand], index: usize) -> Result<&Operand> {
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
fn ensure_at_least(remaining: usize, size: usize, unit: usize) -> bool {
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
