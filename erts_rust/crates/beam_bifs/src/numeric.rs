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

//! Shared numeric and float helpers for the `erlang` conversion BIFs.
//!
//! [`crate::convert`] (`integer_to_list/1,2`, `list_to_integer/1,2`,
//! `float_to_list/1,2`, `list_to_float/1`) and [`crate::binconv`]
//! (`integer_to_binary/1,2`, `binary_to_integer/1,2`, `float_to_binary/1,2`,
//! `binary_to_float/1`) render and parse the very same integers and floats, so the
//! renderer/parser pair lives here once instead of in both modules.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/bif.c` (`Sint_to_buf`,
//! `do_float_to_charbuf`), `../../erts/emulator/beam/big.c`
//! (`c2int_is_valid_char`), `../../erts/emulator/sys/common/erl_sys_common_misc.c`
//! and `../../erts/emulator/sys/unix/sys_float.c` (`sys_double_to_chars_fast`,
//! `sys_chars_to_double`).
//!
//! This module is safe Rust: it inspects terms only through [`crate::access`] (and
//! [`beam_codec::term::big_parts`] for a bignum's limbs) and builds no heap terms
//! — the integer-promotion materializer lives in [`crate::arith`], so it needs no
//! `unsafe` block (the crate is `#![deny(unsafe_code)]`, with `crate::access` its
//! only allowed-`unsafe` module).

use beam_common::Result;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};

use crate::access;
use crate::error::badarg;
use crate::print::proper_list;

/// `SYS_DEFAULT_FLOAT_DECIMALS` (`../../erts/emulator/beam/sys.h`): the mantissa
/// precision of the default `float_to_list/1` and `float_to_binary/1` form.
pub(crate) const DEFAULT_FLOAT_DECIMALS: i64 = 20;

/// The `char fbuf[256]` of `do_float_to_charbuf` (`bif.c`): a rendering is
/// `badarg` (the BIF reports `used <= 0`) when it does not fit 255 characters
/// plus the terminating NUL.
const FLOAT_BUF_CHARS: usize = 255;

/// `MAX_FLOAT` in `sys_double_to_chars_fast`: above this magnitude (and for
/// `decimals >= MAX_DECIMALS`) the fixed formatter falls back to `%.*f`.
const FIXED_FAST_MAX: f64 = 9_007_199_254_740_992.0; // 2^53

/// `MAX_DECIMALS` in `sys_double_to_chars_fast`: the number of entries of its
/// `pow10v` table.
const FIXED_FAST_DECIMALS: i64 = 19;

// ---------------------------------------------------------------------------
// Shared argument helpers
// ---------------------------------------------------------------------------

/// The value of a small-integer argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when `term` is not a
/// small integer — a bignum included (the arithmetic operators accept one, the
/// positional/arity arguments here do not).
pub(crate) fn small_arg(term: Eterm) -> Result<i64> {
    if !access::is_small(term) {
        return Err(badarg());
    }
    Ok(signed_value(term))
}

/// The value of a `2..=36` conversion-base argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-integer or an
/// out-of-range base (the `base < 2 || base > 36` test in `bif.c`/`big.c`).
pub(crate) fn base_arg(term: Eterm) -> Result<u32> {
    let base = small_arg(term)?;
    if !(2..=36).contains(&base) {
        return Err(badarg());
    }
    Ok(base as u32)
}

// ---------------------------------------------------------------------------
// Integer rendering/parsing
// ---------------------------------------------------------------------------

/// Renders an exact integer — a sign plus a little-endian `u64` magnitude —
/// in `base` the way `Sint_to_buf` (`bif.c`) and `big_to_charbuf` (`big.c`) do:
/// uppercase digits `A`..`Z` for 10..35 and a leading `-`. Leading zero limbs are
/// dropped and a zero magnitude renders as `"0"`.
pub(crate) fn mag_to_digits(negative: bool, mag: &[u64], base: u32) -> String {
    let mut mag = mag.to_vec();
    trim_mag(&mut mag);
    let radix = u128::from(base);
    let mut digits = Vec::new();
    while !mag.is_empty() {
        let digit = div_mod_small(&mut mag, radix) as u8;
        digits.push(if digit < 10 {
            b'0' + digit
        } else {
            b'A' + (digit - 10)
        });
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    let mut out = String::with_capacity(digits.len() + 1);
    if negative {
        out.push('-');
    }
    for &digit in digits.iter().rev() {
        out.push(char::from(digit));
    }
    out
}

/// Replaces `mag` with `mag / divisor`, dropping the leading zero limbs, and
/// returns `mag % divisor` (`divisor` is a conversion base, `2..=36`).
fn div_mod_small(mag: &mut Vec<u64>, divisor: u128) -> u32 {
    let mut remainder: u128 = 0;
    for limb in mag.iter_mut().rev() {
        let current = (remainder << 64) | u128::from(*limb);
        *limb = (current / divisor) as u64;
        remainder = current % divisor;
    }
    trim_mag(mag);
    remainder as u32
}

/// Drops the leading zero limbs of a little-endian magnitude.
fn trim_mag(mag: &mut Vec<u64>) {
    while mag.last() == Some(&0) {
        mag.pop();
    }
}

/// The digit string for an **integer** term — a small integer or an
/// arbitrary-precision bignum — in `base`. The shared core of
/// `integer_to_list/1,2` and `integer_to_binary/1,2`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a float or any other
/// non-integer (`integer_to_list(1.0)` in the oracle).
pub(crate) fn integer_digits(term: Eterm, base: u32) -> Result<String> {
    if access::is_small(term) {
        let value = signed_value(term);
        return Ok(mag_to_digits(value < 0, &[value.unsigned_abs()], base));
    }
    if access::is_big(term) {
        let (negative, digits) = beam_codec::term::big_parts(term)?;
        return Ok(mag_to_digits(negative, digits, base));
    }
    Err(badarg())
}

/// The value of the digit `byte` in `base`, or `None` when it is not a valid
/// digit (`c2int_is_valid_char`: `0`..`9`, plus `A`..`Z`/`a`..`z` when
/// `base > 10`).
fn digit_value(byte: u8, base: u32) -> Option<u32> {
    if base <= 10 {
        let value = u32::from(byte.wrapping_sub(b'0'));
        return (byte.is_ascii_digit() && value < base).then_some(value);
    }
    match byte {
        b'0'..=b'9' => Some(u32::from(byte - b'0')),
        b'A'..=b'Z' => {
            let value = 10 + u32::from(byte - b'A');
            (value < base).then_some(value)
        }
        b'a'..=b'z' => {
            let value = 10 + u32::from(byte - b'a');
            (value < base).then_some(value)
        }
        _ => None,
    }
}

/// Parses the whole of `text` as an integer in `base`, as
/// `erts_internal:list_to_integer/2` and `binary_to_integer/2` do (an optional
/// `+`/`-`, then digits), returning the sign and the little-endian `u64` magnitude
/// of an **arbitrary-precision** result. The caller materializes it with
/// [`crate::arith::int_from_mag`], which promotes to a bignum past the 60-bit
/// small range (the `big_from_charbuf` fold-in the oracle uses).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a bad character or a
/// missing digit (`list_to_integer("")`, `("1a")`). A magnitude that exceeds
/// [`beam_codec::term::make_big`]'s `BIG_ARITY_MAX` limb bound is reported by the
/// materializer as `system_limit`, exactly as the oracle reports
/// `list_to_integer` of a >1.26M-digit string.
pub(crate) fn parse_integer(text: &str, base: u32) -> Result<(bool, Vec<u64>)> {
    let bytes = text.as_bytes();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return Err(badarg());
    }
    let radix = u128::from(base);
    let mut magnitude: Vec<u64> = Vec::new();
    for &byte in digits {
        let digit = digit_value(byte, base).ok_or_else(badarg)?;
        // `magnitude = magnitude * base + digit`, one limb at a time.
        let mut carry = u128::from(digit);
        for limb in magnitude.iter_mut() {
            let value = u128::from(*limb) * radix + carry;
            *limb = value as u64;
            carry = value >> 64;
        }
        if carry != 0 {
            magnitude.push(carry as u64);
        }
    }
    trim_mag(&mut magnitude);
    Ok((negative, magnitude))
}

// ---------------------------------------------------------------------------
// Float rendering
// ---------------------------------------------------------------------------

/// The `fmt_type_` of `do_float_to_charbuf` — the rendering strategy selected by
/// a `float_to_list/2` / `float_to_binary/2` option list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FloatFormat {
    /// No `{decimals,_}`/`{scientific,_}`/`short` option: `%.*e` with
    /// `SYS_DEFAULT_FLOAT_DECIMALS`.
    Legacy,
    /// `{decimals, N}`: `%.*f`.
    Fixed,
    /// `{scientific, N}`: `%.*e`.
    Scientific,
    /// `short`: the shortest round-trip rendering (see [`format_short`]).
    Short,
}

/// Parses `float_to_list/2`'s / `float_to_binary/2`'s option list, left to right
/// (a later option wins, exactly as `do_float_to_charbuf` overwrites
/// `decimals`/`fmt_type`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list, an
/// improper list or an option outside `compact`/`short`/`{decimals,N}`/
/// `{scientific,N}` with a small `N`.
pub(crate) fn parse_float_options(
    atoms: &AtomTable,
    opts: Eterm,
) -> Result<(FloatFormat, i64, bool)> {
    let items = proper_list(opts).ok_or_else(badarg)?;
    let mut format = FloatFormat::Legacy;
    let mut decimals = DEFAULT_FLOAT_DECIMALS;
    let mut compact = false;
    for item in items {
        if access::is_atom(item) {
            match atoms.name(atom_val(item)) {
                Some("compact") => compact = true,
                Some("short") => format = FloatFormat::Short,
                _ => return Err(badarg()),
            }
            continue;
        }
        if access::is_tuple(item) && access::tuple_size(item) == 2 {
            let name = access::tuple_element(item, 0);
            let value = access::tuple_element(item, 1);
            if access::is_atom(name) && access::is_small(value) {
                match atoms.name(atom_val(name)) {
                    Some("decimals") => {
                        decimals = signed_value(value);
                        format = FloatFormat::Fixed;
                        continue;
                    }
                    Some("scientific") => {
                        decimals = signed_value(value);
                        format = FloatFormat::Scientific;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        return Err(badarg());
    }
    Ok((format, decimals, compact))
}

/// Renders `value` as `%.*e` with `decimals` mantissa digits, then rewrites the
/// exponent the way C does (`e+00`, never Rust's `e0`).
///
/// A negative precision is "omitted" for `printf`, so it selects the default of
/// 6 — `float_to_list(1.5, [{scientific,-1}])` (and the `float_to_binary`
/// equivalent) is `"1.500000e+00"` in the oracle too.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the rendering does
/// not fit the oracle's 256-byte buffer.
pub(crate) fn format_scientific(value: f64, decimals: i64) -> Result<String> {
    let precision = if decimals < 0 { 6 } else { decimals };
    // The shortest possible rendering is `<d>.<N digits>e+00`, so `N > 249`
    // cannot fit; the check keeps the formatting below from allocating wildly.
    if precision > (FLOAT_BUF_CHARS - 6) as i64 {
        return Err(badarg());
    }
    let rendered = format!("{:.1$e}", value, precision as usize);
    let (mantissa, exponent) = rendered.split_once('e').ok_or_else(badarg)?;
    let exponent: i32 = exponent.parse().map_err(|_| badarg())?;
    let sign = if exponent < 0 { '-' } else { '+' };
    let out = format!("{mantissa}e{sign}{:02}", exponent.unsigned_abs());
    if out.len() > FLOAT_BUF_CHARS {
        return Err(badarg());
    }
    Ok(out)
}

/// Trims a fixed rendering the way `find_first_trailing_zero` does: trailing
/// zeros go, but the digit right after the radix point stays.
fn trim_trailing_zeros(rendered: &mut String) {
    while rendered.ends_with('0') {
        rendered.pop();
    }
    if rendered.ends_with('.') {
        rendered.push('0');
    }
}

/// Renders `value` as `%.*f`, reproducing `sys_double_to_chars_fast`'s fast
/// path: for `|value| =< 2^53` and `decimals < 19` the fraction is scaled and
/// rounded *half away from zero*, which is why `float_to_list(0.15,
/// [{decimals,1}])` (and the `float_to_binary` equivalent) is `"0.2"` (a plain
/// correctly-rounded `%.1f` would say `"0.1"`); larger magnitudes and precisions
/// fall back to `%.*f`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a negative
/// precision (the C formatter reports `-1`) or a rendering beyond the buffer.
fn format_fixed(value: f64, decimals: i64, compact: bool) -> Result<String> {
    if decimals < 0 {
        return Err(badarg());
    }
    let precision = decimals as usize;
    // The shortest possible rendering is `0.<N digits>`, so `N > 253` cannot fit.
    if precision > FLOAT_BUF_CHARS - 2 {
        return Err(badarg());
    }
    let has_decimals = precision != 0;
    let negative = value.is_sign_negative();
    let magnitude = value.abs();

    let mut out = String::with_capacity(precision + 24);
    if negative {
        out.push('-');
    }
    // The C's "Don't bother with optimizing too large numbers or too large
    // precision" branch (`erl_sys_common_misc.c:196`): a magnitude above `2^53` or
    // a precision at/above 19 falls back to a plain `%.*f`.
    let slow = magnitude > FIXED_FAST_MAX || decimals >= FIXED_FAST_DECIMALS;
    if slow {
        out.push_str(&format!("{magnitude:.precision$}"));
    } else if has_decimals {
        let scale = 10f64.powi(precision as i32);
        let integer_f = magnitude.floor();
        let fraction_f = ((magnitude - integer_f) * scale).round();
        let mut integer = integer_f as u64;
        let mut fraction = fraction_f as u64;
        let limit = 10u64.pow(precision as u32);
        if fraction >= limit {
            // The rounded fraction carried into the integer part.
            integer += 1;
            fraction = 0;
        }
        out.push_str(&integer.to_string());
        out.push('.');
        out.push_str(&format!("{fraction:0width$}", width = precision));
    } else {
        out.push_str(&(magnitude.round() as u64).to_string());
    }
    // The buffer guard runs on the **untrimmed** rendering (`erl_sys_common_misc.c:199`),
    // so a rendering the oracle's `fbuf[256]` cannot hold is `badarg` even when
    // `compact` would shorten it (`float_to_list(1.7976931348623157e308,
    // [{decimals,2},compact])`).
    if out.len() > FLOAT_BUF_CHARS {
        return Err(badarg());
    }
    // The slow branch drops trailing zeros whenever `compact` is set
    // (`erl_sys_common_misc.c:202-204`); the fast branch only does so when the
    // rendering has a fractional part (`:263`). So `{decimals,0},compact` of a
    // magnitude above `2^53` collapses to its significant digits
    // (`float_to_list(1.0e16,…)` ⇒ `"1"`, `9.1e15` ⇒ `"91"`) while the same option
    // below `2^53` keeps them (`float_to_list(5.0e15,…)` ⇒ `"5000000000000000"`).
    if compact && (has_decimals || slow) {
        trim_trailing_zeros(&mut out);
    }
    Ok(out)
}

/// Renders `value` for the `short` option the way `sys_double_to_chars_short`
/// (`../../erts/emulator/sys/common/erl_float_short.cpp`) does: the shortest
/// decimal that round-trips, written as `std::to_chars` writes it and then
/// rewritten by hand into the oracle's form.
///
/// This is also the **plain float leaf of the printer**: `io_lib:write1/4`'s
/// float clause is `io_lib_format:fwrite_g(Float)` and `fwrite_g/1` is exactly
/// `float_to_list(Float, [short])` (`lib/stdlib/src/io_lib_format.erl:758-760`),
/// so `~w`/`~p`/`~W`/`~P` render every float through here too
/// ([`crate::print`]). The domain is finite floats: OTP has no representable
/// `inf`/`NaN` at all (a `NEW_FLOAT_EXT` image with exponent `16#7FF` is
/// rejected by `binary_to_term/1`), so there is no oracle behaviour to match
/// beyond it — see `crate::print::format_float`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a rendering the
/// oracle's buffer cannot hold (`do_float_to_charbuf` converts `to_chars`'s zero
/// return into `badarg`). An `f64` never comes close to 255 characters, so this
/// mirrors the C's guard rather than a reachable limit.
pub(crate) fn format_short(value: f64) -> Result<String> {
    let out = shorter(to_chars_general(value).into_bytes());
    if out.len() > FLOAT_BUF_CHARS {
        return Err(badarg());
    }
    Ok(out)
}

/// The rendering `std::to_chars(first, last, value)` (the shortest round-trip
/// overload) produces: `f` or `e`, whichever is **shorter**, with the sign, no
/// trailing `.0`, a `+` on a positive exponent and at least two exponent digits
/// (`erl_float_short.cpp:45-51`).
///
/// Rust's `{:e}` already yields the shortest round-trip digits, so only the
/// notation choice and the digit placement are added. Zero has no digits and is
/// `0` / `-0`.
fn to_chars_general(value: f64) -> String {
    let sign = if value.is_sign_negative() { "-" } else { "" };
    if value == 0.0 {
        return format!("{sign}0");
    }
    let (digits, exponent) = shortest_digits(value.abs());
    let fixed = digits_fixed(&digits, exponent);
    let scientific = digits_scientific(&digits, exponent);
    // `erl_float_short.cpp:45-51` forces the scientific form at and above 2^53
    // (the same bound `sys_double_to_chars_fast` calls `MAX_FLOAT`), because
    // `std::to_chars`'s fixed form would then be exact only by accident.
    if value.abs() >= FIXED_FAST_MAX || scientific.len() < fixed.len() {
        format!("{sign}{scientific}")
    } else {
        format!("{sign}{fixed}")
    }
}

/// The shortest round-trip digits of a finite non-zero float and the decimal
/// exponent of their leading digit (`0.1` ⇒ `("1", -1)`, `1200.0` ⇒ `("12", 3)`).
///
/// Rust's `{:e}` already yields the shortest round-trip digits, so the digit
/// *count* is right and only an exact tie can differ: it rounds such a tie
/// *up*, while the oracle (Ryu) keeps the even candidate. A tie is therefore
/// re-resolved here from the two candidates themselves.
fn shortest_digits(value: f64) -> (String, i32) {
    // `{:e}` renders `d[.ddd]e<exp>` with the shortest round-trip digits.
    let rendered = format!("{value:e}");
    let (mantissa, exponent) = rendered
        .split_once('e')
        .expect("`{:e}` renders an exponent");
    let digits: String = mantissa.chars().filter(|ch| *ch != '.').collect();
    let exponent = exponent.parse().expect("`{:e}` renders an integer exponent");
    let count = digits.len();
    // No tie: `{:e}` rounded to the nearest of the two candidates, and so does
    // the oracle.
    let Some(lower_is_odd) = tie_parity(value, count as i32, exponent) else {
        return (digits, exponent);
    };
    // The candidates are the two `count`-digit decimals around `value`, and
    // `{:e}`'s digits are one of them: the lower one when it rounded down (the
    // two then share a parity), the upper one when it rounded up.
    let (lower, upper) = if lower_is_odd == !last_digit_is_even(&digits) {
        (digits.clone(), neighbour(&digits, true))
    } else {
        (neighbour(&digits, false), digits.clone())
    };
    // The oracle writes the even candidate — but only while it parses back to
    // `value`; below a power of two the lower one is outside the round-trip
    // interval, and the upper one is then the only choice.
    let (even, other) = if lower_is_odd {
        (upper, lower)
    } else {
        (lower, upper)
    };
    if round_trips(value, &even, count, exponent) {
        (even, exponent)
    } else {
        (other, exponent)
    }
}

/// The parity of the lower of the two `count`-digit decimals around `value`,
/// when `value` sits exactly halfway between them — `None` when it does not,
/// i.e. when there is no tie.
///
/// `value` is the dyadic rational `significand / 2^scale`. A tie means
/// `2 * value * 10^(count - 1 - exponent)` is an odd integer, which fixes the
/// binary scale exactly (`trailing_zeros(significand) + shift == scale - 1`),
/// and it makes the lower decimal `(odd * 5^shift - 1) / 2` with `odd` the odd
/// part of `significand`. Since `5^shift % 4 == 1`, that lower decimal is odd
/// exactly when `odd % 4 == 3`.
fn tie_parity(value: f64, count: i32, exponent: i32) -> Option<bool> {
    let bits = value.to_bits();
    let biased = ((bits >> 52) & 0x7ff) as i32;
    let fraction = bits & 0x000f_ffff_ffff_ffff;
    let (significand, scale) = if biased == 0 {
        (fraction, 1074)
    } else {
        ((1 << 52) | fraction, 1075 - biased)
    };
    if scale <= 0 {
        // An integral value has no fractional midpoint.
        return None;
    }
    let shift = count - 1 - exponent;
    if shift < 0 || significand.trailing_zeros() as i32 + shift != scale - 1 {
        return None;
    }
    let odd = significand >> significand.trailing_zeros();
    Some(odd % 4 == 3)
}

/// The `count`-digit decimal next to `digits` — its integer value `- 1`
/// (`up == false`) or `+ 1`, with the borrow/carry propagated. A borrow out of a
/// lone leading `1` (`100` ⇒ `099`) and a carry out of `9`s (`999` ⇒ `1000`)
/// both stay the same decimal value under [`round_trips`], which parses the
/// digits as a plain integer.
fn neighbour(digits: &str, up: bool) -> String {
    let mut bytes = digits.as_bytes().to_vec();
    for at in (0..bytes.len()).rev() {
        if up {
            if bytes[at] < b'9' {
                bytes[at] += 1;
                return ascii(&bytes);
            }
            bytes[at] = b'0';
        } else if bytes[at] > b'0' {
            bytes[at] -= 1;
            return ascii(&bytes);
        } else {
            bytes[at] = b'9';
        }
    }
    if !up {
        // Unreachable: a shortest-digit form never starts with `0`, so the
        // borrow stops inside the string.
        return ascii(&bytes);
    }
    let mut out = String::from("1");
    out.push_str(&ascii(&bytes));
    out
}

/// Whether the candidate `digits` of `count` digits (with the exponent of its
/// leading digit) parses back to `value` — the oracle only writes a decimal
/// inside `value`'s round-trip interval, and `f64::from_str` is correctly
/// rounded, so it decides that membership exactly.
fn round_trips(value: f64, digits: &str, count: usize, exponent: i32) -> bool {
    let text = format!("{digits}e{}", i64::from(exponent) - (count as i64 - 1));
    text.parse::<f64>() == Ok(value)
}

/// Whether the last decimal digit of `digits` is even.
fn last_digit_is_even(digits: &str) -> bool {
    matches!(digits.as_bytes().last(), Some(b'0' | b'2' | b'4' | b'6' | b'8'))
}

/// The `f` notation of `digits`/`exponent` (no sign, no trailing `.0`).
fn digits_fixed(digits: &str, exponent: i32) -> String {
    let count = digits.len() as i32;
    let mut out = String::with_capacity(digits.len() + exponent.unsigned_abs() as usize + 2);
    if exponent >= count - 1 {
        out.push_str(digits);
        for _ in 0..(exponent + 1 - count) {
            out.push('0');
        }
    } else if exponent >= 0 {
        let (head, tail) = digits.split_at((exponent + 1) as usize);
        out.push_str(head);
        out.push('.');
        out.push_str(tail);
    } else {
        out.push_str("0.");
        for _ in 0..(-exponent - 1) {
            out.push('0');
        }
        out.push_str(digits);
    }
    out
}

/// The `e` notation of `digits`/`exponent` (no sign, `+` kept, at least two
/// exponent digits).
fn digits_scientific(digits: &str, exponent: i32) -> String {
    let mut out = String::with_capacity(digits.len() + 6);
    out.push_str(&digits[..1]);
    if digits.len() > 1 {
        out.push('.');
        out.push_str(&digits[1..]);
    }
    out.push('e');
    if exponent < 0 {
        out.push('-');
    } else {
        out.push('+');
    }
    let magnitude = exponent.unsigned_abs().to_string();
    for _ in magnitude.len()..2 {
        out.push('0');
    }
    out.push_str(&magnitude);
    out
}

/// A faithful port of `sys_double_to_chars_short`'s string post-processing
/// (`../../erts/emulator/sys/common/erl_float_short.cpp:76-189`): it turns a
/// `std::to_chars` rendering into the `short` form — `1` ⇒ `1.0`, `1e+04` ⇒
/// `1.0e4`, `1e-04` ⇒ `0.0001`, `1.2e+05` ⇒ `1.2e5`, `1000` ⇒ `1.0e3` — and it
/// may flip between the fixed and scientific notations because the two differ in
/// length once the corrections are applied.
fn shorter(mut buf: Vec<u8>) -> String {
    let start = usize::from(buf.first() == Some(&b'-'));
    let mut end = buf.len();

    if end - start <= 4 {
        return fixed_form(&buf, start, end);
    }

    // `std::to_chars` writes the exponent, if any, as the last four or five
    // characters (`e+05` / `e+308`), so that is where the `e` is looked for.
    let mut science = end - 4;
    if buf[science] != b'e' {
        science -= 1;
        if buf[science] != b'e' {
            return fixed_form(&buf, start, end);
        }
    }

    // Scientific form.
    if science == start + 1 {
        if &buf[science + 1..science + 4] == b"-04" {
            // `Xe-04` ⇒ `0.000X` — the one exponent the oracle spells out in
            // fixed notation.
            let digit = buf[start];
            buf.resize(start + 6, b'0');
            buf[start..start + 5].copy_from_slice(b"0.000");
            buf[start + 5] = digit;
            end = start + 6;
            return ascii(&buf[..end]);
        }
        // A one-digit significand gains the `.0` the oracle requires.
        buf.splice(science..science, [b'.', b'0']);
        science += 2;
        end += 2;
    }

    // `X.XXe+0Y` ⇒ `X.XXeY`: drop the `+`, and a leading zero of the exponent.
    let sign = science + 1;
    let mut at = sign + 1;
    if buf[at] == b'0' {
        at += 1;
    }
    let from = if buf[sign] == b'-' { sign + 1 } else { sign };
    if at > from {
        buf.drain(from..at);
        end -= at - from;
    }
    ascii(&buf[..end])
}

/// The fixed-notation half of `sys_double_to_chars_short`'s post-processing
/// (including the `goto fixed` taken when the rendering has no short exponent).
/// Every index below is inside `buf`, which makes the C's `ASSERT`s into the
/// guards kept here.
fn fixed_form(buf: &[u8], start: usize, end: usize) -> String {
    let len = end - start;
    let trailing_zeros = len > 3
        && buf[end - 1] == b'0'
        && buf[end - 2] == b'0'
        && (len < 11 || buf[end - 3] == b'0');
    if trailing_zeros {
        // A trailing-zero run becomes scientific: `1000` ⇒ `1.0e3`,
        // `12300` ⇒ `1.23e4`, `12345670000` ⇒ `1.234567e10`.
        let exponent = len - 1;
        let mut last = end - 3;
        while buf[last] == b'0' {
            last -= 1;
        }
        // `if (end == p) end++` in the C: keep one zero as the decimal.
        let tail = if last == start { 1 } else { last - start };
        let mut out = ascii(&buf[..start]);
        out.push(char::from(buf[start]));
        out.push('.');
        out.push_str(&ascii(&buf[start + 1..start + 1 + tail]));
        out.push('e');
        debug_assert!(exponent < 100, "an `f64` integer form has < 100 digits");
        out.push_str(&exponent.to_string());
        out
    } else if len >= 7 && buf[start..start + 5] == *b"0.000" {
        // `0.000XXX` ⇒ `X.XXe-4` (the C asserts `end - p >= 7` and
        // `p[5] != '0'`; the shortest-digit forms that reach here satisfy both).
        let mut out = Vec::from(&buf[start..end]);
        let digit = out[5];
        out[0] = digit;
        out[1] = b'.';
        out.copy_within(6.., 2);
        out.truncate(len - 1);
        let at = out.len() - 3;
        out[at..].copy_from_slice(b"e-4");
        let mut text = ascii(&buf[..start]);
        text.push_str(&ascii(&out));
        text
    } else if !buf[start..end].contains(&b'.') {
        // An integer gains the `.0` the oracle requires.
        let mut out = ascii(&buf[..end]);
        out.push_str(".0");
        out
    } else {
        ascii(&buf[..end])
    }
}

/// The text of a `std::to_chars` byte buffer — always ASCII.
fn ascii(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| char::from(*byte)).collect()
}

/// Renders `value` for `float_to_list/2` / `float_to_binary/2`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a rendering the
/// oracle's buffer cannot hold.
pub(crate) fn render_float(
    value: f64,
    format: FloatFormat,
    decimals: i64,
    compact: bool,
) -> Result<String> {
    match format {
        FloatFormat::Legacy | FloatFormat::Scientific => format_scientific(value, decimals),
        FloatFormat::Fixed => format_fixed(value, decimals, compact),
        FloatFormat::Short => format_short(value),
    }
}

// ---------------------------------------------------------------------------
// Float parsing
// ---------------------------------------------------------------------------

/// Parses a float the way `sys_chars_to_double` does: an optional sign, digits,
/// a `.`/`,` radix, digits, an optional `e`/`E` exponent — and nothing else. The
/// value must be finite (`"1.0e309"` is `badarg`, an underflow to `0.0` is not).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for anything the
/// grammar rejects, a non-finite value, or (unreachable after the grammar check)
/// a rejected `f64` parse.
pub(crate) fn parse_float(text: &str) -> Result<f64> {
    let bytes = text.as_bytes();
    let mut at = 0;
    if matches!(bytes.first(), Some(b'+') | Some(b'-')) {
        at += 1;
    }
    let integer_start = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    if at == integer_start || at >= bytes.len() || !matches!(bytes[at], b'.' | b',') {
        return Err(badarg());
    }
    at += 1;
    let fraction_start = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    if at == fraction_start {
        return Err(badarg());
    }
    if at < bytes.len() && matches!(bytes[at], b'e' | b'E') {
        at += 1;
        if at < bytes.len() && matches!(bytes[at], b'+' | b'-') {
            at += 1;
        }
        let exponent_start = at;
        while at < bytes.len() && bytes[at].is_ascii_digit() {
            at += 1;
        }
        if at == exponent_start {
            return Err(badarg());
        }
    }
    if at != bytes.len() {
        return Err(badarg());
    }
    let normalized = text.replace(',', ".");
    let value: f64 = normalized.parse().map_err(|_| badarg())?;
    if !value.is_finite() {
        return Err(badarg());
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::{render_float, FloatFormat};

    /// The `short` rendering of `value` — the `short` arm of [`render_float`],
    /// which ignores the `decimals`/`compact` arguments of the other forms.
    fn short(value: f64) -> String {
        render_float(value, FloatFormat::Short, 20, false).expect("`short` always renders")
    }

    /// The `{decimals, N}` fixed rendering of `value` — the `Fixed` arm of
    /// [`render_float`], with `compact` selecting the trailing-zero trim.
    fn fixed(value: f64, decimals: i64, compact: bool) -> String {
        render_float(value, FloatFormat::Fixed, decimals, compact)
            .expect("expected a rendering")
    }
    /// `erlang:float_to_list(F, [short])` on the system oracle (OTP 29.1.1 /
    /// erts-17.1), each value as its IEEE-754 bit pattern. The table pins every
    /// observable property of the form: the shortest round-trip digits, the
    /// `f`/`e` notation switch, the mandatory `.0` of an integral significand,
    /// the exponent spelling (`e10`, `e-5` — no `+`, no leading zero), the sign
    /// of a negative zero, the 2^53 forced-scientific bound, and the exact-tie
    /// rounding of `shortest_digits`.
    const GOLDEN: &[(u64, &str)] = &[
        (0x3ff0_0000_0000_0000, "1.0"),
        (0xbff0_0000_0000_0000, "-1.0"),
        (0x0000_0000_0000_0000, "0.0"),
        (0x8000_0000_0000_0000, "-0.0"),
        (0x4202_a05f_2000_0000, "1.0e10"),
        (0x430c_6bf5_2634_0000, "1.0e15"),
        (0x4341_c379_37e0_8000, "1.0e16"),
        (0x4376_3457_85d8_a000, "1.0e17"),
        (0x3f1a_36e2_eb1c_432d, "0.0001"),
        (0x3ee4_f8b5_88e3_68f1, "1.0e-5"),
        (0x3cd2_03af_9ee7_5616, "1.0e-15"),
        (0x3c9c_d2b2_97d8_89bc, "1.0e-16"),
        (0x3fb9_9999_9999_999a, "0.1"),
        (0x3f1f_7510_4d55_1d69, "1.2e-4"),
        (0x408f_4000_0000_0000, "1.0e3"),
        (0x40f8_6a00_0000_0000, "1.0e5"),
        (0x412e_8480_0000_0000, "1.0e6"),
        (0x4092_c000_0000_0000, "1.2e3"),
        (0x40c8_0600_0000_0000, "1.23e4"),
        (0x419d_6f34_5400_0000, "123456789.0"),
        (0x4132_d687_0000_0000, "1234567.0"),
        (0x4132_d680_0000_0000, "1234560.0"),
        (0x4167_8c29_c000_0000, "12345678.0"),
        (0x4059_0000_0000_0000, "100.0"),
        (0x3fd5_5555_5555_5555, "0.3333333333333333"),
        (0x3fe5_5555_5555_5555, "0.6666666666666666"),
        (0x3ff8_0000_0000_0000, "1.5"),
        (0x4004_0000_0000_0000, "2.5"),
        (0x3fe0_0000_0000_0000, "0.5"),
        (0x4009_21fb_5444_2d18, "3.141592653589793"),
        (0x7fef_ffff_ffff_ffff, "1.7976931348623157e308"),
        (0xffef_ffff_ffff_ffff, "-1.7976931348623157e308"),
        (0x0000_0000_0000_0001, "5.0e-324"),
        (0x0010_0000_0000_0000, "2.2250738585072014e-308"),
        (0x433f_ffff_ffff_ffff, "9007199254740991.0"),
        (0x4340_0000_0000_0000, "9.007199254740992e15"),
        (0x4340_0000_0000_0001, "9.007199254740994e15"),
        (0x42ed_23cf_7588_0f84, "256317133176956.12"),
        (0x4319_b79a_51d1_1385, "1809686960817377.2"),
        (0x4301_d5e5_8fae_0be6, "627532067357052.8"),
        (0xc31e_a6ee_a9b7_c403, "-2156948320284928.8"),
        (0xc234_5815_9bde_fc00, "-87377157086.98438"),
        (0x3e70_0000_0000_0000, "5.960464477539063e-8"),
        (0xbe70_0000_0000_0000, "-5.960464477539063e-8"),
        (0x40f0_0000_0000_0000, "65536.0"),
        (0x40f0_0008_0000_0000, "65536.5"),
        (0x4415_af1d_78b5_8c40, "1.0e20"),
        (0x444b_1ae4_d6e2_ef50, "1.0e21"),
        (0x4480_f0cf_064d_d592, "1.0e22"),
        (0x44b5_2d02_c7e1_4af6, "1.0e23"),
        (0x54b2_49ad_2594_c37d, "1.0e100"),
    ];

    #[test]
    fn short_matches_the_oracle_golden_table() {
        for &(bits, expected) in GOLDEN {
            let value = f64::from_bits(bits);
            assert_eq!(short(value), expected, "{value:?}");
        }
    }

    #[test]
    fn short_is_the_shortest_round_trip_form() {
        for &(bits, expected) in GOLDEN {
            let value = f64::from_bits(bits);
            assert_eq!(
                expected.parse::<f64>().expect("a decimal"),
                value,
                "{expected} must parse back to {value:?}"
            );
        }
    }

    /// The verification the objective names: `float_to_list(1.0e10, [short])`
    /// is exactly `"1.0e10"` — the notation is the *shorter* one after the
    /// oracle's `.0` corrections, and the exponent carries neither a `+` nor a
    /// leading zero.
    #[test]
    fn short_notation_and_exponent_spelling_match_the_oracle() {
        assert_eq!(short(1.0e10), "1.0e10");
        for value in [
            1.0e3, 1.0e5, 1.0e10, 1.0e15, 1.0e-5, 1.0e-15, 1.0e100, 5.0e-324,
        ] {
            let rendered = short(value);
            assert!(!rendered.contains('+'), "{rendered}");
            if let Some((_, exponent)) = rendered.split_once('e') {
                assert!(
                    !exponent.starts_with('0') && !exponent.starts_with("-0"),
                    "{rendered}"
                );
            }
        }
        // The switch is on length, not on `%g`'s `-4..=precision` rule: `1.0e3`
        // beats `1000`, but `1234567.0` stays fixed.
        assert_eq!(short(1000.0), "1.0e3");
        assert_eq!(short(1200.0), "1.2e3");
        assert_eq!(short(12300.0), "1.23e4");
        assert_eq!(short(123456.0), "123456.0");
        assert_eq!(short(100000.0), "1.0e5");
        assert_eq!(short(1234560.0), "1234560.0");
        assert_eq!(short(1.0e-4), "0.0001");
        assert_eq!(short(0.00012), "1.2e-4");
        // An integral significand always keeps the decimal point.
        assert_eq!(short(1.0), "1.0");
        assert_eq!(short(0.0), "0.0");
        assert_eq!(short(-0.0), "-0.0");
    }

    /// An exact decimal midpoint is resolved to the *even* candidate (`…125` ⇒
    /// `…12`, where Rust's `{:e}` rounds up to `…13`) — but only while that
    /// candidate is inside the round-trip interval. Just below a power of two the
    /// lower candidate is not, so `2^-24` keeps the upper `…063`.
    #[test]
    fn short_resolves_exact_midpoints_like_the_oracle() {
        assert_eq!(short(256317133176956.125), "256317133176956.12");
        assert_eq!(short(627532067357052.75), "627532067357052.8");
        assert_eq!(short(-2156948320284928.75), "-2156948320284928.8");
        assert_eq!(short(1809686960817377.25), "1809686960817377.2");
        assert_eq!(short(2f64.powi(-24)), "5.960464477539063e-8");
        assert_eq!(short(-2f64.powi(-24)), "-5.960464477539063e-8");
    }

    /// At and above `2^53` the fixed form is forced to the scientific one (the
    /// `MAX_FLOAT` bound `sys_double_to_chars_fast` shares).
    #[test]
    fn short_forces_scientific_at_two_pow_53() {
        assert_eq!(short(9_007_199_254_740_991.0), "9007199254740991.0");
        assert_eq!(short(9_007_199_254_740_992.0), "9.007199254740992e15");
        assert_eq!(short(9_007_199_254_740_994.0), "9.007199254740994e15");
    }

    /// `erlang:float_to_list(F, [{decimals,0},compact])` on the system oracle
    /// (OTP 29.1.1 / erts-17.1). The C's `compact` trim is *unconditional* in the
    /// slow branch (`erl_sys_common_misc.c:202-204`), so a magnitude above `2^53`
    /// collapses to its significant digits; the fast branch (`:262-263`) trims
    /// only when the rendering has a fractional part, so a magnitude at or below
    /// `2^53` keeps its integer trailing zeros.
    #[test]
    fn fixed_decimals_zero_compact_matches_the_oracle() {
        // Slow branch (|v| > 2^53): trailing zeros are dropped.
        for (value, expected) in [
            (1.0e20, "1"),
            (1.0e19, "1"),
            (1.0e17, "1"),
            (1.0e16, "1"),
            (9.1e15, "91"),
            (9.999e15, "9999"),
            (2.0e20, "2"),
            (1.5e20, "15"),
            (1.234e20, "1234"),
            (5.0e16, "5"),
            (-1.0e20, "-1"),
        ] {
            assert_eq!(fixed(value, 0, true), expected, "float_to_list({value}, …)");
        }
        // Fast branch (|v| <= 2^53): the zeros are kept.
        for (value, expected) in [
            (1.0e15, "1000000000000000"),
            (7.0e15, "7000000000000000"),
            (5.0e15, "5000000000000000"),
        ] {
            assert_eq!(fixed(value, 0, true), expected, "float_to_list({value}, …)");
        }
        // Non-`compact` and `{decimals, N>0}` are unchanged.
        assert_eq!(fixed(1.0e16, 0, false), "10000000000000000");
        assert_eq!(fixed(1.0e20, 2, true), "100000000000000000000.0");
        assert_eq!(fixed(9.1e15, 1, true), "9100000000000000.0");
        assert_eq!(fixed(1.0, 2, true), "1.0");
        assert_eq!(fixed(1.0, 19, true), "1.0");
    }

    /// The `fbuf[256]` guard runs on the **untrimmed** rendering
    /// (`erl_sys_common_misc.c:199`), so a magnitude whose `%.*f` text overflows
    /// the buffer is `badarg` even when `compact` would shorten the result —
    /// `1.7976931348623157e308` trims to 17 characters but is still rejected.
    #[test]
    fn fixed_buffer_overflow_is_badarg_even_when_compact_would_shorten_it() {
        for decimals in [0, 2] {
            assert!(
                render_float(f64::MAX, FloatFormat::Fixed, decimals, true).is_err(),
                "decimals={decimals}"
            );
        }
    }

    /// `$ERTS_ORACLE` if it names an existing file, else `erl` on `$PATH` — the
    /// same resolution `src/ets/specs.rs` and `tests/checksum.rs` use.
    fn resolve_oracle() -> Option<std::path::PathBuf> {
        if let Some(value) = std::env::var_os("ERTS_ORACLE") {
            let program = std::path::PathBuf::from(&value);
            if program.is_file() {
                return Some(program);
            }
            eprintln!(
                "[beam_bifs numeric] $ERTS_ORACLE='{}' is not an existing file — \
                 treating it as no oracle.",
                program.display()
            );
            return None;
        }
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join("erl"))
            .find(|candidate| candidate.is_file())
    }

    /// The `float_to_list/2` sweep the live-oracle cross-check runs:
    /// `(Erlang float literal, Erlang option list, form, decimals, compact)`.
    /// The `{decimals,0},compact` rows straddle the `2^53` fast/slow boundary (the
    /// boundary this module's trim fix turns on); the rest are the
    /// `{decimals,N>0},compact` and `{scientific,N}` controls that must not regress.
    const SWEEP: &[(&str, &str, FloatFormat, i64, bool)] = &[
        // `{decimals,0},compact` below `2^53`: fast branch, integer zeros kept.
        ("1.0e15", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("2.0e15", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("7.0e15", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("9.0e15", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        // `{decimals,0},compact` above `2^53`: slow branch, zeros dropped.
        ("9.1e15", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("9.999e15", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.0e16", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("5.0e16", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.0e17", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.0e18", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.0e19", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.0e20", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("2.0e20", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.5e20", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.234e20", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("-1.0e20", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        // Controls: the non-compact / fractional / scientific forms are unchanged.
        ("1.0e16", "[{decimals,0}]", FloatFormat::Fixed, 0, false),
        ("1.0e20", "[{decimals,0}]", FloatFormat::Fixed, 0, false),
        ("1.0e20", "[{decimals,2},compact]", FloatFormat::Fixed, 2, true),
        ("1.0e20", "[{decimals,4},compact]", FloatFormat::Fixed, 4, true),
        ("9.1e15", "[{decimals,1},compact]", FloatFormat::Fixed, 1, true),
        ("1.0", "[{decimals,2},compact]", FloatFormat::Fixed, 2, true),
        ("1.0", "[{decimals,19},compact]", FloatFormat::Fixed, 19, true),
        ("1.5", "[{decimals,1},compact]", FloatFormat::Fixed, 1, true),
        ("0.0", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.0", "[{decimals,0},compact]", FloatFormat::Fixed, 0, true),
        ("1.0e16", "[{scientific,3}]", FloatFormat::Scientific, 3, false),
        ("1.0e20", "[{scientific,3}]", FloatFormat::Scientific, 3, false),
        ("9.1e15", "[{scientific,5}]", FloatFormat::Scientific, 5, false),
    ];

    /// The live-oracle cross-check for [`SWEEP`]: the oracle re-derives every
    /// expectation, so the pinned goldens above cannot silently drift from the
    /// real `float_to_list/2`.
    #[test]
    fn fixed_form_matches_the_live_oracle() {
        let Some(oracle) = resolve_oracle() else {
            eprintln!();
            eprintln!("=========== beam_bifs numeric: live oracle SKIPPED ===========");
            eprintln!("No usable system `erl` oracle found ($ERTS_ORACLE unset or not");
            eprintln!("executable, and no `erl` on $PATH).");
            eprintln!("This test is NOT a pass — set $ERTS_ORACLE (or install Erlang/OTP).");
            eprintln!("=============================================================");
            eprintln!();
            return;
        };

        // Build the oracle's case list from `SWEEP` itself, so the two sides cannot
        // drift out of step.
        let cases = SWEEP
            .iter()
            .map(|(literal, opts, ..)| format!("{{{literal},{opts}}}"))
            .collect::<Vec<_>>()
            .join(", ");
        let eval = format!(
            "Cases = [{cases}], \
             [io:format(\"~s~n\",[erlang:float_to_list(F,O)]) || {{F,O}} <- Cases], \
             halt()."
        );
        let output = std::process::Command::new(&oracle)
            .arg("-noshell")
            .arg("-eval")
            .arg(&eval)
            .arg("-s")
            .arg("init")
            .arg("stop")
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
        assert!(
            output.status.success(),
            "the oracle '{}' exited with {:?}\n--- stderr ---\n{}",
            oracle.display(),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr),
        );
        let stdout = String::from_utf8(output.stdout).expect("the oracle printed UTF-8");
        let lines: Vec<&str> = stdout.lines().collect();
        assert_eq!(lines.len(), SWEEP.len(), "oracle output:\n{stdout}");

        for ((literal, opts, format, decimals, compact), line) in SWEEP.iter().zip(&lines) {
            let value: f64 = literal.parse().expect("an Erlang float literal");
            let rendered = render_float(value, *format, *decimals, *compact)
                .unwrap_or_else(|e| panic!("float_to_list({literal}, {opts}) => {e:?}"));
            assert_eq!(&rendered, line, "float_to_list({literal}, {opts})");
        }
    }
}
