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

//! Pure bit-construction helpers for the `-eval` `<<…>>` segments.
//!
//! [`Evaluator::append_segment`](super::Evaluator::append_segment) owns the segment
//! semantics — the size resolution, the byte order and every `badarg` it raises — and
//! delegates the pure bit work here, so each helper can be checked directly against the
//! system `erl`. Nothing in this module touches the evaluator, the heap or the runtime's
//! error type.

/// The little-endian byte image of a fixed-width two's-complement integer.
///
/// `words` is the already-widened and (for a negative value) complemented magnitude,
/// `ErtsDigit` limbs least significant first; a limb index past its end reads as zero, which
/// is what the caller's zero padding means.
///
/// A `little` integer segment is laid out **byte**-wise, not limb-wise: the `n_bits / 8`
/// whole bytes of the value follow least-significant first, and a remaining `n_bits % 8`
/// bits are the **low** bits of the value's next byte, pushed into the high bits of the
/// final (partial) byte. The oracle emits exactly this: `<<255:12/little>>` is `<<255,0:4>>`
/// and `<<-300:12/little>>` is `<<212,14:4>>` (`-300` is `16#FED4` in two's complement, so
/// its byte image is `16#D4` followed by the low 4 bits of `16#FE`).
///
/// Returns the whole bytes, the trailing partial field's value (already masked to its width)
/// and that width. The width is `0` — and the caller must then ignore the field — when
/// `n_bits` is a whole number of bytes.
pub(super) fn integer_le_image(words: &[u64], n_bits: usize) -> (Vec<u8>, u64, usize) {
    let byte_at = |index: usize| -> u8 {
        let word = words.get(index / 8).copied().unwrap_or(0);
        (word >> (8 * (index % 8))) as u8
    };
    let full = n_bits / 8;
    let rem = n_bits % 8;
    let bytes = (0..full).map(byte_at).collect();
    // With a partial trailing field, `full < 8 * words.len()` holds: a bit width that is not
    // a multiple of 8 cannot fill the last limb, so `byte_at(full)` is in range.
    let tail = if rem == 0 {
        0
    } else {
        u64::from(byte_at(full)) & super::mask_bits(rem)
    };
    (bytes, tail, rem)
}

/// `2^64`, the radix of one `ErtsDigit` limb (exact as an `f64`).
const LIMB_RADIX: f64 = 18_446_744_073_709_551_616.0;

/// The `f64` nearest `Σ limbs[i] * 2^(64 * i)`, negated when `negative`.
///
/// Horner's rule from the most significant limb down: each step scales by exactly `2^64` (a
/// power of two) and `u64 as f64` is the language's round-to-nearest conversion. A magnitude
/// past the `f64` range overflows to `±inf`, as the oracle's own conversion does for a float
/// segment (`<<(1 bsl 64):64/float>>` is `<<67,240,0,0,0,0,0,0>>` — the exact `2^64`).
pub(super) fn integer_limbs_to_f64(negative: bool, limbs: &[u64]) -> f64 {
    let mut value = 0.0f64;
    for limb in limbs.iter().rev() {
        value = value * LIMB_RADIX + *limb as f64;
    }
    if negative {
        -value
    } else {
        value
    }
}

/// The bytes of an `n_bits`-wide IEEE 754 float segment holding `value`, big-endian; `None`
/// when `n_bits` is not one of the three widths the oracle accepts (binary16/32/64), which
/// the caller turns into `badarg` (`<<1.5:8/float>>` and `<<1.5:24/float>>` are `badarg`).
///
/// `little` reverses the whole field's byte order, which is what the oracle's
/// `little`/`native` specifiers do for a float segment (`<<1.5:32/float-little>>` is
/// `<<0,0,192,63>>`).
pub(super) fn float_bytes(value: f64, n_bits: usize, little: bool) -> Option<Vec<u8>> {
    let mut bytes = match n_bits {
        16 => f64_to_f16(value).to_be_bytes().to_vec(),
        32 => f64_to_f32_bits(value).to_be_bytes().to_vec(),
        64 => value.to_bits().to_be_bytes().to_vec(),
        _ => return None,
    };
    if little {
        bytes.reverse();
    }
    Some(bytes)
}

/// The IEEE 754 **binary16** (half-precision) encoding of `value`.
///
/// Round to nearest, ties to even, subnormal results preserved, overflow to `±inf`, and the
/// sign of a zero or a NaN kept. The oracle's own values: `<<1.5:16/float>>` is `<<62,0>>`,
/// `<<0.1:16/float>>` is `<<46,102>>`, `<<5.0e-8:16/float>>` is `<<0,1>>` (a subnormal),
/// `<<70000.0:16/float>>` is `<<124,0>>` (`+inf`) and `<<-0.0:16/float>>` is `<<128,0>>`.
pub(super) fn f64_to_f16(value: f64) -> u16 {
    let sign = if value.is_sign_negative() { 0x8000u16 } else { 0 };
    // The field is at most `0x7C00` (`+inf`), so the narrowing cast is lossless.
    sign | round_to_ieee(value.abs(), 5, 10) as u16
}

/// The IEEE 754 **binary32** encoding of `value`.
///
/// The same rounding core as [`f64_to_f16`] (round to nearest, ties to even, subnormals
/// preserved, overflow to `±inf`), so `<<1.5:32/float>>` is the oracle's `<<63,192,0,0>>` and
/// `<<1.0e40:32/float>>` its `<<127,128,0,0>>` (`+inf`). Rust's own `f64 as f32` cast is
/// correctly rounded as well, so the module's tests pin this implementation against it over
/// the interesting boundaries instead of asserting the two agree by assumption.
pub(super) fn f64_to_f32_bits(value: f64) -> u32 {
    let sign = if value.is_sign_negative() {
        0x8000_0000u32
    } else {
        0
    };
    sign | round_to_ieee(value.abs(), 8, 23) as u32
}

/// The unsigned IEEE 754 binary interchange field of `magnitude`, with `exp_bits` exponent
/// bits and `mant_bits` mantissa bits (5/10 for binary16, 8/23 for binary32).
///
/// Round to nearest, ties to even; a magnitude below the format's smallest normal is encoded
/// in its subnormal form (a raw mantissa count, which rolls up into the smallest normal
/// encoding when the rounding carries), and anything at or past the format's overflow
/// threshold becomes the format's infinity field. A NaN yields the format's quiet NaN.
fn round_to_ieee(magnitude: f64, exp_bits: u32, mant_bits: u32) -> u64 {
    let bias = (1i32 << (exp_bits - 1)) - 1;
    let min_normal_exponent = 1 - bias;
    let mantissa_scale = (1u64 << mant_bits) as f64;
    let infinity = ((1u64 << exp_bits) - 1) << mant_bits;
    if magnitude.is_nan() {
        return infinity | (1u64 << (mant_bits - 1));
    }
    if magnitude == 0.0 {
        return 0;
    }
    if magnitude < pow2(min_normal_exponent) {
        // Subnormal (or below the format entirely, which rounds to zero): a raw mantissa
        // count of `magnitude / 2^(min_normal_exponent - mant_bits)`.
        let scaled = magnitude * mantissa_scale * pow2(-min_normal_exponent);
        return round_half_even(scaled);
    }
    // Normal: `magnitude = 1.f * 2^exponent`. The `f64` is itself normal here (every
    // format's smallest normal exponent is far inside the `f64` range), so its own biased
    // exponent gives `exponent` directly.
    let mut exponent = ((magnitude.to_bits() >> 52) & 0x7FF) as i32 - 1023;
    let scale = pow2(exponent);
    // Exact: `scale` and `mantissa_scale` are powers of two, `magnitude / scale` is the
    // significand in `[1, 2)`, and `significand * mantissa_scale` (in
    // `[2^mant_bits, 2^(mant_bits + 1))`) minus `mantissa_scale` is exactly representable
    // for `mant_bits <= 52`.
    let mut mantissa = round_half_even(magnitude / scale * mantissa_scale - mantissa_scale);
    if mantissa >= (1u64 << mant_bits) {
        // The rounding carried out of the mantissa into the exponent.
        mantissa = 0;
        exponent += 1;
    }
    if exponent > bias {
        return infinity;
    }
    (((exponent + bias) as u64) << mant_bits) | mantissa
}

/// `2^exponent` as an `f64` (exact for the exponents used here).
fn pow2(exponent: i32) -> f64 {
    f64::from_bits(((exponent + 1023) as u64) << 52)
}

/// `value` — a non-negative `f64` below `2^52` — rounded to an integer, ties to even.
fn round_half_even(value: f64) -> u64 {
    let floor = value.floor();
    let fraction = value - floor;
    if fraction > 0.5 || (fraction == 0.5 && floor as u64 % 2 == 1) {
        (floor + 1.0) as u64
    } else {
        floor as u64
    }
}

/// Whether a utf-type segment (`utf8`/`utf16`/`utf32`) carries an explicit width — a
/// `:Size` or a `unit:`.
///
/// The oracle's parser rejects such a segment outright (`<<16#41:8/utf8>>` and
/// `<<16#41/utf16-unit:8>>` do not parse at all), so there is no width this runtime could
/// honour: the caller raises `badarg` rather than fabricating an encoding. The size
/// expression is deliberately not evaluated — the oracle never reaches it either.
pub(super) fn utf_segment_has_width(segment: &super::Seg) -> bool {
    segment.size.is_some() || segment.unit.is_some()
}

/// The UTF-16 encoding of the code point `code`: one 2-byte unit, or a surrogate pair (two
/// units) above `16#FFFF`. `little` byte-swaps **each unit** but keeps the unit order, as
/// the oracle does (`<<16#10000/utf16-little>>` is `<<0,216,0,220>>`).
///
/// `None` — the caller's `badarg` — for a code point Erlang rejects: negative, a lone
/// surrogate (`16#D800..=16#DFFF`) or above `16#10FFFF` (`<<16#D800/utf16>>` and
/// `<<16#110000/utf16>>` are both `error:badarg` on the oracle).
pub(super) fn utf16_bytes(code: i64, little: bool) -> Option<Vec<u8>> {
    let code = u32::try_from(code).ok()?;
    if code > 0x10FFFF || (0xD800..=0xDFFF).contains(&code) {
        return None;
    }
    let mut units = Vec::with_capacity(2);
    if code < 0x10000 {
        units.push(code as u16);
    } else {
        let offset = code - 0x10000;
        units.push(0xD800 + (offset >> 10) as u16);
        units.push(0xDC00 + (offset & 0x3FF) as u16);
    }
    let mut bytes = Vec::with_capacity(units.len() * 2);
    for unit in units {
        let encoded = if little {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        };
        bytes.extend_from_slice(&encoded);
    }
    Some(bytes)
}

/// The UTF-32 encoding of the code point `code`: one 4-byte unit, `little` reversing it
/// (`<<1234/utf32-little>>` is `<<210,4,0,0>>`, `<<16#10000/utf32>>` is `<<0,1,0,0>>`).
///
/// `None` — the caller's `badarg` — for a code point Erlang rejects: negative, a lone
/// surrogate or above `16#10FFFF` (`<<16#D800/utf32>>` and `<<16#110000/utf32>>` are both
/// `error:badarg` on the oracle, which range-checks a UTF-32 segment exactly as UTF-16).
pub(super) fn utf32_bytes(code: i64, little: bool) -> Option<Vec<u8>> {
    let code = u32::try_from(code).ok()?;
    if code > 0x10FFFF || (0xD800..=0xDFFF).contains(&code) {
        return None;
    }
    let encoded = if little {
        code.to_le_bytes()
    } else {
        code.to_be_bytes()
    };
    Some(encoded.to_vec())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use beam_common::BeamError;

    use crate::config::RuntimeConfig;
    use crate::context::RuntimeContext;
    use crate::runtime::Runtime;

    use super::*;

    /// Every construction row, as `(expression, the system Erlang oracle's own `~w`
    /// rendering of the value it builds)`.
    ///
    /// The strings are the oracle's stdout for exactly these expressions
    /// (`erl -noshell -eval 'io:format("~w", [<expr>])'`), and
    /// [`the_rows_are_the_system_erlangs`] re-runs them against the live `erl`, so they
    /// cannot rot. `{error,badarg}` is the oracle's rendering of a raise (the batch below
    /// wraps each expression so a failure prints as `{Class,Reason}`).
    const ROWS: &[(&str, &str)] = &[
        // -- little-endian integers (the byte-wise image, including a partial last byte) --
        ("<<-300:12/little>>", "<<212,14:4>>"),
        ("<<16#1234:12/little>>", "<<52,2:4>>"),
        ("<<256:9/little>>", "<<0,1:1>>"),
        ("<<(1 bsl 60):60/little>>", "<<0,0,0,0,0,0,0,0:4>>"),
        ("<<255:12/little>>", "<<255,0:4>>"),
        ("<<(1 bsl 64):72/little>>", "<<0,0,0,0,0,0,0,0,1>>"),
        // … and the byte-aligned forms, which must not regress.
        ("<<16#1234:2/little-unit:8>>", "<<52,18>>"),
        ("<<1:2/unit:8>>", "<<0,1>>"),
        ("<<256:2/unit:8>>", "<<1,0>>"),
        ("<<256:16/little>>", "<<0,1>>"),
        ("<<16#1234:16/little>>", "<<52,18>>"),
        ("<<16#1234:32/little>>", "<<52,18,0,0>>"),
        ("<<-1:12/little>>", "<<255,15:4>>"),
        // A segment that does not fill a whole byte at all, and one starting at a bit offset.
        ("<<16#1234:4/little>>", "<<4:4>>"),
        ("<<16#1234:5/little>>", "<<20:5>>"),
        ("<<1:1, 16#1234:12/little>>", "<<154,2:5>>"),
        // -- float segments (IEEE binary16/32/64; a float *or* an integer value) ----------
        ("<<1.5/float>>", "<<63,248,0,0,0,0,0,0>>"),
        ("<<1.5/float-little>>", "<<0,0,0,0,0,0,248,63>>"),
        ("<<1.5:16/float>>", "<<62,0>>"),
        ("<<1.5:16/float-little>>", "<<0,62>>"),
        ("<<1.5:32/float>>", "<<63,192,0,0>>"),
        ("<<1.5:32/float-little>>", "<<0,0,192,63>>"),
        ("<<1.0e40:64/float>>", "<<72,61,99,41,241,195,92,165>>"),
        ("<<1.0e40:32/float>>", "<<127,128,0,0>>"),
        ("<<0.1:16/float>>", "<<46,102>>"),
        ("<<3.14159:16/float>>", "<<66,72>>"),
        ("<<70000.0:16/float>>", "<<124,0>>"),
        ("<<1.0e10:16/float>>", "<<124,0>>"),
        ("<<5.0e-8:16/float>>", "<<0,1>>"),
        ("<<(1 bsl 64):64/float>>", "<<67,240,0,0,0,0,0,0>>"),
        ("<<1.5:2/float-unit:8>>", "<<62,0>>"),
        (
            "<<1.5/float, 2.5/float>>",
            "<<63,248,0,0,0,0,0,0,64,4,0,0,0,0,0,0>>",
        ),
        ("<<-0.0:16/float>>", "<<128,0>>"),
        ("<<-0.0/float>>", "<<128,0,0,0,0,0,0,0>>"),
        // -- utf16/utf32/utf8 segments ----------------------------------------------------
        ("<<16#41/utf16>>", "<<0,65>>"),
        ("<<16#41/utf16-little>>", "<<65,0>>"),
        ("<<16#41/utf16-native>>", "<<65,0>>"),
        ("<<16#7FF/utf16>>", "<<7,255>>"),
        ("<<16#10FFFF/utf16>>", "<<219,255,223,255>>"),
        ("<<16#10000/utf16-little>>", "<<0,216,0,220>>"),
        ("<<16#10FFFF/utf16-little>>", "<<255,219,255,223>>"),
        ("<<1234/utf32>>", "<<0,0,4,210>>"),
        ("<<1234/utf32-little>>", "<<210,4,0,0>>"),
        ("<<16#10000/utf32>>", "<<0,1,0,0>>"),
        ("<<16#10FFFF/utf32>>", "<<0,16,255,255>>"),
        ("<<16#41/utf8>>", "<<65>>"),
        ("<<16#7FF/utf8>>", "<<223,191>>"),
        ("<<16#10FFFF/utf8>>", "<<244,143,191,191>>"),
        // -- the rows the oracle rejects with `error:badarg` ------------------------------
        ("<<1.5:8/float>>", "{error,badarg}"),
        ("<<1.5:24/float>>", "{error,badarg}"),
        ("<<1.5:0/float>>", "{error,badarg}"),
        ("<<1.5:128/float>>", "{error,badarg}"),
        ("<<foo/float>>", "{error,badarg}"),
        ("<<16#D800/utf16>>", "{error,badarg}"),
        ("<<16#110000/utf16>>", "{error,badarg}"),
        ("<<16#D800/utf32>>", "{error,badarg}"),
        ("<<16#110000/utf32>>", "{error,badarg}"),
        ("<<-1/utf8>>", "{error,badarg}"),
        ("<<16#D800/utf8>>", "{error,badarg}"),
        ("<<16#110000/utf8>>", "{error,badarg}"),
        ("<<atom/utf16>>", "{error,badarg}"),
        ("<<[1040]/utf16>>", "{error,badarg}"),
    ];

    /// A utf segment with an explicit width (`:Size` / `unit:`) — a **syntax** error on the
    /// oracle, so it never builds a value there either.
    const WIDTH_ROWS: &[&str] = &[
        "<<16#41:8/utf8>>",
        "<<16#41:2/utf8>>",
        "<<16#41:16/utf16>>",
        "<<16#41/utf16-unit:8>>",
        "<<16#41/utf32-unit:8>>",
    ];

    /// Evaluates `expr` through the evaluator on `ctx` and renders the outcome exactly as the
    /// oracle's batch below prints it: the `~w` bit-syntax form of the bitstring it built, or
    /// `{error,badarg}` for the rows that raise.
    ///
    /// A bitstring result is heap-allocated, so the payload is copied out immediately; the
    /// registered `~w` printer rejects a non-byte-aligned bitstring (a `beam_bifs` gap), so
    /// the rendering is assembled here from the raw `(bytes, bits)`.
    fn outcome(ctx: &mut RuntimeContext<'_>, expr: &str) -> String {
        match crate::eval::eval(ctx, expr) {
            Ok(value) => {
                let (bytes, offset, bits) = beam_codec::term::bitstring_bytes(value)
                    .unwrap_or_else(|_| panic!("{expr} did not produce a bitstring"));
                assert_eq!(offset, 0, "{expr}: the payload starts at bit 0");
                render_bitstring(bytes, bits)
            }
            Err(error) => {
                assert!(
                    matches!(&error, BeamError::Raise(raise) if raise.reason.as_atom() == Some("badarg")),
                    "{expr}: expected error:badarg, got {error}"
                );
                "{error,badarg}".to_string()
            }
        }
    }

    /// The `~w` bit-syntax rendering of `bits` bits of `bytes`: `<<212,14:4>>`.
    fn render_bitstring(bytes: &[u8], bits: usize) -> String {
        let (full, rem) = (bits / 8, bits % 8);
        let mut parts: Vec<String> = bytes[..full].iter().map(u8::to_string).collect();
        if rem != 0 {
            // The partial byte's meaningful bits are its high `rem` bits (`BitBuf` packs
            // most significant bit first), and `~w` prints their value with the width.
            parts.push(format!("{}:{}", bytes[full] >> (8 - rem), rem));
        }
        format!("<<{}>>", parts.join(","))
    }

    #[test]
    fn the_rows_build_the_oracles_bitstrings() {
        // One runtime for the whole table; `outcome` copies each payload out before the next
        // evaluation.
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        for (expr, expected) in ROWS {
            assert_eq!(&outcome(&mut ctx, expr), expected, "{expr}");
        }
    }

    #[test]
    fn a_utf_segment_with_a_width_is_a_typed_badarg() {
        // An explicit `:Size`/`unit:` on a utf segment is rejected by the oracle's *parser*
        // (`<<16#41:8/utf8>>` does not even parse), so there is no width this runtime could
        // honour: it raises `badarg` rather than fabricating an encoding. The rows are
        // asserted against the live oracle below.
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        for expr in WIDTH_ROWS {
            let error = crate::eval::eval(&mut ctx, expr).expect_err("a width must be rejected");
            assert!(
                matches!(&error, BeamError::Raise(raise) if raise.reason.as_atom() == Some("badarg")),
                "{expr}: expected error:badarg, got {error}"
            );
        }
        drop(ctx);
        let Some(()) = have_oracle() else {
            return;
        };
        for expr in WIDTH_ROWS {
            match oracle_eval(expr) {
                Some((stdout, succeeded)) => assert!(
                    stdout.trim().is_empty() && !succeeded,
                    "{expr}: the oracle built {stdout:?} (success={succeeded})"
                ),
                None => panic!("{expr}: the oracle probe did not finish"),
            }
        }
    }

    // -- the live oracle cross-check -----------------------------------------

    /// The oracle's own stdout for every [`ROWS`] expression, in order, or `None` when the
    /// system `erl` is unavailable (the caller then skips **loudly**).
    ///
    /// One `erl` invocation evaluates the whole table: each row is wrapped in a fun and run
    /// through `try … catch`, so a row that raises prints as `{Class,Reason}` exactly like a
    /// value row, and every row lands on its own line.
    fn oracle_rows() -> Option<String> {
        let mut list = String::new();
        for (expr, _) in ROWS {
            if !list.is_empty() {
                list.push(',');
            }
            list.push_str(&format!("F(fun() -> {expr} end)"));
        }
        let program = format!(
            "F = fun(E) -> try E() catch C:R -> {{C,R}} end end, L = [{list}], \
             [io:format(\"~w~n\", [V]) || V <- L], halt(0)."
        );
        let (stdout, succeeded) = oracle_eval(&program)?;
        assert!(succeeded, "the oracle batch failed: {stdout:?}");
        Some(stdout)
    }

    /// Skips **loudly** when no system `erl` is available, so the live cross-check is never a
    /// silent pass (`None` means "skip").
    fn have_oracle() -> Option<()> {
        match Command::new("erl")
            .args(["-noshell", "-eval", "halt(0)."])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => Some(()),
            Ok(status) => {
                eprintln!("SKIP: `erl` exited with {status} — live oracle cross-check skipped");
                None
            }
            Err(error) => {
                eprintln!("SKIP: no erl on PATH ({error}) — live oracle cross-check skipped");
                None
            }
        }
    }

    /// Runs `erl -noshell -eval <expression>` with a **bounded** poll/kill, returning its
    /// stdout and whether the process succeeded.
    ///
    /// `None` when the system `erl` is unavailable, or when the probe outlived
    /// `ORACLE_TIMEOUT` and was killed (every probe here ends in `halt/0` or a parse error,
    /// so the bound exists only so a hung `erl` cannot wedge the suite).
    fn oracle_eval(expression: &str) -> Option<(String, bool)> {
        const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);
        let mut child = Command::new("erl")
            .args(["-noshell", "-eval", expression])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let deadline = Instant::now() + ORACLE_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    eprintln!("SKIPPING the bits oracle probe: `erl` did not exit within {ORACLE_TIMEOUT:?}");
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                Err(error) => {
                    eprintln!("SKIPPING the bits oracle probe: cannot poll `erl`: {error}");
                    return None;
                }
            }
        };
        let mut text = String::new();
        child.stdout.take()?.read_to_string(&mut text).ok()?;
        Some((text, status.success()))
    }

    #[test]
    fn the_rows_are_the_system_erlangs() {
        let Some(()) = have_oracle() else {
            return;
        };
        let stdout = oracle_rows().expect("the oracle ran");
        // `~w` may wrap a long term across lines, so the comparison ignores layout and cares
        // only about the terms, row for row.
        let expected: Vec<&str> = ROWS.iter().map(|(_, expected)| *expected).collect();
        let stripped = |text: &str| -> String {
            text.chars().filter(|c| !c.is_whitespace()).collect::<String>()
        };
        assert_eq!(
            stripped(&stdout),
            stripped(&expected.join("\n")),
            "the live oracle disagrees with the recorded rows"
        );
    }

    #[test]
    fn a_defaulted_float_segment_is_64_bits() {
        // The scalar row of the objective's table: `:16`/`:32`/`:64` are the widths, and a
        // segment with no size is the full 64-bit field.
        let mut runtime = Runtime::new(RuntimeConfig::default()).expect("a runtime");
        let mut sink = beam_bifs::NullSink;
        let mut ctx = RuntimeContext::with_output(&mut runtime, &mut sink);
        let value = crate::eval::eval(&mut ctx, "bit_size(<<1.0/float>>)")
            .unwrap_or_else(|error| panic!("bit_size(<<1.0/float>>): {error}"));
        assert!(
            beam_term::is_small(value),
            "bit_size(<<1.0/float>>): expected a small integer"
        );
        assert_eq!(beam_term::signed_value(value), 64);
    }

    // -- the pure helpers, on the same oracle values -------------------------

    #[test]
    fn little_endian_images_are_byte_wise() {
        // `-300` within 12 bits is `16#FED4`: the whole byte `16#D4`, then the low 4 bits of
        // `16#FE` — the oracle's `<<212,14:4>>`.
        assert_eq!(
            integer_le_image(&[(!300u64).wrapping_add(1)], 12),
            (vec![0xD4], 0xE, 4)
        );
        assert_eq!(
            integer_le_image(&[0x1234], 12),
            (vec![0x34], 0x2, 4)
        );
        // `<<256:9/little>>` is `<<0,1:1>>`: one whole byte, then the single next bit.
        assert_eq!(integer_le_image(&[256], 9), (vec![0x00], 1, 1));
        // A size below a byte contributes no whole byte at all.
        assert_eq!(integer_le_image(&[0x1234], 4), (vec![], 0x4, 4));
        // A byte-aligned size has no partial field, so the caller ignores the tail.
        assert_eq!(
            integer_le_image(&[0, 1], 72),
            (vec![0, 0, 0, 0, 0, 0, 0, 0, 1], 0, 0)
        );
        assert_eq!(integer_le_image(&[0x1234], 16), (vec![0x34, 0x12], 0, 0));
    }

    #[test]
    fn half_precision_encodes_the_oracles_values() {
        // Every expected field is what the system `erl` prints for the same segment
        // (`<<1.5:16/float>>` is `<<62,0>>` = `16#3E00`, …).
        for (value, expected) in [
            (1.5f64, 0x3E00u16),
            (0.1, 0x2E66),
            (3.14159, 0x4248), // a tie on the mantissa: 584 (even) wins over 585
            (70000.0, 0x7C00),
            (1.0e10, 0x7C00),
            (5.0e-8, 0x0001),
            (-0.0, 0x8000),
            (-1.5, 0xBE00),
            (0.0, 0x0000),
            (1.0, 0x3C00),
            (65504.0, 0x7BFF),
            (65520.0, 0x7C00), // the round-to-infinity threshold
            (6.103515625e-5, 0x0400), // the smallest normal, 2^-14
        ] {
            assert_eq!(f64_to_f16(value), expected, "{value}");
        }
    }

    #[test]
    fn single_precision_agrees_with_the_language_cast() {
        // The implementation rounds by hand, so it is pinned against Rust's own
        // correctly-rounded `f64 as f32` conversion over the interesting boundaries instead
        // of assumed equal.
        for value in [
            0.0f64,
            -0.0,
            1.0,
            1.5,
            -1.5,
            0.1,
            1.0 / 3.0,
            1.0e10,
            1.0e40,
            -1.0e40,
            f64::MAX,
            f64::MIN_POSITIVE,
            1.0e-300,
            3.4028234663852886e38,  // the largest finite binary32
            3.4028235677973366e38,  // just past it: rounds to +inf
            1.1754943508222875e-38, // the smallest normal binary32
            1.401298464324817e-45,  // the smallest subnormal binary32
            7.006492321624085e-46,  // half of it: the tie rounds to the even zero
        ] {
            assert_eq!(
                f64_to_f32_bits(value),
                (value as f32).to_bits(),
                "{value}"
            );
        }
        // The oracle's own fields.
        assert_eq!(f64_to_f32_bits(1.5), 0x3FC0_0000);
        assert_eq!(f64_to_f32_bits(1.0e40), 0x7F80_0000);
    }

    #[test]
    fn limb_magnitudes_convert_and_overflow_to_infinity() {
        assert_eq!(integer_limbs_to_f64(false, &[300]), 300.0);
        assert_eq!(integer_limbs_to_f64(true, &[300]), -300.0);
        // `1 bsl 64` is the two-limb magnitude `[0, 1]` — the oracle's exact `2^64`.
        assert_eq!(integer_limbs_to_f64(false, &[0, 1]), 1.8446744073709552e19);
        assert_eq!(integer_limbs_to_f64(false, &[0, 1]).to_bits(), 0x43F0_0000_0000_0000);
        // A magnitude past the `f64` range overflows to `±inf` rather than saturating.
        let huge = vec![u64::MAX; 32];
        assert_eq!(integer_limbs_to_f64(false, &huge), f64::INFINITY);
        assert_eq!(integer_limbs_to_f64(true, &huge), f64::NEG_INFINITY);
    }

    #[test]
    fn float_fields_carry_their_byte_order() {
        assert_eq!(float_bytes(1.5, 64, false), Some(vec![63, 248, 0, 0, 0, 0, 0, 0]));
        assert_eq!(float_bytes(1.5, 64, true), Some(vec![0, 0, 0, 0, 0, 0, 248, 63]));
        assert_eq!(float_bytes(1.5, 16, false), Some(vec![62, 0]));
        assert_eq!(float_bytes(1.5, 16, true), Some(vec![0, 62]));
        assert_eq!(float_bytes(1.5, 32, false), Some(vec![63, 192, 0, 0]));
        // Every other width is the caller's `badarg`.
        for unsupported in [0usize, 8, 24, 128, 63] {
            assert_eq!(float_bytes(1.5, unsupported, false), None, "{unsupported}");
        }
    }

    #[test]
    fn utf_encoders_reject_what_erlang_rejects() {
        assert_eq!(utf16_bytes(0x41, false), Some(vec![0, 65]));
        assert_eq!(utf16_bytes(0x41, true), Some(vec![65, 0]));
        assert_eq!(utf16_bytes(0x7FF, false), Some(vec![7, 255]));
        assert_eq!(utf16_bytes(0x10FFFF, false), Some(vec![219, 255, 223, 255]));
        // A surrogate pair, little-endian: each unit byte-swapped, unit order kept.
        assert_eq!(utf16_bytes(0x10000, true), Some(vec![0, 216, 0, 220]));
        assert_eq!(utf16_bytes(0x10FFFF, true), Some(vec![255, 219, 255, 223]));
        for rejected in [0xD800i64, 0xDFFF, 0x110000, -1, i64::MIN] {
            assert_eq!(utf16_bytes(rejected, false), None, "{rejected}");
        }
        assert_eq!(utf32_bytes(1234, false), Some(vec![0, 0, 4, 210]));
        assert_eq!(utf32_bytes(1234, true), Some(vec![210, 4, 0, 0]));
        assert_eq!(utf32_bytes(0x10000, false), Some(vec![0, 1, 0, 0]));
        assert_eq!(utf32_bytes(0x10FFFF, false), Some(vec![0, 16, 255, 255]));
        for rejected in [0xD800i64, 0xDFFF, 0x110000, -1, i64::MIN] {
            assert_eq!(utf32_bytes(rejected, false), None, "{rejected}");
        }
    }
}
