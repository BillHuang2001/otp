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
    if magnitude > FIXED_FAST_MAX || decimals >= FIXED_FAST_DECIMALS {
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
    if compact && has_decimals {
        trim_trailing_zeros(&mut out);
    }
    if out.len() > FLOAT_BUF_CHARS {
        return Err(badarg());
    }
    Ok(out)
}

/// Renders `value` for the `short` option the way `sys_double_to_chars_short`
/// (`../../erts/emulator/sys/common/erl_float_short.cpp`) does: the shortest
/// decimal that round-trips, written as `std::to_chars` writes it and then
/// rewritten by hand into the oracle's form.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a rendering the
/// oracle's buffer cannot hold (`do_float_to_charbuf` converts `to_chars`'s zero
/// return into `badarg`). An `f64` never comes close to 255 characters, so this
/// mirrors the C's guard rather than a reachable limit.
fn format_short(value: f64) -> Result<String> {
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
/// Rust's `{:e}` gives the shortest digits but rounds an exact tie *up*;
/// `std::to_chars` is Ryu-based and rounds such a tie to the **even** digit
/// (`256317133176956.125` ⇒ `…12`, where `{:e}` says `…13`), so a tie is
/// corrected here.
fn shortest_digits(value: f64) -> (String, i32) {
    // `{:e}` renders `d[.ddd]e<exp>` with the shortest round-trip digits.
    let rendered = format!("{value:e}");
    let (mantissa, exponent) = rendered
        .split_once('e')
        .expect("`{:e}` renders an exponent");
    let mut digits: String = mantissa.chars().filter(|ch| *ch != '.').collect();
    let exponent = exponent.parse().expect("`{:e}` renders an integer exponent");
    if tied_to_even(value, digits.len() as i32, exponent) {
        // `{:e}` rounded the tie up, so its last digit is the larger of the two
        // neighbours; the even one is kept (an already-even digit is unchanged).
        let last = digits.pop().expect("a float has a digit");
        let digit = last.to_digit(10).expect("a digit is decimal");
        if digit % 2 == 1 {
            digits.push(char::from_digit(digit - 1, 10).expect("a digit is decimal"));
        } else {
            digits.push(last);
        }
    }
    (digits, exponent)
}

/// Whether `value`'s exact decimal expansion is precisely halfway between the
/// two `digits`-long decimals around it — the case in which Ryu keeps the even
/// last digit.
///
/// `value` is a dyadic rational `significand / 2^scale`, and the tie means
/// `2 * value * 10^(digits - 1 - exponent)` is an *odd* integer (Ryu's
/// `d2d` tie test): the power of two left over must be exactly zero.
fn tied_to_even(value: f64, digits: i32, exponent: i32) -> bool {
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
        return false;
    }
    let shift = digits - 1 - exponent;
    shift >= 0 && significand.trailing_zeros() as i32 + shift == scale - 1
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
mod tmp_tests {
    use super::format_short;

    #[test]
    fn tmp_short_golden_sweep() {
        let Ok(path) = std::env::var("SHORT_GOLDEN") else {
            return;
        };
        let text = std::fs::read_to_string(path).expect("golden file");
        let mut checked = 0usize;
        let mut bad = Vec::new();
        for line in text.lines() {
            let Some((bits, expected)) = line.split_once('|') else {
                continue;
            };
            if bits.len() != 16 || !bits.bytes().all(|b| b.is_ascii_hexdigit()) {
                continue;
            }
            let bits = u64::from_str_radix(bits, 16).expect("hex bits");
            let value = f64::from_bits(bits);
            let got = format_short(value).expect("renders");
            checked += 1;
            if got != expected && bad.len() < 25 {
                bad.push(format!("{value:?} => got {got:?} want {expected:?}"));
            }
        }
        eprintln!("checked {checked}");
        assert!(bad.is_empty(), "{bad:#?}");
    }
}
