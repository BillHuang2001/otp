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
    /// `short`: Ryu's shortest round-trip (unsupported, see the module docs).
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

/// Renders `value` for `float_to_list/2` / `float_to_binary/2`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for the unsupported
/// `short` format or a rendering the oracle's buffer cannot hold.
pub(crate) fn render_float(
    value: f64,
    format: FloatFormat,
    decimals: i64,
    compact: bool,
) -> Result<String> {
    match format {
        FloatFormat::Legacy | FloatFormat::Scientific => format_scientific(value, decimals),
        FloatFormat::Fixed => format_fixed(value, decimals, compact),
        FloatFormat::Short => Err(badarg()),
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
