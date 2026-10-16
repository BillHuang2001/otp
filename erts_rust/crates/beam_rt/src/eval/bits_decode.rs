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

//! Pure bit-field decoders for the `-eval` binary **pattern** matcher.
//!
//! Every function here reads a field out of a *bit stream* — an `&[u8]` plus a starting bit
//! offset, with each byte's bits MSB-first, exactly the `(bytes, offset_bits, size_bits)`
//! view [`beam_codec::term::bitstring_bytes`] gives a bitstring term — and reports an
//! **invalid, truncated or unrepresentable** field as `None`. That `None` is always a
//! *match failure* at the call site ([`super::Evaluator::match_binary_segments`]), never a
//! raise: Erlang's rule is that a `<<…>>` pattern segment that cannot be read (a width the
//! oracle rejects, fewer bits than the segment needs, a malformed UTF-8/16/32 sequence, a
//! value past `16#10FFFF`) simply does not match.
//!
//! Nothing here is `Evaluator`-specific (no heap, no context), so each decoder is
//! unit-testable against the system `erl`'s own output in isolation; the values are pinned
//! by the `#[cfg(test)]` module at the bottom of this file, and the same tables are
//! cross-checked live against the oracle there.
//!
//! The legacy C ground truth each decoder reproduces:
//! `erts_bs_get_float_2` (`erts/emulator/beam/erl_bits.c:549-611`),
//! `erts_bs_get_utf8` (`:2128-2221`), `erts_bs_get_utf16` (`:2223-2285`), the
//! `bs_get_integer2` + `i_bs_validate_unicode_retract` pair that implements a utf32 read
//! (`erts/emulator/beam/emu/ops.tab:1296-1309`), and `erts_bs_get_integer_2` (`:169-380`).

/// Whether bit `bit` of the stream is set (`None` once the stream has ended).
///
/// The stream is MSB-first within each byte, so bit `b` lives in byte `b / 8` at mask
/// `0x80 >> (b % 8)`.
fn bit_at(bytes: &[u8], bit: usize) -> Option<bool> {
    if bit >= bytes.len() * 8 {
        return None;
    }
    Some(bit_get(bytes, bit))
}

/// [`bit_at`] without the bounds check, for the readers whose *documented precondition* is
/// that the field lies inside the stream.
fn bit_get(bytes: &[u8], bit: usize) -> bool {
    bytes[bit / 8] & (0x80 >> (bit % 8)) != 0
}

/// The eight bits at `bit`, MSB-first, as a byte (`None` when fewer than 8 bits remain).
fn read_u8(bytes: &[u8], bit: usize) -> Option<u8> {
    (0..8).try_fold(0u8, |value, index| {
        Some((value << 1) | u8::from(bit_at(bytes, bit + index)?))
    })
}

/// The `n_bytes` whole bytes starting at `bit`, each read MSB-first (`None` when the stream
/// is shorter than the field).
fn field_bytes(bytes: &[u8], bit: usize, n_bytes: usize) -> Option<Vec<u8>> {
    let end = bit.checked_add(n_bytes.checked_mul(8)?)?;
    if end > bytes.len().checked_mul(8)? {
        return None;
    }
    (0..n_bytes)
        .map(|index| read_u8(bytes, bit + 8 * index))
        .collect()
}

/// The byte width of an IEEE 754 float field the oracle accepts, or `None` for a width it
/// does not.
///
/// Only binary16/32/64 are floats (`erts_bs_get_float_2`'s `num_bits == 16|32|64` arms,
/// `erl_bits.c:569-577`); `<<F:8/float>>` and `<<F:24/float>>` do not match.
fn width_bytes(width_bits: usize) -> Option<usize> {
    match width_bits {
        16 => Some(2),
        32 => Some(4),
        64 => Some(8),
        _ => None,
    }
}

/// Decodes an IEEE 754 float field of `width_bits` bits at `start_bit`.
///
/// `width_bits` is one of `16`/`32`/`64` (binary16/32/64); any other width — and any field
/// that does not fit in `bytes` — is `None`, which the caller turns into a **match failure**
/// (`<<F:24/float>>` does not match, it does not raise).
///
/// `little` reverses the field's **whole byte image**, which is what the oracle's
/// `little`/`native` specifiers do for a float segment (`erts_copy_bits_rev` into the last
/// byte of the field, `erl_bits.c:579-587`): a 64-bit `<<1.5:64/float>>` read back with
/// `float-native` is the denormal `3.13984e-319`, and a 16-bit field's two bytes are swapped
/// the same way. `native` is little-endian on the platforms this runtime supports, and the
/// caller resolves it through its own [`super::integer_segment_is_little`].
pub(crate) fn decode_float(
    bytes: &[u8],
    start_bit: usize,
    width_bits: usize,
    little: bool,
) -> Option<f64> {
    let mut field = field_bytes(bytes, start_bit, width_bytes(width_bits)?)?;
    if little {
        field.reverse();
    }
    let raw = field
        .iter()
        .fold(0u64, |value, byte| (value << 8) | u64::from(*byte));
    Some(match width_bits {
        16 => f16_to_f64(raw as u16),
        32 => f64::from(f32::from_bits(raw as u32)),
        _ => f64::from_bits(raw),
    })
}

/// The `f64` an IEEE 754 binary16 bit pattern denotes (subnormals, zero, infinity and `NaN`
/// included).
fn f16_to_f64(bits: u16) -> f64 {
    let exponent = i32::from((bits >> 10) & 0x1F);
    let fraction = f64::from(bits & 0x03FF);
    let magnitude = match exponent {
        0 => fraction * 2f64.powi(-24),
        31 if fraction == 0.0 => f64::INFINITY,
        31 => f64::NAN,
        _ => (1.0 + fraction / 1024.0) * 2f64.powi(exponent - 15),
    };
    if bits & 0x8000 == 0 {
        magnitude
    } else {
        -magnitude
    }
}

/// Decodes a UTF-8 sequence at `start_bit`: the code point and the bits it consumed
/// (8, 16, 24 or 32).
///
/// `None` — a match failure — for a truncated sequence, a stray continuation byte, a bad
/// leader (`16#80`..=`16#C1`, `16#F5`..=`16#FF`), an overlong encoding, a surrogate
/// (`16#D800`..=`16#DFFF`, which UTF-8 must never carry) or anything past `16#10FFFF` —
/// exactly the set `erts_bs_get_utf8` rejects (`erl_bits.c:2140-2219`; the leader table
/// rejects `16#C0`/`16#C1` as overlong and `16#F8`..`16#FF` as never-valid, and the
/// `result == 0xE0 && a < 0xA0` / `result == 0xF0 && a < 0x90` / `result > 0x10FFFF`
/// checks are the same minimum-code-point and range rules spelled out as bounds).
pub(crate) fn decode_utf8(bytes: &[u8], start_bit: usize) -> Option<(u32, usize)> {
    let first = read_u8(bytes, start_bit)?;
    let (length, mut code) = match first {
        0x00..=0x7F => return Some((u32::from(first), 8)),
        0xC2..=0xDF => (2usize, u32::from(first & 0x1F)),
        0xE0..=0xEF => (3, u32::from(first & 0x0F)),
        0xF0..=0xF4 => (4, u32::from(first & 0x07)),
        _ => return None,
    };
    for index in 1..length {
        let byte = read_u8(bytes, start_bit + 8 * index)?;
        if byte & 0xC0 != 0x80 {
            return None;
        }
        code = (code << 6) | u32::from(byte & 0x3F);
    }
    let minimum = match length {
        2 => 0x80,
        3 => 0x800,
        _ => 0x10000,
    };
    if code < minimum || (0xD800..=0xDFFF).contains(&code) || code > 0x10FFFF {
        return None;
    }
    Some((code, 8 * length))
}

/// One 16-bit UTF-16 code *unit* at `bit`: two stream bytes, byte-swapped when `little`
/// (`little` swaps each unit, never the unit order — `erl_bits.c:2258-2262`).
fn read_u16_unit(bytes: &[u8], bit: usize, little: bool) -> Option<u16> {
    let first = read_u8(bytes, bit)?;
    let second = read_u8(bytes, bit + 8)?;
    Some(if little {
        u16::from(first) | (u16::from(second) << 8)
    } else {
        (u16::from(first) << 8) | u16::from(second)
    })
}

/// Decodes a UTF-16 sequence at `start_bit`: the code point and the bits it consumed (16 for
/// a BMP code point, 32 for a surrogate pair).
///
/// `None` — a match failure — for a truncated sequence, a lone low surrogate
/// (`16#DC00`..=`16#DFFF`) or a high surrogate not followed by a low one
/// (`erl_bits.c:2263-2284`).
pub(crate) fn decode_utf16(bytes: &[u8], start_bit: usize, little: bool) -> Option<(u32, usize)> {
    let first = read_u16_unit(bytes, start_bit, little)?;
    if !(0xD800..=0xDFFF).contains(&first) {
        return Some((u32::from(first), 16));
    }
    // A unit above `16#DBFF` is a *low* surrogate, which is not a code point on its own.
    if first > 0xDBFF {
        return None;
    }
    let second = read_u16_unit(bytes, start_bit + 16, little)?;
    if !(0xDC00..=0xDFFF).contains(&second) {
        return None;
    }
    let code = 0x10000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00);
    Some((code, 32))
}

/// Decodes a UTF-32 sequence at `start_bit`: the code point and the 32 bits it consumed.
///
/// `little`/`native` reverse all four bytes (`bs_get_integer2` with a 32-bit width and the
/// `BSF_LITTLE` flag). `None` — a match failure — for a truncated sequence, a surrogate
/// (`16#D800`..=`16#DFFF`) or anything past `16#10FFFF`: a utf32 read is a 32-bit integer
/// read followed by `i_bs_validate_unicode_retract` (`emu/ops.tab:1296-1309`, whose check is
/// `i > 0x10FFFF || 0xD800 <= i <= 0xDFFF`).
pub(crate) fn decode_utf32(bytes: &[u8], start_bit: usize, little: bool) -> Option<(u32, usize)> {
    let mut code = 0u32;
    for index in 0..4 {
        code = (code << 8) | u32::from(read_u8(bytes, start_bit + 8 * index)?);
    }
    if little {
        code = code.swap_bytes();
    }
    if (0xD800..=0xDFFF).contains(&code) || code > 0x10FFFF {
        return None;
    }
    Some((code, 32))
}

/// The raw `n_bits` bits at `start_bit` as **magnitude limbs**, least significant limb first.
///
/// `little` selects the oracle's two integer layouts:
///
/// * **big-endian** (`None`/`big`): the field is one `n_bits`-wide unsigned integer, most
///   significant bit first;
/// * **little-endian** (`little`/`native`): the field's whole bytes follow
///   **least-significant first** and a width that is not a multiple of 8 leaves the
///   remaining `n_bits % 8` **top** bits as the high bits of the next (partial) byte — the
///   bit stream the oracle's own `llittle` writer emits (`<<255:12/little>>` is
///   `<<255,0:4>>`, `<<-300:12/little>>` is `<<212,14:4>>`, and `integer_le_image`
///   (`bits.rs`) is its construction twin).
///
/// Exact for **any** `n_bits`, including widths that are neither a multiple of 8 nor of 64:
/// the returned limbs are the field's magnitude, with no sign applied (the caller
/// sign-extends within `n_bits` when the segment is `signed`).
///
/// # Panics
///
/// The caller must have established that the field fits the stream (`start_bit + n_bits <=
/// bytes.len() * 8`) — [`super::Evaluator::match_binary_segments`] checks the segment's bit
/// width against the bits that remain before calling. A field that runs past the stream is a
/// caller bug, and reading it would be a wrong value rather than a match failure.
pub(crate) fn read_integer_limbs(
    bytes: &[u8],
    start_bit: usize,
    n_bits: usize,
    little: bool,
) -> Vec<u64> {
    let mut limbs = vec![0u64; n_bits.div_ceil(64)];
    if little {
        let whole = n_bits / 8;
        for index in 0..whole {
            let byte = (0..8).fold(0u8, |value, j| {
                (value << 1) | u8::from(bit_get(bytes, start_bit + 8 * index + j))
            });
            limbs[index / 8] |= u64::from(byte) << (8 * (index % 8));
        }
        let remainder = n_bits % 8;
        for index in 0..remainder {
            if bit_get(bytes, start_bit + 8 * whole + index) {
                // The partial byte's bits are the value's *top* bits, most significant first.
                let weight = 8 * whole + (remainder - 1 - index);
                limbs[weight / 64] |= 1u64 << (weight % 64);
            }
        }
        return limbs;
    }
    for index in 0..n_bits {
        if bit_get(bytes, start_bit + index) {
            let weight = n_bits - 1 - index;
            limbs[weight / 64] |= 1u64 << (weight % 64);
        }
    }
    limbs
}

#[cfg(test)]
mod tests {
    //! The module's decoder tables, pinned twice: each case asserts the **hardcoded** value
    //! the system `erl` produced on this host, and then — when a live oracle is available —
    //! evaluates the identical expression under `erl` and asserts it still renders that
    //! value, so a table entry can never rot.
    //!
    //! The oracle runs through the self-terminating probe
    //!
    //! ```text
    //! erl -noshell -eval 'try (fun() -> <EXPR> end)() of V -> io:format("ok ~w~n",[V]), halt(0)
    //!                      catch C:R -> io:format("exc ~w ~w~n",[C,R]), halt(1) end.'
    //! ```
    //!
    //! with its mandatory `(fun() -> … end)()` wrapper (`erl_eval` cannot evaluate a `case`/
    //! binary-pattern expression directly inside a `try` body — it exits 1 with no output).
    //! The probes are **batched into one `erl` boot** per test function (the same
    //! `{N, fun() -> EXPR end}` entries and the same per-probe `try … catch` body), because
    //! the host is shared and heavily loaded and a boot per case would cost ~1 s each; any
    //! single case can be reproduced by hand with the command above. A missing `erl` is a
    //! **loud skip**, never a silent pass.

    use beam_term::{is_small, signed_value};

    use crate::config::RuntimeConfig;
    use crate::context::RuntimeContext;
    use crate::eval::eval;
    use crate::runtime::Runtime;

    use super::*;

    /// Renders `expr` with the runtime's own `~w` printer.
    ///
    /// The `Runtime` is created per call and dropped with the value, so only the *printed
    /// string* escapes — exactly what a heap-allocated result (a float, a bignum, a tuple)
    /// allows.
    fn runtime_renders(expr: &str) -> String {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::BytesSink::new();
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let printed = format!("io:format(\"~w\", [{expr}])");
        eval(&mut ctx, &printed).unwrap_or_else(|error| panic!("{expr}: {error}"));
        drop(ctx);
        sink.as_str()
    }

    /// The oracle's `~w` rendering of each expression, in order — or `None` (after a loud
    /// `SKIP` line) when there is no live oracle to consult.
    fn oracle_renders(exprs: &[&str]) -> Option<Vec<String>> {
        let probes = exprs
            .iter()
            .enumerate()
            .map(|(index, expr)| format!("{{{}, fun() -> {expr} end}}", index + 1))
            .collect::<Vec<_>>()
            .join(",\n");
        let program = format!(
            "L = [{probes}],\n\
             lists:foreach(\
             fun({{N,F}}) ->\n\
             \x20 try F() of V -> io:format(\"~w ~w~n\",[N,V])\n\
             \x20 catch C:R -> io:format(\"~w exc ~w ~w~n\",[N,C,R]) end end, L),\n\
             halt(0)."
        );
        let output = match std::process::Command::new("erl")
            .args(["-noshell", "-eval", &program])
            .output()
        {
            Ok(output) => output,
            Err(error) => {
                eprintln!(
                    "SKIP: no erl on PATH ({error}) — this file's binary-segment tables are \
                     NOT cross-checked against the oracle"
                );
                return None;
            }
        };
        let text = String::from_utf8_lossy(&output.stdout).into_owned();
        let mut values = Vec::with_capacity(exprs.len());
        for line in text.lines() {
            let Some((index, rest)) = line.split_once(' ') else {
                eprintln!("SKIP: unparsable oracle line {line:?} — not cross-checked");
                return None;
            };
            if index.parse::<usize>() != Ok(values.len() + 1) {
                eprintln!("SKIP: oracle line {line:?} is out of order — not cross-checked");
                return None;
            }
            values.push(rest.to_string());
        }
        if values.len() != exprs.len() {
            eprintln!(
                "SKIP: the oracle probe printed {} line(s) for {} expression(s) — this file's \
                 tables are NOT cross-checked",
                values.len(),
                exprs.len()
            );
            return None;
        }
        Some(values)
    }

    /// Asserts each `exprs[i]` renders as `expected[i]` under **this** runtime and — when the
    /// oracle is available — under the system `erl` too.
    fn assert_renders(exprs: &[&str], expected: &[&str]) {
        assert_eq!(exprs.len(), expected.len(), "the table is malformed");
        let oracle = oracle_renders(exprs);
        for (index, expr) in exprs.iter().enumerate() {
            let rendered = runtime_renders(expr);
            assert_eq!(
                rendered, expected[index],
                "this runtime rendered `{expr}` as {rendered}"
            );
            if let Some(oracle) = &oracle {
                assert_eq!(
                    oracle[index], expected[index],
                    "the oracle no longer renders `{expr}` as {} — the table is stale (oracle \
                     said {})",
                    expected[index], oracle[index]
                );
            }
        }
    }

    #[test]
    fn float_segments_decode_ieee_fields() {
        // (a) of the oracle table: a float pattern reads an IEEE 754 field whose width
        // defaults to 64 and must be one of 16/32/64; `signed` is ignored, and
        // `little`/`native` reverse the field's whole byte image.
        assert_renders(
            &[
                "case <<1.5/float>> of <<F/float>> -> F end",
                "case <<1.5:64/float>> of <<F:64/float-native>> -> F end",
                "case <<1.5:64/float>> of <<F/float-native>> -> F end",
                "case <<1.5:32/float>> of <<F:32/float>> -> F end",
                "case <<1.5:32/float-little>> of <<F:32/float-little>> -> F end",
                "case <<1.5:32/float-native>> of <<F:32/float>> -> F end",
                "case <<1.5:16/float>> of <<F:16/float>> -> F end",
                "case <<0,62>> of <<F:16/float>> -> F end",
                "case <<1.5:16/float>> of <<F:16/float-little>> -> F end",
                "case <<1,2,3,4,5,6,7,8>> of <<F:16/float,Rest/binary>> -> {F,Rest} end",
                "case <<1.5:64/signed-float>> of <<F:64/float>> -> F end",
                // A width the oracle does not accept is a MATCH FAILURE, not a raise.
                "case <<1.5:64/float>> of <<F:16/float>> -> F; _ -> nomatch end",
                "case <<0,0,0,0,0,0,0,0>> of <<F:24/float,Rest/binary>> -> {F,Rest}; _ -> nomatch end",
                "case <<0,0,0,0>> of <<F:24/float,Rest/binary>> -> {F,Rest}; _ -> nomatch end",
                // Truncated: fewer bits remain than the segment needs.
                "case <<0,0,0>> of <<F:32/float>> -> F; _ -> nomatch end",
            ],
            &[
                "1.5",
                "3.13984e-319",
                "3.13984e-319",
                "1.5",
                "1.5",
                "6.896490392174587e-41",
                "1.5",
                "3.6954879760742188e-6",
                "3.6954879760742188e-6",
                "{1.537799835205078e-5,<<3,4,5,6,7,8>>}",
                "1.5",
                "nomatch",
                "nomatch",
                "nomatch",
                "nomatch",
            ],
        );
    }

    #[test]
    fn utf_segments_decode_code_points() {
        // (b),(c),(d) of the oracle table: utf8 consumes 1..4 bytes, utf16 one 2-byte unit
        // or a 4-byte surrogate pair, utf32 four bytes; an overlong/surrogate/out-of-range/
        // truncated sequence is a MATCH FAILURE.
        assert_renders(
            &[
                "case <<16#41/utf8>> of <<C/utf8>> -> C end",
                "case <<16#7FF/utf8>> of <<C/utf8>> -> C end",
                "case <<16#E0,16#A0,16#80>> of <<C/utf8>> -> C end",
                "case <<16#10FFFF/utf8>> of <<C/utf8>> -> C end",
                "case <<16#E2,16#82,16#AC,65>> of <<C/utf8,Rest/binary>> -> {C,Rest} end",
                "case <<16#C0,16#80>> of <<C/utf8>> -> C; _ -> nomatch end",
                "case <<16#FF>> of <<C/utf8>> -> C; _ -> nomatch end",
                "case <<16#ED,16#A0,16#80>> of <<C/utf8>> -> C; _ -> nomatch end",
                "case <<16#E2,16#82>> of <<C/utf8>> -> C; _ -> nomatch end",
                "case <<16#E2,16#41,16#AC>> of <<C/utf8>> -> C; _ -> nomatch end",
                "case <<16#F4,16#90,16#80,16#80>> of <<C/utf8>> -> C; _ -> nomatch end",
                "case <<16#F0,16#90,16#80,16#80>> of <<C/utf8>> -> C end",
                "case <<16#41/utf16>> of <<C/utf16>> -> C end",
                "case <<16#41/utf16-little>> of <<C/utf16-little>> -> C end",
                "case <<16#41/utf16>> of <<C/utf16-little>> -> C end",
                "case <<16#10000/utf16>> of <<C/utf16>> -> C end",
                "case <<16#10FFFF/utf16>> of <<C/utf16>> -> C end",
                "case <<16#10FFFF/utf16-little>> of <<C/utf16-little>> -> C end",
                "case <<255,219,255,223>> of <<C/utf16-little>> -> C end",
                "case <<255,223,255,219>> of <<C/utf16-little>> -> C; _ -> nomatch end",
                "case <<255,255,219,255>> of <<C/utf16-little>> -> C; _ -> nomatch end",
                "case <<16#D8,0>> of <<C/utf16>> -> C; _ -> nomatch end",
                "case <<16#D8,0,16#41,0>> of <<C/utf16>> -> C; _ -> nomatch end",
                "case <<16#D8,0,16#DC,0>> of <<C/utf16>> -> bad_encoding; _ -> nomatch end",
                "case <<16#41/utf16,65>> of <<C/utf16,Rest/binary>> -> {C,Rest} end",
                "case <<1234/utf32>> of <<C/utf32>> -> C end",
                "case <<0,0,0,0>> of <<C/utf32>> -> C end",
                "case <<16#10FFFF/utf32>> of <<C/utf32>> -> C end",
                "case <<16#FF,16#10,0,0>> of <<C/utf32-little>> -> C end",
                "case <<0,0,16#10,16#FF>> of <<C/utf32-little>> -> C; _ -> nomatch end",
                "case <<1234/utf32>> of <<C/utf32-signed>> -> C end",
                "case <<0,0,16#D8,0>> of <<C/utf32>> -> C; _ -> nomatch end",
                "case <<0,0,0,65,255>> of <<C/utf32,Rest/binary>> -> {C,Rest} end",
            ],
            &[
                "65",
                "2047",
                "2048",
                "1114111",
                "{8364,<<65>>}",
                "nomatch",
                "nomatch",
                "nomatch",
                "nomatch",
                "nomatch",
                "nomatch",
                "65536",
                "65",
                "65",
                "16640",
                "65536",
                "1114111",
                "1114111",
                "1114111",
                "nomatch",
                "nomatch",
                "nomatch",
                "nomatch",
                "bad_encoding",
                "{65,<<65>>}",
                "1234",
                "0",
                "1114111",
                "4351",
                "nomatch",
                "1234",
                "nomatch",
                "{65,<<255>>}",
            ],
        );
    }

    #[test]
    fn integer_segments_decode_any_width_exactly() {
        // (e) of the oracle table: an integer pattern segment of ANY width — a value the
        // oracle materialises as a bignum (>= 60 bits, or a negative magnitude outside the
        // small range) is built exactly here too, and a segment that cannot consume the
        // value is a MATCH FAILURE rather than a `system_limit`.
        assert_renders(
            &[
                "case <<255,255,255,255,255,255,255,255>> of <<A:64>> -> A end",
                "case <<255,255,255,255,255,255,255,255>> of <<A:64/signed>> -> A end",
                "case <<255,255,255,255,255,255,255,255>> of <<A:64/little>> -> A end",
                "case <<1,2,3,4,5,6,7,8>> of <<A:64/little>> -> A end",
                "case <<255,255,255,255,255,255,255,255,255>> of <<A:72>> -> A end",
                "case <<255,255,255,255,255,255,255,255,255>> of <<A:72/signed>> -> A end",
                "case <<0,0,0,0,0,3,141,0,0,0,0,0,0,0,0>> of <<A:120>> -> A end",
                "case <<255,255,255,255,255,255,255,255,255,255>> of <<A:80,_/binary>> -> A end",
                "case <<1,2,3,4,5,6,7,8,9>> of <<A:72/little>> -> A end",
                "case <<1,2,3,4,5,6,7,8,9>> of <<A:72/signed-little>> -> A end",
                "case <<255,255,255,255,255,255,255,255,255>> of <<A:72/signed-little>> -> A end",
                "case <<255,255,255,255,255,255,255,255>> of <<A:60>> -> A; _ -> nomatch end",
                "case <<16#0FFFFFFFFFFFFFFF:60>> of <<A:60>> -> A end",
                "case <<(1 bsl 59):60>> of <<A:60/signed>> -> A end",
                "case <<(1 bsl 59):60>> of <<A:60>> -> A end",
                "case <<16#80,0,0,0,0,0,0,0>> of <<A:64/signed>> -> A end",
                "case <<>> of <<A:0>> -> A end",
                "case <<16#12,16#34>> of <<A:2/unit:8>> -> A end",
                "case <<16#12,16#34>> of <<A:1/little-unit:8>> -> A; _ -> nomatch end",
                "case <<1:1,16#12,16#34>> of <<_:1,A:16>> -> A end",
                "case <<1:1,16#12,16#34>> of <<_:1,A:8,Rest/binary>> -> {A,Rest} end",
                "case <<0:4,(<<255:12/little>>)/bitstring>> of <<_:4,A:12/little>> -> A end",
                "case <<0:4,(<<3796:12>>)/bitstring>> of <<_:4,A:12/little>> -> A; _ -> nomatch end",
                "case <<1:3,16#12,16#34>> of <<_:1,A:2,_/bitstring>> -> A end",
                // A little-endian field whose width is not a whole number of bytes: the
                // field's trailing partial byte holds its *high* bits, so `<<212,14:4>>`
                // reads back as `16#ED4` (`-300` when `signed`) — the case a byte-wise
                // gather gets wrong, and the one `read_integer_limbs` exists for.
                "case <<212,14:4>> of <<A:12/little>> -> A end",
                "case <<212,14:4>> of <<A:12/signed-little>> -> A end",
                "case <<-300:12/little>> of <<A:12/little>> -> A end",
                // … and past a word, with the field itself starting off byte-aligned.
                "case <<1,2,3,4,5,6,7,8,9,10>> of <<_:3,A:77/little>> -> A end",
                "case <<1,2,3,4,5,6,7,8,9,10>> of <<_:3,A:77/signed-little>> -> A end",
                "case <<255,255,255,255,255,255,255,255,255,255>> of <<_:3,A:77/signed-little>> -> A end",
                "case <<1,2,3,4,5,6,7,8,9,10>> of <<_:5,A:75>> -> A end",
                "case <<1,2,3,4,5,6,7,8,9,10>> of <<_:5,A:75/signed>> -> A end",
                "case <<255,255,255,255,255,255,255,255,255,255>> of <<_:1,A:79/signed>> -> A end",
            ],
            &[
                "18446744073709551615",
                "-1",
                "18446744073709551615",
                "578437695752307201",
                "4722366482869645213695",
                "-1",
                "16768090363001982418944",
                "1208925819614629174706175",
                "166599134359138271745",
                "166599134359138271745",
                "-1",
                "nomatch",
                "1152921504606846975",
                "-576460752303423488",
                "576460752303423488",
                "-9223372036854775808",
                "0",
                "4660",
                "nomatch",
                "4660",
                "{18,<<52>>}",
                "255",
                "1261",
                "1",
                "3796",
                "-300",
                "3796",
                "48556457903569558310920",
                "48556457903569558310920",
                "-1",
                "4759477275222530853130",
                "4759477275222530853130",
                "-1",
            ],
        );
    }

    /// The pure decoders, asserted directly on the bit patterns the oracle spells above —
    /// no runtime, no oracle, and no dependence on the `-eval` parser.
    #[test]
    fn the_pure_decoders_read_the_oracles_bit_patterns() {
        // IEEE 754: binary16 subnormal, binary16 1.5, binary32 1.5, binary64 1.5.
        assert_eq!(
            decode_float(&[0x00, 0x3E], 0, 16, false),
            Some(62.0 * 2f64.powi(-24))
        );
        assert_eq!(decode_float(&[0x3E, 0x00], 0, 16, false), Some(1.5));
        assert_eq!(
            decode_float(&[0x3E, 0x00], 0, 16, true),
            Some(62.0 * 2f64.powi(-24))
        );
        assert_eq!(decode_float(&[0x3F, 0xC0, 0, 0], 0, 32, false), Some(1.5));
        assert_eq!(
            decode_float(&[0x3F, 0xF8, 0, 0, 0, 0, 0, 0], 0, 64, false),
            Some(1.5)
        );
        // A width the oracle rejects, and a field that does not fit the slice (here the
        // 16-bit field starting at bit 8 needs bit 24, but the slice ends at bit 16).
        assert_eq!(decode_float(&[0, 0, 0], 0, 24, false), None);
        assert_eq!(decode_float(&[0, 0, 0], 0, 32, false), None);
        assert_eq!(decode_float(&[0x3E, 0x00], 8, 16, false), None);

        // UTF-8: 1, 2, 3 and 4 byte sequences, plus the rejected shapes.
        assert_eq!(decode_utf8(&[0x41], 0), Some((0x41, 8)));
        assert_eq!(decode_utf8(&[0xDF, 0xBF], 0), Some((0x7FF, 16)));
        assert_eq!(decode_utf8(&[0xE2, 0x82, 0xAC, 65], 0), Some((0x20AC, 24)));
        assert_eq!(
            decode_utf8(&[0xF4, 0x8F, 0xBF, 0xBF], 0),
            Some((0x10FFFF, 32))
        );
        assert_eq!(decode_utf8(&[0xC0, 0x80], 0), None);
        assert_eq!(decode_utf8(&[0xED, 0xA0, 0x80], 0), None);
        assert_eq!(decode_utf8(&[0xE2, 0x82], 0), None);
        assert_eq!(decode_utf8(&[0xE2, 0x41, 0xAC], 0), None);
        assert_eq!(decode_utf8(&[0xFF], 0), None);
        assert_eq!(decode_utf8(&[0xF4, 0x90, 0x80, 0x80], 0), None);

        // UTF-16: a BMP unit, a surrogate pair (either byte order) and the bad shapes.
        assert_eq!(decode_utf16(&[0x00, 0x41], 0, false), Some((0x41, 16)));
        assert_eq!(
            decode_utf16(&[0x00, 0xD8, 0x00, 0xDC], 0, true),
            Some((0x10000, 32))
        );
        assert_eq!(
            decode_utf16(&[0xFF, 0xDB, 0xFF, 0xDF], 0, true),
            Some((0x10FFFF, 32))
        );
        assert_eq!(
            decode_utf16(&[0xDB, 0xFF, 0xDF, 0xFF], 0, false),
            Some((0x10FFFF, 32))
        );
        assert_eq!(decode_utf16(&[0xFF, 0xDB], 0, false), Some((0xFFDB, 16)));
        assert_eq!(decode_utf16(&[0xD8, 0x00], 0, false), None);
        assert_eq!(decode_utf16(&[0xDC, 0x00], 0, false), None);
        assert_eq!(decode_utf16(&[0xD8, 0x00, 0x41, 0x00], 0, false), None);
        assert_eq!(decode_utf16(&[0xFF, 0xDF, 0xFF, 0xDB], 0, true), None);

        // UTF-32: four bytes, either byte order, plus the surrogate / range / truncation
        // rejections.
        assert_eq!(
            decode_utf32(&[0, 0, 0x04, 0xD2], 0, false),
            Some((1234, 32))
        );
        assert_eq!(decode_utf32(&[0xD2, 0x04, 0, 0], 0, true), Some((1234, 32)));
        assert_eq!(decode_utf32(&[0, 0x11, 0, 0], 0, false), None);
        assert_eq!(decode_utf32(&[0, 0, 0xD8, 0], 0, false), None);
        assert_eq!(decode_utf32(&[0, 0, 0x04], 0, false), None);

        // Integer limbs: width-exact, limb-reversed (least significant first), for widths
        // that are whole limbs, whole bytes and neither.
        assert_eq!(read_integer_limbs(&[0xFF], 0, 8, false), vec![0xFF]);
        assert_eq!(
            read_integer_limbs(&[0x01, 0x02], 0, 16, false),
            vec![0x0102]
        );
        assert_eq!(read_integer_limbs(&[0x01, 0x02], 0, 16, true), vec![0x0201]);
        assert_eq!(
            read_integer_limbs(&[1, 2, 3, 4, 5, 6, 7, 8], 0, 64, true),
            vec![0x0807_0605_0403_0201]
        );
        assert_eq!(
            read_integer_limbs(&[0xFF; 9], 0, 72, false),
            vec![u64::MAX, 0xFF]
        );
        assert_eq!(
            read_integer_limbs(&[0xFF; 9], 0, 72, true),
            vec![u64::MAX, 0xFF]
        );
        // `<<255:12/little>>` is `<<255,0:4>>`: 12 bits, the last 4 the value's top bits.
        assert_eq!(read_integer_limbs(&[0xFF, 0x00], 0, 12, true), vec![0xFF]);
        assert_eq!(read_integer_limbs(&[0xFF, 0xF0], 0, 12, true), vec![0xFFF]);
        // A non-byte-aligned field (one leading bit skipped): the eight bits at 1..=8 of
        // `0b0100_0000 1000_0000` are `1000_0001`.
        assert_eq!(read_integer_limbs(&[0x40, 0x80], 1, 8, false), vec![0x81]);
        assert_eq!(
            read_integer_limbs(&[0x10, 0x00], 0, 0, false),
            Vec::<u64>::new()
        );
    }

    /// The width-agnostic limb reader agrees with the two ≤64-bit word readers the
    /// evaluator's fast path uses, wherever both are applicable.
    ///
    /// `read_bits_be` is bit-exact at **every** width up to a word and `sign_extend` is a
    /// plain extension of the low `bits` bits, so the big-endian fast path and this module
    /// cannot disagree. The byte-wise `read_bits_le` is only equivalent for a **whole**
    /// number of bytes: with a `n_bits % 8` remainder it gathers the field's trailing partial
    /// byte as the *most significant* byte, while the field's high bits are in fact that
    /// byte's high bits (`<<212,14:4>>` is `16#ED4`, not `16#E0D4`). That is exactly why the
    /// little-endian fast path is restricted to byte-aligned widths and everything else goes
    /// through [`read_integer_limbs`].
    #[test]
    fn the_limb_reader_agrees_with_the_word_readers() {
        let bytes: Vec<u8> = (0u8..17)
            .map(|index| index.wrapping_mul(37).wrapping_add(0x5A))
            .collect();
        for bits in 1..=64 {
            for start in [0usize, 3, 8] {
                assert_eq!(
                    read_integer_limbs(&bytes, start, bits, false),
                    vec![super::super::read_bits_be(&bytes, start, bits)],
                    "big-endian {bits} bits at bit {start}"
                );
                if bits % 8 == 0 {
                    assert_eq!(
                        read_integer_limbs(&bytes, start, bits, true),
                        vec![super::super::read_bits_le(&bytes, start, bits)],
                        "byte-aligned little-endian {bits} bits at bit {start}"
                    );
                }
            }
            // The signed fast path is `sign_extend` of the field's low `bits` bits.
            assert_eq!(super::super::sign_extend(u64::MAX, bits), -1);
            let top = 1u64 << (bits - 1);
            let expected = if bits == 64 {
                i64::MIN
            } else {
                -(1i64 << (bits - 1))
            };
            assert_eq!(super::super::sign_extend(top, bits), expected);
            assert_eq!(super::super::sign_extend(0, bits), 0);
        }
        // The divergence the little-endian gate exists for, stated as a value: `16#ED4`,
        // not the `16#E0D4` a byte-wise gather of the same field produces.
        assert_eq!(read_integer_limbs(&[0xD4, 0xE0], 0, 12, true), vec![0xED4]);
    }

    /// The `-eval` renderer really is the printer the tables compare against.
    #[test]
    fn the_runtime_renderer_is_the_comparison_printer() {
        assert_eq!(runtime_renders("1.5"), "1.5");
        assert!(is_small(eval_int("16#41")));
        assert_eq!(signed_value(eval_int("16#41")), 0x41);
    }

    /// Evaluates an integer expression (no heap-allocated result escapes).
    fn eval_int(expr: &str) -> beam_term::Eterm {
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = eval(&mut ctx, expr).unwrap_or_else(|error| panic!("{expr}: {error}"));
        assert!(
            beam_term::is_immediate(value),
            "{expr} produced a heap-allocated term"
        );
        value
    }
}
