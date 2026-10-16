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

//! `bs_create_bin` **per-segment-type sources and encoders**.
//!
//! The `binary`/`string` source readers ([`segment_source`], [`string_source`]) and
//! the `float`/`utf8`/`utf16`/`utf32` value readers and byte encoders
//! ([`float_source`], [`put_float`], [`f64_to_f16`], [`put_utf8`], [`put_utf16`],
//! [`put_utf32`]) that the segment loop in [`super`] dispatches to. The integer
//! segment's wide-field writer is [`super::super::push_wide_int`].

use beam_common::Result;
use beam_loader::{Module, Operand};
use beam_term::Eterm;

use crate::context::ExecutionContext;
use crate::error;
use crate::machine::Machine;

use super::read_value;
use super::super::{is_all, raw, read_size, small_value};

/// The byte slice, window bit offset and bit count a `binary` segment draws from,
/// or `None` when the operand is not a bitstring — the C `is_not_bitstring(Src)` ->
/// `$BS_FAIL_INFO(..., BADARG, am_type, Src)` arm (`emu/bs_instrs.tab:390`, `:650`).
///
/// The offset is the source's own window offset (`0` for a byte-aligned subject,
/// non-zero for a refc binary whose window does not start on a byte boundary); the
/// caller threads it into the bit index it copies from, so a non-byte-aligned source
/// is copied bit-exactly.
pub(super) fn segment_source<'a>(
    machine: &Machine,
    module: &Module,
    operand: &Operand,
    ctx: &mut dyn ExecutionContext,
) -> Result<Option<(&'a [u8], usize, usize)>> {
    let value = read_value(machine, module, ctx, operand)?;
    let Some((_, bits, _)) = raw::heap_bits(value) else {
        return Ok(None);
    };
    let (bytes, off) = raw::subject_bytes(value)?;
    Ok(Some((bytes, off, bits)))
}

/// The `StrT` bytes a `string` constant segment appends.
///
/// The value operand is the string's byte offset into [`Module::strings`]
/// (`beam_asm.erl:771-773` writes it as an untagged integer) and the size operand
/// is its byte count; C computes `size * 8` bits and the unit operand is asserted
/// to be 8 (`generators.tab:994-1001`) and otherwise unused.
///
/// A value operand that is not an integer, a size that is `all`/negative, or a
/// range outside the `StrT` table is a typed [`crate::error::unsupported`] — a
/// malformed segment is never turned into a wrong byte sequence.
pub(super) fn string_source<'a>(
    module: &'a Module,
    value_op: &Operand,
    size_op: &Operand,
    machine: &Machine,
    ctx: &mut dyn ExecutionContext,
) -> Result<&'a [u8]> {
    let Operand::Integer(offset) = value_op else {
        return Err(error::unsupported(
            "bs_create_bin: string segment value is not a StrT offset",
        ));
    };
    if is_all(module, size_op) {
        return Err(error::unsupported("bs_create_bin: string segment size `all`"));
    }
    let size = read_size(machine, ctx, size_op)?;
    let start = usize::try_from(*offset)
        .map_err(|_| error::unsupported("bs_create_bin: negative StrT offset"))?;
    let end = start
        .checked_add(size)
        .ok_or_else(|| error::unsupported("bs_create_bin: string segment size overflow"))?;
    module
        .strings
        .get(start..end)
        .ok_or_else(|| error::unsupported("bs_create_bin: string segment outside the StrT table"))
}

/// The `f64` a float-segment value denotes: an Erlang float, a small integer, or a
/// **bignum** (its nearest finite double).
///
/// C's `erts_bs_put_float` (`erl_bits.c:1341`) has three accepted value shapes — a
/// float, a small integer, and a **bignum** via `is_big(arg)` -> `big_to_double`
/// (`:1371` for the 64-bit arm and `:1404` for the 32-bit arm). `big_to_double`'s
/// `< 0` return — a magnitude too large for a finite double — sets
/// `c_p->fvalue = am_no_float` and returns the arg, which the construct loop raises
/// as the bare `badarg`; that is the `None` arm here.
///
/// [`beam_term::number::big_to_f64`] is the faithful `big_to_double` port (the same
/// helper `families/floats/convert.rs` uses), so a bignum is **encoded** rather than
/// wrongly rejected, and only an out-of-range magnitude still fails.
pub(super) fn float_source(value: Eterm) -> Option<f64> {
    if let Some(f) = crate::terms::float_value(value).ok() {
        return Some(f);
    }
    if let Some(n) = small_value(value) {
        return Some(n as f64);
    }
    // A small integer takes the arm above; a bignum here. A non-number (or a
    // magnitude that overflows a finite double) yields `None` -> `badarg`.
    beam_term::number::big_to_f64(value)
}

// ---------------------------------------------------------------------------
// Encoders (`erl_bits.c`)
// ---------------------------------------------------------------------------

/// `erts_bs_put_float` (`erl_bits.c:1341`) for a byte-aligned destination: the
/// `nbits`-bit big- or little-endian representation of `f`.
pub(super) fn put_float(f: f64, nbits: usize, little: bool) -> Vec<u8> {
    let mut bytes = match nbits {
        16 => f64_to_f16(f).to_be_bytes().to_vec(),
        32 => (f as f32).to_bits().to_be_bytes().to_vec(),
        _ => f.to_bits().to_be_bytes().to_vec(),
    };
    if little {
        bytes.reverse();
    }
    bytes
}

/// `f64_to_f16` (`erl_bits.c:431`): round-to-nearest-even conversion of a finite
/// double to IEEE-754 binary16, ported line for line.
pub(super) fn f64_to_f16(fp: f64) -> u16 {
    let u64bits = fp.to_bits();
    let sign = ((u64bits >> 48) & 0x8000) as u16;
    let exp = ((u64bits >> 52) & 0x7FF) as i32;
    let mut man = u64bits & ((1u64 << 52) - 1);
    if exp == 0 {
        // Subnormal doubles are far below the binary16 subnormal range: they round
        // to zero.
        return sign;
    }
    let mut e = exp - 1023;
    man |= 1u64 << 52; // Full 53-bit significand.
    if e >= -14 {
        if e > 15 {
            return sign | 0x7C00; // Overflow becomes infinity.
        }
        let mut k = man >> 42; // 11 bits: 1024..2047
        let guard = (man >> 41) & 1;
        let sticky = man & ((1u64 << 41) - 1) != 0;
        if guard != 0 && (sticky || (k & 1) != 0) {
            k += 1;
            if k == 2048 {
                k = 1024;
                e += 1;
                if e > 15 {
                    return sign | 0x7C00;
                }
            }
        }
        return sign | ((e + 15) as u16) << 10 | (k as u16 & 0x3FF);
    }
    // Subnormal value in binary16: RNE(|fp| * 2^24).
    let shift = 28 - e; // e <= -15, so shift >= 43.
    if shift > 63 {
        return sign; // Underflow to zero.
    }
    let shift = shift as u32;
    let mut keep = man >> shift;
    let guard = (man >> (shift - 1)) & 1;
    let low = man & ((1u64 << (shift - 1)) - 1);
    let sticky = low != 0;
    if guard != 0 && (sticky || (keep & 1) != 0) {
        keep += 1;
    }
    sign | keep as u16
}

/// `erts_bs_put_utf8` (`erl_bits.c:1162`): encode a non-negative scalar as UTF-8.
/// `None` for a negative value, a surrogate, or a value ≥ U+110000.
pub(super) fn put_utf8(value: i64) -> Option<Vec<u8>> {
    if value < 0 || value >= 0x110000 {
        return None;
    }
    let v = value as u32;
    if (0x800..0x10000).contains(&v) && (0xD800..=0xDFFF).contains(&v) {
        return None;
    }
    let mut out = Vec::with_capacity(4);
    if v < 0x80 {
        out.push(v as u8);
    } else if v < 0x800 {
        out.push(0xC0 | (v >> 6) as u8);
        out.push(0x80 | (v & 0x3F) as u8);
    } else if v < 0x10000 {
        out.push(0xE0 | (v >> 12) as u8);
        out.push(0x80 | ((v >> 6) & 0x3F) as u8);
        out.push(0x80 | (v & 0x3F) as u8);
    } else {
        out.push(0xF0 | (v >> 18) as u8);
        out.push(0x80 | ((v >> 12) & 0x3F) as u8);
        out.push(0x80 | ((v >> 6) & 0x3F) as u8);
        out.push(0x80 | (v & 0x3F) as u8);
    }
    Some(out)
}

/// `erts_bs_put_utf16` (`erl_bits.c:1222`): encode a scalar as UTF-16, big- or
/// little-endian. `None` for a value > U+10FFFF or a surrogate.
pub(super) fn put_utf16(value: i64, little: bool) -> Option<Vec<u8>> {
    if value < 0 || value > 0x10FFFF {
        return None;
    }
    let v = value as u32;
    if (0xD800..=0xDFFF).contains(&v) {
        return None;
    }
    let mut words: Vec<u16> = Vec::with_capacity(2);
    if v < 0x10000 {
        words.push(v as u16);
    } else {
        let t = v - 0x10000;
        words.push(0xD800 | (t >> 10) as u16);
        words.push(0xDC00 | (t & 0x3FF) as u16);
    }
    let mut out = Vec::with_capacity(words.len() * 2);
    for w in words {
        if little {
            out.extend_from_slice(&w.to_le_bytes());
        } else {
            out.extend_from_slice(&w.to_be_bytes());
        }
    }
    Some(out)
}

/// `erts_bs_put_utf32`: encode a scalar as 32-bit big-endian. `None` for a value
/// > U+10FFFF or a surrogate.
pub(super) fn put_utf32(value: i64) -> Option<Vec<u8>> {
    if value < 0 || value > 0x10FFFF {
        return None;
    }
    let v = value as u32;
    if (0xD800..=0xDFFF).contains(&v) {
        return None;
    }
    Some(v.to_be_bytes().to_vec())
}
