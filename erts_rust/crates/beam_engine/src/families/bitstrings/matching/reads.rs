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

//! Fixed bit/float/UTF reads for the bitstring match opcodes.
//!
//! The match-side integer field read (`erts_bs_get_integer_2`), the float read
//! (`erts_bs_get_float_2`), the UTF-8/16/32 decoders and their skip variants, plus
//! the pure bit-level arithmetic they are built on. Everything here is plain safe
//! `&[u8]` arithmetic; the only frozen-header access is through [`raw`].
use crate::interpreter::LabelMap;

use beam_common::Result;
use beam_loader::{Label, Module, Operand};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;
use crate::Step;

use super::super::{
    extract_bits, fail_step, le_bytes_to_digits, make_int, raw, read_uint_be, read_uint_le,
    sign_extend, twos_complement_bits, BSF_LITTLE, BSF_SIGNED,
};
use super::match_ctx::read_ctx;
/// Run a UTF skip: advance the position by the decoded length, or fail.
pub(crate) fn skip_utf(
    machine: &mut Machine,
    module: &Module,
    labels: &LabelMap,
    ctx: &mut dyn ExecutionContext,
    ctx_op: &Operand,
    fail: Label,
    decode: fn(&[u8], usize, usize) -> Option<(u32, usize)>,
) -> Result<Option<Step>> {
    let (term, orig, start, end) = read_ctx(machine, ctx, ctx_op, "bs_skip_utf")?;
    let bytes = raw::subject_bytes(orig)?;
    match decode(bytes, start, end) {
        Some((_, len)) => {
            raw::set_ctx_start(term, start + len);
            Ok(Some(Step::Next))
        }
        None => fail_step(machine, module, labels, fail, ctx_op, ctx),
    }
}
/// Extract an integer field of `nbits` bits at bit `start`, honouring the
/// `BSF_LITTLE`/`BSF_SIGNED` flags, and build the resulting term.
///
/// `erts_bs_get_integer_2` (`erl_bits.c:170`) has fast paths for fields that are
/// *guaranteed* to fit a small integer (`<= 8` bits, and `< SMALL_BITS` bits
/// big-endian) and one general path for everything else. Only the fast paths are
/// special-cased here; a field of at most 64 bits that is **not** guaranteed to
/// fit is collected into a machine word and handed to the same `IS_USMALL`
/// decision the general path uses ([`make_int`], which delegates the bignum
/// itself to `beam_term`). A field **wider** than 64 bits is the general path's
/// byte-array form ([`read_wide_integer_field`]).
///
/// Because `MAX_SMALL` is only `2^59-1`, the fast paths a *big-endian* field
/// needs are `nbits <= 8` and `9 <= nbits < 59` — a 59-bit unsigned field's
/// maximum (`2^59-1`) still fits. A 60..64-bit field, and every little-endian
/// field of 9 or more bits (C has no small fast path for those), can therefore
/// exceed `MAX_SMALL` and must build a bignum.
pub(crate) fn read_integer_field(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    bytes: &[u8],
    start: usize,
    nbits: usize,
    flags: u32,
) -> Result<Eterm> {
    let little = flags & BSF_LITTLE != 0;
    // A byte-aligned little-endian field is the only one `read_uint_le` handles;
    // every other little-endian field takes the general bit-level path (C's
    // `erts_copy_bits_fwd` + `*MSB >>= 8 - BIT_OFFSET(num_bits)`,
    // `erl_bits.c:305-313`), including the **non-byte-multiple** widths the
    // standard compiler emits (`<<Y:12/little>>`, `<<Y:20/little>>`, …).
    let le_fast_path = nbits % 8 == 0 && start % 8 == 0;
    if nbits > 64 || (little && !le_fast_path) {
        return read_wide_integer_field(machine, ctx, bytes, start, nbits, flags);
    }
    let raw_value = if little {
        read_uint_le(bytes, start, nbits)?
    } else {
        read_uint_be(bytes, start, nbits)?
    };
    // The sign and magnitude exactly as `erts_bs_get_integer_2` derives them: a
    // signed field is sign-extended at its own width first (`MAYBE_SIGN_EXTEND`),
    // so its machine word read as `i64` **is** the value; an unsigned field's word
    // is a pure magnitude (never reinterpreted as negative, even above `2^63`).
    let (negative, magnitude) = if flags & BSF_SIGNED != 0 {
        let signed = sign_extend(raw_value, nbits) as i64;
        (signed < 0, signed.unsigned_abs())
    } else {
        (false, raw_value)
    };
    make_int(machine, ctx, negative, &[magnitude])
}
/// Read an integer field **wider than 64 bits** and build the (possibly bignum)
/// term, reproducing `erts_bs_get_integer_2` beyond its small-integer special
/// cases (`erl_bits.c:245-398`).
///
/// The field's `nbits` bits are first laid out MSB-first in `field` (exactly what
/// C's `erts_copy_bits_rev`/`erts_copy_bits_fwd` produce); `field` is then turned
/// into the value's little-endian byte array:
/// * **big-endian** (the default): the natural big-endian integer, i.e. value bit
///   `k` is field bit `nbits - 1 - k`;
/// * **little-endian**: the first 8-bit group is the least-significant byte, with
///   the final partial group (`nbits % 8` bits) read MSB-first — C's
///   `*MSB >>= 8 - BIT_OFFSET(num_bits)` adjustment.
///
/// A signed field whose top bit is set is two's-complemented into its magnitude
/// (`MAYBE_SIGN_EXTEND`); the leading-zero normalisation is
/// [`le_bytes_to_digits`], and `IS_USMALL` is [`make_int`].
pub(crate) fn read_wide_integer_field(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    bytes: &[u8],
    start: usize,
    nbits: usize,
    flags: u32,
) -> Result<Eterm> {
    let num_bytes = nbits.div_ceil(8);
    // The field's bits, MSB-first, left-aligned in `num_bytes` bytes.
    let field = extract_bits(bytes, start, nbits)?;
    let mut value = vec![0u8; num_bytes];
    if flags & BSF_LITTLE != 0 {
        let rem = nbits % 8;
        for (j, out) in value.iter_mut().enumerate() {
            *out = if j + 1 == num_bytes && rem != 0 {
                field[j] >> (8 - rem)
            } else {
                field[j]
            };
        }
    } else {
        for k in 0..nbits {
            let i = nbits - 1 - k;
            let bit = (field[i / 8] >> (7 - (i % 8))) & 1;
            value[k / 8] |= bit << (k % 8);
        }
    }
    let negative =
        flags & BSF_SIGNED != 0 && (value[(nbits - 1) / 8] >> ((nbits - 1) % 8)) & 1 == 1;
    if negative {
        twos_complement_bits(&mut value, nbits);
    }
    let digits = le_bytes_to_digits(&value);
    make_int(machine, ctx, negative, &digits)
}
// ---------------------------------------------------------------------------
// Bit-level helpers (a *bit* offset, not necessarily byte-aligned)
// ---------------------------------------------------------------------------
/// The bit at absolute index `index` (bit 0 is the MSB of byte 0).
pub(crate) fn bit_at(bytes: &[u8], index: usize) -> u8 {
    (bytes[index / 8] >> (7 - (index % 8))) & 1
}
/// Compare `nbits` bits of `a` starting at bit `astart` with `nbits` bits of `b`
/// starting at bit `bstart`, MSB-first.
///
/// The caller guarantees both ranges are in bounds (a violated precondition would
/// panic, so every call site range-checks first).
pub(crate) fn bits_equal(a: &[u8], astart: usize, b: &[u8], bstart: usize, nbits: usize) -> bool {
    (0..nbits).all(|i| bit_at(a, astart + i) == bit_at(b, bstart + i))
}
/// Read `nbits` bits (a whole number of bytes) starting at bit `start` as a
/// **little-endian** integer: the field's least-significant byte comes first in the
/// stream, each byte taken MSB-first.
pub(crate) fn read_uint_le_bits(bytes: &[u8], start: usize, nbits: usize) -> Result<u64> {
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
pub(crate) fn read_float_field(
    bytes: &[u8],
    start: usize,
    nbits: usize,
    flags: u32,
) -> Result<f64> {
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
pub(crate) fn f16_to_f32(fp: u16) -> f32 {
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
pub(crate) fn trailing_bytes_for_utf8(b: u8) -> u8 {
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
pub(crate) fn decode_utf8(bytes: &[u8], start: usize, end: usize) -> Option<(u32, usize)> {
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
pub(crate) fn read_uint16(bytes: &[u8], start: usize, little: bool) -> Result<u16> {
    if little {
        Ok(read_uint_le_bits(bytes, start, 16)? as u16)
    } else {
        Ok(read_uint_be(bytes, start, 16)? as u16)
    }
}
/// Decode one UTF-16 scalar from bit `start`, bounded by bit `end`
/// (`erts_bs_get_utf16`). `BSF_LITTLE` selects little-endian 16-bit words.
pub(crate) fn decode_utf16(
    bytes: &[u8],
    start: usize,
    end: usize,
    flags: u32,
) -> Option<(u32, usize)> {
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
pub(crate) fn decode_utf32(bytes: &[u8], start: usize, end: usize) -> Option<(u32, usize)> {
    if end < start + 32 {
        return None;
    }
    let value = read_uint_be(bytes, start, 32).ok()? as u32;
    if value > 0x10FFFF || (0xD800..=0xDFFF).contains(&value) {
        return None;
    }
    Some((value, 32))
}
