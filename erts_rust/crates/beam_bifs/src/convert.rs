//! Pure `erlang` conversions and numeric/term helpers that need no binaries:
//! `integer_to_list/1,2`, `list_to_integer/1,2`, `float_to_list/1,2`,
//! `list_to_float/1`, `atom_to_list/1`, `list_to_atom/1`,
//! `list_to_existing_atom/1`, `min/2`, `max/2`, `round/1`, `trunc/1`, `float/1`,
//! `ceil/1`, `floor/1`, `make_tuple/2,3`, `append_element/2`, `raise/3`, `node/0`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/bif.tab` (the roster),
//! `bif.c` (`integer_to_list`, `float_to_list`, `list_to_float`, `make_tuple`,
//! `append_element`, `raise_3`), `../../erts/emulator/beam/big.c`
//! (`erts_internal:list_to_integer/2`), `../../erts/emulator/beam/erl_bif_guard.c`
//! (`round`/`trunc`/`float`/`ceil`/`floor`),
//! `../../erts/emulator/sys/common/erl_sys_common_misc.c` and
//! `../../erts/emulator/sys/unix/sys_float.c` (the float formatter/parser), and
//! `../../erts/preloaded/src/erlang.erl` (the `min`/`max`/`list_to_integer`
//! wrappers).
//!
//! # Pinned against the OTP 29 (erts-17.1) oracle
//!
//! * `integer_to_list(255, 16)` ⇒ `"FF"` (uppercase digits), `(255, 2)` ⇒
//!   `"11111111"`, `(-255, 36)` ⇒ `"-73"`, `(0, 10)` ⇒ `"0"`; a base outside
//!   `2..=36` or a non-integer argument ⇒ `badarg`.
//! * `list_to_integer("+123")` ⇒ `123`, `("ff", 16)` ⇒ `255`, `("ZZ", 36)` ⇒
//!   `1295`; `""`, `" 12"`, `"12.0"`, `"-"`, `"2"` base 2 ⇒ `badarg`.
//! * `float_to_list(1.5)` ⇒ `"1.50000000000000000000e+00"` (the default is
//!   `{scientific, SYS_DEFAULT_FLOAT_DECIMALS = 20}`); `{decimals, 2}` ⇒
//!   `"1.50"`; `{decimals, 0}` ⇒ `"2"` for `1.5` and `"3"` for `2.5` (the fixed
//!   formatter rounds halves away from zero); `compact` only shortens a
//!   `{decimals, N}` result (`float_to_list(1.0, [{decimals,2}, compact])` ⇒
//!   `"1.0"`), never the legacy/scientific form.
//! * `list_to_float("1.5")` ⇒ `1.5`, `("1.5e3")` ⇒ `1500.0`, `("1,5")` ⇒ `1.5`
//!   (the comma radix is accepted); `"1"`, `"1."`, `".5"`, `"1e3"`, `" 1.5"` and
//!   an overflow such as `"1.0e309"` ⇒ `badarg`.
//! * `min(1.0, 1)` ⇒ `1.0` and `max(1, 1.0)` ⇒ `1`: both use the arithmetic term
//!   order and return the *first* argument when the two compare equal
//!   (`min(A,B) when A > B -> B; min(A,_) -> A.` in `erlang.erl`).
//! * `round(1.5)` ⇒ `2`, `round(-1.5)` ⇒ `-2`, `round(2.5)` ⇒ `3` (halves away
//!   from zero); `trunc(-1.7)` ⇒ `-1`; `ceil(-1.2)` ⇒ `-1`; `floor(-1.2)` ⇒ `-2`;
//!   `float(1)` ⇒ `1.0`; every one of them returns an integer argument unchanged.
//! * `make_tuple(3, a)` ⇒ `{a,a,a}`; `make_tuple(3, x, [{1,a},{3,c}])` ⇒
//!   `{a,x,c}` (the third argument is a list of `{Index, Value}` 2-tuples, *not*
//!   a list of values); an index outside `1..=Arity` ⇒ `badarg`;
//!   `append_element({1,2}, 3)` ⇒ `{1,2,3}`.
//! * `node()` ⇒ `nonode@nohost`.
//!
//! # Documented Milestone-1 gaps (never a silently wrong result)
//!
//! * **No bignums.** Integer values that leave the 60-bit small range report
//!   [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit) where real
//!   Erlang promotes to a bignum (`list_to_integer` of a 30-digit string,
//!   `round/1` of `1.0e30`).
//! * **`float_to_list` option `short` is unsupported** (`badarg`): it selects
//!   Ryu's shortest round-trip rendering. `{decimals, N}`, `{scientific, N}`
//!   and `compact` are supported; the oracle's rendering buffer is 256 bytes, so
//!   a rendering longer than 255 characters is `badarg` here exactly as there.
//! * **`raise/3` carries atom reasons only.** `beam_common::ExitReason` encodes
//!   an atom reason (named or `Atom(name)`) but no reason *term* (see
//!   [`crate::reason`]); a non-atom reason is therefore reported as `badarg`
//!   rather than raised. (erts-17.1 raises the term; the same representational
//!   gap is why `reason_term_from_exit_reason(Structured)` is `badarg`.)
//! * **`list_to_atom` interprets its elements as Unicode code points** and needs
//!   an installed [`AtomInterner`](crate::context::AtomInterner); without one an
//!   unknown name reports
//!   [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound).
//! * **`list_to_integer` requires byte-sized elements** (`0..=255`); erts
//!   truncates each element to a byte, so `list_to_integer([304])` is `0` there
//!   and `badarg` here.

use core::cmp::Ordering;

use beam_common::{ExceptionClass, Result};
use beam_term::compare::cmp_arith;
use beam_term::{atom_val, signed_value, AtomTable, Eterm, NIL};

use crate::access;
use crate::context::BifContext;
use crate::error::{badarg, raise_error, raise_exit, raise_throw, system_limit};
use crate::print::proper_list;
use crate::reason::exit_reason_from_term;
use crate::registry::BifRegistry;

/// `MAX_ARITYVAL` (`../../erts/emulator/beam/erl_term.h`): the largest tuple
/// arity, also `ERTS_MAX_TUPLE_SIZE`.
const MAX_TUPLE_SIZE: i64 = (1 << 24) - 1;

/// The maximum atom name length in *characters* (erts-17.1); longer names are
/// `system_limit` (`list_to_atom`).
const MAX_ATOM_CHARS: usize = 255;

/// `SYS_DEFAULT_FLOAT_DECIMALS` (`../../erts/emulator/beam/sys.h`): the mantissa
/// precision of the default `float_to_list/1` form.
const DEFAULT_FLOAT_DECIMALS: i64 = 20;

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
/// small integer (Milestone-1 has no bignums, so `is_small` is `is_integer`).
fn small_arg(term: Eterm) -> Result<i64> {
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
fn base_arg(term: Eterm) -> Result<u32> {
    let base = small_arg(term)?;
    if !(2..=36).contains(&base) {
        return Err(badarg());
    }
    Ok(base as u32)
}

/// A list of the bytes of the ASCII string `s`, built on the context heap.
///
/// # Errors
///
/// Propagates the heap's allocation failure.
fn ascii_list(ctx: &mut BifContext<'_>, s: &str) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(s.len());
    for byte in s.bytes() {
        elems.push(ctx.int(i64::from(byte))?);
    }
    ctx.list(&elems)
}

/// A list of the code points of `name`, built on the context heap (the shape
/// `atom_to_list/1` returns).
///
/// # Errors
///
/// Propagates the heap's allocation failure.
fn code_point_list(ctx: &mut BifContext<'_>, name: &str) -> Result<Eterm> {
    let mut elems = Vec::with_capacity(name.len());
    for ch in name.chars() {
        elems.push(ctx.int(i64::from(u32::from(ch)))?);
    }
    ctx.list(&elems)
}

// ---------------------------------------------------------------------------
// integer_to_list/1,2 and list_to_integer/1,2
// ---------------------------------------------------------------------------

/// Renders `value` in `base` the way `Sint_to_buf` does: uppercase digits
/// `A`..`Z` for 10..35 and a leading `-`.
fn int_to_digits(value: i64, base: u32) -> String {
    let mut n = i128::from(value).unsigned_abs();
    let radix = u128::from(base);
    let mut digits = Vec::new();
    if n == 0 {
        digits.push(b'0');
    }
    while n > 0 {
        let digit = (n % radix) as u8;
        digits.push(if digit < 10 {
            b'0' + digit
        } else {
            b'A' + (digit - 10)
        });
        n /= radix;
    }
    let mut out = String::with_capacity(digits.len() + 1);
    if value < 0 {
        out.push('-');
    }
    for &digit in digits.iter().rev() {
        out.push(char::from(digit));
    }
    out
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
/// `erts_internal:list_to_integer/2` does (an optional `+`/`-`, then digits).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a bad character, a
/// missing digit or a non-byte element; [`BeamError::SystemLimit`] for a value
/// beyond the 60-bit small range (real Erlang promotes to a bignum, see the
/// module docs).
fn parse_integer(text: &str, base: u32) -> Result<i64> {
    let bytes = text.as_bytes();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty() {
        return Err(badarg());
    }
    // `i64::MAX + 1` is the smallest magnitude that cannot be a small integer, so
    // anything beyond it is reported as a limit rather than as a wrong value.
    let ceiling: u128 = (i64::MAX as u128) + 1;
    let mut magnitude: u128 = 0;
    for &byte in digits {
        let digit = digit_value(byte, base).ok_or_else(badarg)?;
        magnitude = magnitude * u128::from(base) + u128::from(digit);
        if magnitude > ceiling {
            return Err(system_limit());
        }
    }
    let signed = if negative {
        -(magnitude as i128)
    } else {
        magnitude as i128
    };
    i64::try_from(signed).map_err(|_| system_limit())
}

/// `integer_to_list(Integer)` — base 10.
fn integer_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = small_arg(args[0])?;
    let rendered = int_to_digits(value, 10);
    ascii_list(ctx, &rendered)
}

/// `integer_to_list(Integer, Base)` for `2 =< Base =< 36`.
fn integer_to_list_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = small_arg(args[0])?;
    let base = base_arg(args[1])?;
    let rendered = int_to_digits(value, base);
    ascii_list(ctx, &rendered)
}

/// `list_to_integer(String)` — base 10.
fn list_to_integer_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    list_to_integer(ctx, args[0], 10)
}

/// `list_to_integer(String, Base)` for `2 =< Base =< 36`.
fn list_to_integer_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let base = base_arg(args[1])?;
    list_to_integer(ctx, args[0], base)
}

/// `list_to_integer/1,2`: a proper list of byte-sized integers, parsed whole.
fn list_to_integer(ctx: &mut BifContext<'_>, list: Eterm, base: u32) -> Result<Eterm> {
    let elems = proper_list(list).ok_or_else(badarg)?;
    let mut text = String::with_capacity(elems.len());
    for elem in elems {
        if !access::is_small(elem) {
            return Err(badarg());
        }
        // erts narrows each element to a `byte`; Milestone-1 requires a byte
        // outright (see the module docs).
        let byte = u8::try_from(signed_value(elem)).map_err(|_| badarg())?;
        text.push(char::from(byte));
    }
    let value = parse_integer(&text, base)?;
    ctx.int(value)
}

// ---------------------------------------------------------------------------
// float_to_list/1,2
// ---------------------------------------------------------------------------

/// The `fmt_type_` of `do_float_to_charbuf`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FloatFormat {
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

/// Parses `float_to_list/2`'s option list, left to right (a later option wins,
/// exactly as `do_float_to_charbuf` overwrites `decimals`/`fmt_type`).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list, an
/// improper list or an option outside `compact`/`short`/`{decimals,N}`/
/// `{scientific,N}` with a small `N`.
fn parse_float_options(atoms: &AtomTable, opts: Eterm) -> Result<(FloatFormat, i64, bool)> {
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
/// 6 — `float_to_list(1.5, [{scientific,-1}])` is `"1.500000e+00"` in the
/// oracle too.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) when the rendering does
/// not fit the oracle's 256-byte buffer.
fn format_scientific(value: f64, decimals: i64) -> Result<String> {
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
/// [{decimals,1}])` is `"0.2"` (a plain correctly-rounded `%.1f` would say
/// `"0.1"`); larger magnitudes and precisions fall back to `%.*f`.
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

/// Renders `value` for `float_to_list/2`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for the unsupported
/// `short` format or a rendering the oracle's buffer cannot hold.
fn render_float(value: f64, format: FloatFormat, decimals: i64, compact: bool) -> Result<String> {
    match format {
        FloatFormat::Legacy | FloatFormat::Scientific => format_scientific(value, decimals),
        FloatFormat::Fixed => format_fixed(value, decimals, compact),
        FloatFormat::Short => Err(badarg()),
    }
}

/// `float_to_list(Float)` — `[{scientific, 20}]`.
fn float_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_float(args[0]) {
        return Err(badarg());
    }
    let rendered = format_scientific(access::float_value(args[0]), DEFAULT_FLOAT_DECIMALS)?;
    ascii_list(ctx, &rendered)
}

/// `float_to_list(Float, Options)`.
fn float_to_list_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_float(args[0]) {
        return Err(badarg());
    }
    let value = access::float_value(args[0]);
    let (format, decimals, compact) = parse_float_options(ctx.atoms(), args[1])?;
    let rendered = render_float(value, format, decimals, compact)?;
    ascii_list(ctx, &rendered)
}

// ---------------------------------------------------------------------------
// list_to_float/1
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
fn parse_float(text: &str) -> Result<f64> {
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

/// `list_to_float(String)`.
fn list_to_float_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(badarg)?;
    let mut text = String::with_capacity(elems.len());
    for elem in elems {
        if !access::is_small(elem) {
            return Err(badarg());
        }
        let byte = u8::try_from(signed_value(elem)).map_err(|_| badarg())?;
        text.push(char::from(byte));
    }
    let value = parse_float(&text)?;
    ctx.make_float(value)
}

// ---------------------------------------------------------------------------
// atom_to_list/1, list_to_atom/1, list_to_existing_atom/1
// ---------------------------------------------------------------------------

/// The name spelled by a list of code points (`atom_to_list`'s inverse). The
/// elements are Unicode scalar values, so `[16#D800]` and `[16#110000]` are
/// `badarg` exactly as in erts-17.1; `[]` spells the empty atom `''`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-list, an
/// improper list, a non-integer element or an invalid code point.
fn chars_to_string(list: Eterm) -> Result<String> {
    let elems = proper_list(list).ok_or_else(badarg)?;
    let mut name = String::with_capacity(elems.len());
    for elem in elems {
        if !access::is_small(elem) {
            return Err(badarg());
        }
        let code = u32::try_from(signed_value(elem)).map_err(|_| badarg())?;
        name.push(char::from_u32(code).ok_or_else(badarg)?);
    }
    Ok(name)
}

/// `atom_to_list(Atom)` — the atom's code points.
fn atom_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_atom(args[0]) {
        return Err(badarg());
    }
    let name = ctx
        .atoms()
        .name(atom_val(args[0]))
        .ok_or_else(badarg)?
        .to_string();
    code_point_list(ctx, &name)
}

/// `list_to_atom(String)`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a bad character
/// list, [`BeamError::SystemLimit`] for a name longer than 255 characters, and
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the
/// name is new and no [`AtomInterner`](crate::context::AtomInterner) is
/// installed (see the module docs).
fn list_to_atom_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let name = chars_to_string(args[0])?;
    if name.chars().count() > MAX_ATOM_CHARS {
        return Err(system_limit());
    }
    ctx.intern_atom(&name)
}

/// `list_to_existing_atom(String)` — never creates an atom.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a bad character
/// list or a name that is not already interned.
fn list_to_existing_atom_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let name = chars_to_string(args[0])?;
    ctx.atom(&name).map_err(|_| badarg())
}

// ---------------------------------------------------------------------------
// min/2, max/2 and the numeric conversions
// ---------------------------------------------------------------------------

/// `min(A, B)` — `min(A, B) when A > B -> B; min(A, _) -> A.` in `erlang.erl`,
/// with the arithmetic term order (`min(1.0, 1)` is `1.0`).
fn min_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if cmp_arith(args[0], args[1], ctx.atoms()) == Ordering::Greater {
        Ok(args[1])
    } else {
        Ok(args[0])
    }
}

/// `max(A, B)` — `max(A, B) when A < B -> B; max(A, _) -> A.`, arithmetic order.
fn max_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if cmp_arith(args[0], args[1], ctx.atoms()) == Ordering::Less {
        Ok(args[1])
    } else {
        Ok(args[0])
    }
}

/// The float value of a number argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-number.
fn as_float(term: Eterm) -> Result<f64> {
    if access::is_float(term) {
        return Ok(access::float_value(term));
    }
    if access::is_small(term) {
        return Ok(signed_value(term) as f64);
    }
    Err(badarg())
}

/// A rounded/truncated integer result.
///
/// `float` must be finite (an infinite or NaN float cannot exist in Erlang —
/// Milestone-1 arithmetic can still build one, and it is `badarg` here rather
/// than a saturated, silently wrong integer).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-finite value
/// or a non-number argument; [`BeamError::SystemLimit`] when the result leaves
/// the 60-bit small range (real Erlang returns a bignum).
fn integer_result(ctx: &mut BifContext<'_>, term: Eterm, float: f64) -> Result<Eterm> {
    if !access::is_number(term) {
        return Err(badarg());
    }
    if !float.is_finite() {
        return Err(badarg());
    }
    ctx.int(float as i64)
}

/// `round(Number)` — halves away from zero (`round(2.5)` is `3`).
fn round_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = as_float(args[0])?;
    integer_result(ctx, args[0], value.round())
}

/// `trunc(Number)` — toward zero (`trunc(-1.7)` is `-1`).
fn trunc_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = as_float(args[0])?;
    integer_result(ctx, args[0], value.trunc())
}

/// `ceil(Number)`.
fn ceil_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = as_float(args[0])?;
    integer_result(ctx, args[0], value.ceil())
}

/// `floor(Number)`.
fn floor_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let value = as_float(args[0])?;
    integer_result(ctx, args[0], value.floor())
}

/// `float(Number)` — an integer becomes a float; a float is returned as is.
fn float_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match args[0] {
        term if access::is_float(term) => Ok(term),
        term if access::is_small(term) => ctx.make_float(signed_value(term) as f64),
        _ => Err(badarg()),
    }
}

// ---------------------------------------------------------------------------
// make_tuple/2,3 and append_element/2
// ---------------------------------------------------------------------------

/// The tuple arity of a `make_tuple`/`append_element` argument.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-integer, a
/// negative arity or an arity above `ERTS_MAX_TUPLE_SIZE` (`(1 << 24) - 1`).
fn arity_arg(term: Eterm) -> Result<usize> {
    let arity = small_arg(term)?;
    if arity < 0 || arity > MAX_TUPLE_SIZE {
        return Err(badarg());
    }
    Ok(arity as usize)
}

/// `make_tuple(Arity, Default)`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a bad arity;
/// [`BeamError::SystemLimit`] when the tuple cannot fit the process heap
/// (Milestone-1 heaps are fixed-size).
fn make_tuple_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let arity = arity_arg(args[0])?;
    if arity + 1 > ctx.heap().available() {
        return Err(system_limit());
    }
    let elements = vec![args[1]; arity];
    ctx.tuple(&elements)
}

/// `make_tuple(Arity, Default, InitList)` — `InitList` holds `{Index, Value}`
/// 2-tuples, each overwriting element `Index` (1-based).
fn make_tuple_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let arity = arity_arg(args[0])?;
    let updates = proper_list(args[2]).ok_or_else(badarg)?;
    if arity + 1 > ctx.heap().available() {
        return Err(system_limit());
    }
    let mut elements = vec![args[1]; arity];
    for update in updates {
        if !access::is_tuple(update) || access::tuple_size(update) != 2 {
            return Err(badarg());
        }
        let index = access::tuple_element(update, 0);
        if !access::is_small(index) {
            return Err(badarg());
        }
        let index = signed_value(index);
        if index < 1 || index > arity as i64 {
            return Err(badarg());
        }
        elements[index as usize - 1] = access::tuple_element(update, 1);
    }
    ctx.tuple(&elements)
}

/// `append_element(Tuple, Element)`.
fn append_element_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let tuple = args[0];
    if !access::is_tuple(tuple) {
        return Err(badarg());
    }
    let arity = access::tuple_size(tuple);
    if arity as i64 + 1 > MAX_TUPLE_SIZE {
        return Err(badarg());
    }
    if arity + 2 > ctx.heap().available() {
        return Err(system_limit());
    }
    let mut elements: Vec<Eterm> = (0..arity).map(|i| access::tuple_element(tuple, i)).collect();
    elements.push(args[1]);
    ctx.tuple(&elements)
}

// ---------------------------------------------------------------------------
// raise/3 and node/0
// ---------------------------------------------------------------------------

/// The exception class of `raise/3`'s first argument, which must be the atom
/// `error`, `exit` or `throw`.
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for anything else.
fn exception_class(atoms: &AtomTable, class: Eterm) -> Result<ExceptionClass> {
    if !access::is_atom(class) {
        return Err(badarg());
    }
    match atoms.name(atom_val(class)) {
        Some("error") => Ok(ExceptionClass::Error),
        Some("exit") => Ok(ExceptionClass::Exit),
        Some("throw") => Ok(ExceptionClass::Throw),
        _ => Err(badarg()),
    }
}

/// Whether `location` (the last element of a stacktrace frame) is a list.
fn is_location(location: Eterm) -> bool {
    location == NIL || access::is_list(location)
}

/// Whether `frame` is one of the stacktrace shapes `raise_3` accepts:
/// `{Fun, Args}`, `{Fun, Args, Location}`, `{M, F, A}` or `{M, F, A, Location}`.
fn is_stacktrace_frame(frame: Eterm) -> bool {
    if !access::is_tuple(frame) {
        return false;
    }
    match access::tuple_size(frame) {
        2 => access::is_function(access::tuple_element(frame, 0)),
        3 => {
            if access::is_function(access::tuple_element(frame, 0)) {
                is_location(access::tuple_element(frame, 2))
            } else {
                access::is_atom(access::tuple_element(frame, 0))
                    && access::is_atom(access::tuple_element(frame, 1))
            }
        }
        4 => {
            access::is_atom(access::tuple_element(frame, 0))
                && access::is_atom(access::tuple_element(frame, 1))
                && is_location(access::tuple_element(frame, 3))
        }
        _ => false,
    }
}

/// Whether `stacktrace` is a proper list of valid stacktrace frames.
fn is_stacktrace(stacktrace: Eterm) -> bool {
    match proper_list(stacktrace) {
        Some(frames) => frames.into_iter().all(is_stacktrace_frame),
        None => false,
    }
}

/// `raise(Class, Reason, Stacktrace)`.
///
/// The class must be `error`/`exit`/`throw` and the stacktrace must be a proper
/// list of valid frames (`raise(error, foo, [notatuple])` is `badarg` in the
/// oracle too). The reason is mapped through [`exit_reason_from_term`], which
/// carries atom reasons; a non-atom reason is `badarg` (see the module docs).
fn raise_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let class = exception_class(ctx.atoms(), args[0])?;
    if !is_stacktrace(args[2]) {
        return Err(badarg());
    }
    if !access::is_atom(args[1]) {
        return Err(badarg());
    }
    let reason = exit_reason_from_term(args[1], ctx.atoms());
    Err(match class {
        ExceptionClass::Error => raise_error(reason),
        ExceptionClass::Exit => raise_exit(reason),
        ExceptionClass::Throw => raise_throw(reason),
        // `ExceptionClass` is `#[non_exhaustive]`; `exception_class` above only
        // ever produces the three classes handled here, so this arm is
        // unreachable (defaulting to `error` keeps it total without a panic).
        _ => raise_error(reason),
    })
}

/// `node()` — the atom `nonode@nohost` (Milestone-1 never starts distribution).
///
/// # Errors
///
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when the
/// runtime's atom table does not hold `nonode@nohost` (it is a standard
/// `atom.names` entry, so this only signals an unseeded table).
fn node_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.atom("nonode@nohost")
}

/// Registers the pure `erlang` conversion/numeric BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "integer_to_list", 1, integer_to_list_1);
    reg.register("erlang", "integer_to_list", 2, integer_to_list_2);
    reg.register("erlang", "list_to_integer", 1, list_to_integer_1);
    reg.register("erlang", "list_to_integer", 2, list_to_integer_2);
    reg.register("erlang", "float_to_list", 1, float_to_list_1);
    reg.register("erlang", "float_to_list", 2, float_to_list_2);
    reg.register("erlang", "list_to_float", 1, list_to_float_1);
    reg.register("erlang", "atom_to_list", 1, atom_to_list_1);
    reg.register("erlang", "list_to_atom", 1, list_to_atom_1);
    reg.register("erlang", "list_to_existing_atom", 1, list_to_existing_atom_1);
    reg.register("erlang", "min", 2, min_2);
    reg.register("erlang", "max", 2, max_2);
    reg.register("erlang", "round", 1, round_1);
    reg.register("erlang", "trunc", 1, trunc_1);
    reg.register("erlang", "float", 1, float_1);
    reg.register("erlang", "ceil", 1, ceil_1);
    reg.register("erlang", "floor", 1, floor_1);
    reg.register("erlang", "make_tuple", 2, make_tuple_2);
    reg.register("erlang", "make_tuple", 3, make_tuple_3);
    reg.register("erlang", "append_element", 2, append_element_2);
    reg.register("erlang", "raise", 3, raise_3);
    reg.register("erlang", "node", 0, node_0);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{Atom, AtomTable};

    /// An [`AtomInterner`](crate::context::AtomInterner) over its own table; the
    /// table *is* the shared atom table in a real runtime (see `context.rs`).
    struct Interner {
        table: AtomTable,
    }

    impl crate::context::AtomInterner for Interner {
        fn intern(&mut self, name: &str) -> Atom {
            self.table.intern(name)
        }
    }

    /// The names the tests build terms from; `nonode@nohost`, `error`, `exit`
    /// and `throw` are the ones the BIFs look up themselves.
    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "a",
            "b",
            "x",
            "hello",
            "nonode@nohost",
            "error",
            "exit",
            "throw",
        ]);
        atoms
    }

    fn heap() -> Heap {
        Heap::new(1024).expect("test heap")
    }

    /// Runs `f` over a fresh context.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = heap();
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// The byte list for `s`, built on the context heap.
    fn bytes(ctx: &mut BifContext<'_>, s: &str) -> Eterm {
        ascii_list(ctx, s).expect("the list fits")
    }

    /// The string a byte/code-point list spells.
    fn spell(term: Eterm) -> String {
        proper_list(term)
            .expect("proper list")
            .into_iter()
            .map(|elem| char::from(u8::try_from(signed_value(elem)).expect("a byte")))
            .collect()
    }

    /// The integer a BIF returned.
    fn int_of(term: Eterm) -> i64 {
        assert!(access::is_small(term), "not a small integer");
        signed_value(term)
    }

    // -- integer_to_list / list_to_integer --------------------------------

    #[test]
    fn integer_to_list_matches_the_oracle() {
        with_ctx(|ctx| {
            for (value, expected) in [(0i64, "0"), (123, "123"), (-123, "-123"), (-1, "-1")] {
                let term = integer_to_list_1(ctx, &[ctx.int(value).expect("fits")]).expect("ok");
                assert_eq!(spell(term), expected, "integer_to_list({value})");
            }
            // Base conversion: uppercase digits, base 2/8/16/36.
            for (value, base, expected) in [
                (255i64, 16i64, "FF"),
                (255, 2, "11111111"),
                (255, 8, "377"),
                (-255, 36, "-73"),
                (-255, 16, "-FF"),
                (10, 2, "1010"),
                (35, 36, "Z"),
            ] {
                let args = [ctx.int(value).expect("fits"), ctx.int(base).expect("fits")];
                let term = integer_to_list_2(ctx, &args).expect("ok");
                assert_eq!(spell(term), expected, "integer_to_list({value}, {base})");
            }
            // A bad base or a non-integer is `badarg`.
            for base in [1i64, 37, 0, -2] {
                let args = [ctx.int(5).expect("fits"), ctx.int(base).expect("fits")];
                assert!(
                    matches!(integer_to_list_2(ctx, &args), Err(BeamError::BadArg)),
                    "base {base}"
                );
            }
            let float_base = [ctx.int(5).expect("fits"), ctx.make_float(2.0).expect("fits")];
            assert!(matches!(
                integer_to_list_2(ctx, &float_base),
                Err(BeamError::BadArg)
            ));
            let float = ctx.make_float(5.0).expect("fits");
            assert!(matches!(
                integer_to_list_1(ctx, &[float]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn list_to_integer_matches_the_oracle() {
        with_ctx(|ctx| {
            for (text, base, expected) in [
                ("123", 10i64, 123i64),
                ("-123", 10, -123),
                ("+123", 10, 123),
                ("0", 10, 0),
                ("00", 10, 0),
                ("ff", 16, 255),
                ("FF", 16, 255),
                ("zz", 36, 1295),
                ("-73", 36, -73),
            ] {
                let args = [bytes(ctx, text), ctx.int(base).expect("fits")];
                let term = list_to_integer_2(ctx, &args).expect("parses");
                assert_eq!(int_of(term), expected, "list_to_integer({text:?}, {base})");
            }
            // `list_to_integer/1` is base 10.
            let arg = bytes(ctx, "-42");
            let term = list_to_integer_1(ctx, &[arg]).expect("parses");
            assert_eq!(int_of(term), -42);

            // Every invalid form the oracle rejects.
            for text in ["", " 12", "12 ", "12.0", "-", "+", "a", "1a", "1_0"] {
                let args = [bytes(ctx, text), ctx.int(10).expect("fits")];
                assert!(
                    matches!(list_to_integer_2(ctx, &args), Err(BeamError::BadArg)),
                    "list_to_integer({text:?})"
                );
            }
            // A digit outside the base, a non-list, an improper list and a
            // non-byte element.
            let two = bytes(ctx, "2");
            let base_two = ctx.int(2).expect("fits");
            assert!(matches!(
                list_to_integer_2(ctx, &[two, base_two]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                list_to_integer_1(ctx, &[ctx.int(1).expect("fits")]),
                Err(BeamError::BadArg)
            ));
            let improper =
                ctx.cons(ctx.int(49).expect("fits"), ctx.int(50).expect("fits"))
                    .expect("fits");
            assert!(matches!(
                list_to_integer_1(ctx, &[improper]),
                Err(BeamError::BadArg)
            ));
            let big = ctx
                .list(&[ctx.int(49).expect("fits"), ctx.int(304).expect("fits")])
                .expect("fits");
            assert!(matches!(
                list_to_integer_1(ctx, &[big]),
                Err(BeamError::BadArg)
            ));
            // A value past the small range: a bignum in real Erlang (documented).
            let huge = bytes(ctx, "99999999999999999999999999");
            assert!(matches!(
                list_to_integer_1(ctx, &[huge]),
                Err(BeamError::SystemLimit)
            ));
        });
    }

    // -- float_to_list / list_to_float ------------------------------------

    #[test]
    fn float_to_list_default_is_scientific_20() {
        with_ctx(|ctx| {
            for (value, expected) in [
                (1.5f64, "1.50000000000000000000e+00"),
                (0.0, "0.00000000000000000000e+00"),
                (-0.0, "-0.00000000000000000000e+00"),
                (-2.5, "-2.50000000000000000000e+00"),
                (1.0e10, "1.00000000000000000000e+10"),
                (100.0, "1.00000000000000000000e+02"),
                (0.1, "1.00000000000000005551e-01"),
                (3.14159, "3.14158999999999988262e+00"),
                (1.0e-5, "1.00000000000000008180e-05"),
            ] {
                let term = ctx.make_float(value).expect("fits");
                let rendered = float_to_list_1(ctx, &[term]).expect("renders");
                assert_eq!(spell(rendered), expected, "float_to_list({value})");
            }
            // A non-float is `badarg`.
            let one = ctx.int(1).expect("fits");
            assert!(matches!(float_to_list_1(ctx, &[one]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn float_to_list_options_match_the_oracle() {
        with_ctx(|ctx| {
            let options = |ctx: &mut BifContext<'_>, items: &[Eterm]| ctx.list(items).expect("fits");
            let decimals = |ctx: &mut BifContext<'_>, n: i64| {
                let atom = beam_term::make_atom(ctx.atoms().lookup("decimals").expect("seeded"));
                ctx.tuple(&[atom, ctx.int(n).expect("fits")]).expect("fits")
            };
            let scientific = |ctx: &mut BifContext<'_>, n: i64| {
                let atom = beam_term::make_atom(ctx.atoms().lookup("scientific").expect("seeded"));
                ctx.tuple(&[atom, ctx.int(n).expect("fits")]).expect("fits")
            };
            let compact = beam_term::make_atom(ctx.atoms().lookup("compact").expect("seeded"));
            let short = beam_term::make_atom(ctx.atoms().lookup("short").expect("seeded"));
            // `{decimals, N}` is `%.*f` (halves away from zero via the fast path).
            for (value, n, expected) in [
                (1.5f64, 2i64, "1.50"),
                (1.5, 0, "2"),
                (2.5, 0, "3"),
                (-2.5, 0, "-3"),
                (0.5, 0, "1"),
                (2.5, 0, "3"),
                (0.15, 1, "0.2"),
                (0.25, 1, "0.3"),
                (1.005, 2, "1.00"),
                (1.0, 4, "1.0000"),
                (1.0e10, 2, "10000000000.00"),
                (-1.5, 2, "-1.50"),
                (1.0e-5, 7, "0.0000100"),
                (1.0e16, 2, "10000000000000000.00"),
                (1.0e16, 20, "10000000000000000.00000000000000000000"),
                (0.1, 1, "0.1"),
            ] {
                let value_term = ctx.make_float(value).expect("fits");
                let item = decimals(ctx, n);
                let opts = options(ctx, &[item]);
                let rendered = float_to_list_2(ctx, &[value_term, opts]).expect("renders");
                assert_eq!(spell(rendered), expected, "float_to_list(_, [{{decimals,{n}}}])");
            }

            // `{scientific, N}`.
            for (value, n, expected) in [
                (1.5f64, 4i64, "1.5000e+00"),
                (1.5, 0, "2e+00"),
                (0.0, 0, "0e+00"),
                (0.5, 0, "5e-01"),
                (1234.5678, 3, "1.235e+03"),
                (123456789.0, 2, "1.23e+08"),
                // A negative precision is `printf`'s "omitted" → the default 6.
                (1.5, -1, "1.500000e+00"),
            ] {
                let value_term = ctx.make_float(value).expect("fits");
                let item = scientific(ctx, n);
                let opts = options(ctx, &[item]);
                let rendered = float_to_list_2(ctx, &[value_term, opts]).expect("renders");
                assert_eq!(spell(rendered), expected, "float_to_list(_, scientific {n})");
            }

            // `compact` alone leaves the legacy form alone, but shortens a fixed
            // rendering (keeping one digit after the radix point).
            for (value, items, expected) in [
                (1.5f64, vec![compact], "1.50000000000000000000e+00"),
                (1.0, vec![compact], "1.00000000000000000000e+00"),
            ] {
                let value = ctx.make_float(value).expect("fits");
                let opts = options(ctx, &items);
                let rendered = float_to_list_2(ctx, &[value, opts]).expect("renders");
                assert_eq!(spell(rendered), expected);
            }
            for (value, n, expected) in [
                (1.5f64, 2i64, "1.5"),
                (1.0, 2, "1.0"),
                (0.0, 2, "0.0"),
                (1.0e16, 2, "10000000000000000.0"),
            ] {
                let value_term = ctx.make_float(value).expect("fits");
                let item = decimals(ctx, n);
                let opts = options(ctx, &[item, compact]);
                let rendered = float_to_list_2(ctx, &[value_term, opts]).expect("renders");
                assert_eq!(spell(rendered), expected, "compact {n}");
            }

            // Option order: the last of `{decimals,_}`/`{scientific,_}` wins.
            let d2 = decimals(ctx, 2);
            let s5 = scientific(ctx, 5);
            let both = options(ctx, &[d2, s5]);
            let value = ctx.make_float(1.5).expect("fits");
            assert_eq!(spell(float_to_list_2(ctx, &[value, both]).expect("ok")), "1.50000e+00");
            let d2 = decimals(ctx, 2);
            let s5 = scientific(ctx, 5);
            let both = options(ctx, &[s5, d2]);
            assert_eq!(spell(float_to_list_2(ctx, &[value, both]).expect("ok")), "1.50");

            // Unsupported/invalid options.
            let short_opts = options(ctx, &[short]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, short_opts]),
                Err(BeamError::BadArg)
            ));
            let unknown = options(ctx, &[beam_term::make_atom(atoms().lookup("a").expect("seeded"))]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, unknown]),
                Err(BeamError::BadArg)
            ));
            let bare_tuple = decimals(ctx, 2);
            assert!(matches!(
                float_to_list_2(ctx, &[value, bare_tuple]),
                Err(BeamError::BadArg)
            ));
            let float_precision = ctx
                .tuple(&[
                    beam_term::make_atom(atoms().lookup("decimals").expect("seeded")),
                    ctx.make_float(2.0).expect("fits"),
                ])
                .expect("fits");
            let opts = options(ctx, &[float_precision]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, opts]),
                Err(BeamError::BadArg)
            ));
            let negative = options(ctx, &[decimals(ctx, -1)]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, negative]),
                Err(BeamError::BadArg)
            ));
            // The oracle's rendering buffer is 256 bytes: 255 characters fit.
            let fits = options(ctx, &[decimals(ctx, 253)]);
            assert_eq!(
                spell(float_to_list_2(ctx, &[value, fits]).expect("fits")).len(),
                255
            );
            let overflow = options(ctx, &[decimals(ctx, 254)]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, overflow]),
                Err(BeamError::BadArg)
            ));
            let fits = options(ctx, &[scientific(ctx, 249)]);
            assert_eq!(
                spell(float_to_list_2(ctx, &[value, fits]).expect("fits")).len(),
                255
            );
            let overflow = options(ctx, &[scientific(ctx, 250)]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, overflow]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn list_to_float_matches_the_oracle() {
        with_ctx(|ctx| {
            for (text, expected) in [
                ("1.5", 1.5f64),
                ("1.5e3", 1500.0),
                ("-1.5", -1.5),
                ("+1.5", 1.5),
                ("1.5E3", 1500.0),
                ("1,5", 1.5),
                ("1.5e-3", 0.0015),
                ("1.5e+3", 1500.0),
                ("0.0", 0.0),
            ] {
                let term = list_to_float_1(ctx, &[bytes(ctx, text)]).expect("parses");
                assert!(access::is_float(term), "{text:?} is not a float");
                assert_eq!(access::float_value(term), expected, "list_to_float({text:?})");
            }
            // Underflow to zero is accepted (the C `errno == ERANGE` special case).
            let underflow = list_to_float_1(ctx, &[bytes(ctx, "1.0e-400")]).expect("parses");
            assert_eq!(access::float_value(underflow), 0.0);

            for text in ["", "1", "1.", ".5", "1e3", "1.5e", "1.5.5", " 1.5", "abc", "1.5e309"] {
                assert!(
                    matches!(list_to_float_1(ctx, &[bytes(ctx, text)]), Err(BeamError::BadArg)),
                    "list_to_float({text:?})"
                );
            }
            // Non-list and improper list arguments.
            let float = ctx.make_float(1.5).expect("fits");
            assert!(matches!(list_to_float_1(ctx, &[float]), Err(BeamError::BadArg)));
            let improper =
                ctx.cons(ctx.int(49).expect("fits"), ctx.int(50).expect("fits"))
                    .expect("fits");
            assert!(matches!(
                list_to_float_1(ctx, &[improper]),
                Err(BeamError::BadArg)
            ));
        });
    }

    // -- atoms ------------------------------------------------------------

    #[test]
    fn atom_to_list_matches_the_oracle() {
        with_ctx(|ctx| {
            let hello = ctx.atom("hello").expect("seeded");
            assert_eq!(spell(atom_to_list_1(ctx, &[hello]).expect("ok")), "hello");
            // The empty name is the atom `''`; the tests' table has no `''`, so
            // look it up in a table that does.
            let a = ctx.atom("a").expect("seeded");
            assert_eq!(spell(atom_to_list_1(ctx, &[a]).expect("ok")), "a");
            // A non-atom is `badarg`.
            let one = ctx.int(1).expect("fits");
            assert!(matches!(atom_to_list_1(ctx, &[one]), Err(BeamError::BadArg)));
        });
    }

    #[test]
    fn list_to_atom_and_existing_atom_match_the_oracle() {
        let mut heap = heap();
        let mut atoms = atoms();
        // The runtime's global table also holds the names the BIFs themselves
        // look up.
        atoms.seed(["decimals", "scientific", "compact", "short", "nonode@nohost"]);
        let mut interner = Interner {
            table: AtomTable::new(),
        };
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0))
                .with_atom_interner(&mut interner);

            // An already-interned name never consults the hook.
            let existing = bytes(&mut ctx, "hello");
            let term = list_to_atom_1(&mut ctx, &[existing]).expect("resolves");
            assert_eq!(atoms.name(atom_val(term)), Some("hello"));

            // A new name goes through the `AtomInterner` hook.
            let fresh = bytes(&mut ctx, "brand_new");
            let term = list_to_atom_1(&mut ctx, &[fresh]).expect("interns");
            assert_eq!(interner.table.name(atom_val(term)), Some("brand_new"));

            // The empty atom `''` is legal, and code points round-trip.
            let empty = ctx.list(&[]).expect("fits");
            let term = list_to_atom_1(&mut ctx, &[empty]).expect("interns");
            assert_eq!(interner.table.name(atom_val(term)), Some(""));
            let points = ctx
                .list(&[ctx.int(255).expect("fits"), ctx.int(300).expect("fits")])
                .expect("fits");
            let term = list_to_atom_1(&mut ctx, &[points]).expect("interns");
            let name = interner.table.name(atom_val(term)).expect("interned");
            assert_eq!(name.chars().collect::<Vec<_>>(), vec!['\u{ff}', '\u{12c}']);

            // Invalid character lists.
            let improper_atom_list = ctx
                .cons(ctx.int(1).expect("fits"), ctx.int(2).expect("fits"))
                .expect("fits");
            for bad in [ctx.int(1).expect("fits"), improper_atom_list] {
                assert!(matches!(list_to_atom_1(&mut ctx, &[bad]), Err(BeamError::BadArg)));
            }
            for code in [0xD800i64, 0x11_0000, -1] {
                let list = ctx.list(&[ctx.int(code).expect("fits")]).expect("fits");
                assert!(
                    matches!(list_to_atom_1(&mut ctx, &[list]), Err(BeamError::BadArg)),
                    "code point {code}"
                );
            }
            // 255 characters fit, 256 are `system_limit`.
            let long = "a".repeat(255);
            let list = bytes(&mut ctx, &long);
            assert!(list_to_atom_1(&mut ctx, &[list]).is_ok());
            let too_long = "a".repeat(256);
            let list = bytes(&mut ctx, &too_long);
            assert!(matches!(
                list_to_atom_1(&mut ctx, &[list]),
                Err(BeamError::SystemLimit)
            ));

            // `list_to_existing_atom/1` never creates.
            let existing = bytes(&mut ctx, "hello");
            assert_eq!(
                atoms.name(atom_val(
                    list_to_existing_atom_1(&mut ctx, &[existing]).expect("resolves")
                )),
                Some("hello")
            );
            let absent = bytes(&mut ctx, "brand_new");
            assert!(matches!(
                list_to_existing_atom_1(&mut ctx, &[absent]),
                Err(BeamError::BadArg)
            ));
            let not_a_list = ctx.int(1).expect("fits");
            assert!(matches!(
                list_to_existing_atom_1(&mut ctx, &[not_a_list]),
                Err(BeamError::BadArg)
            ));
        }

        // Without an interner an unknown name is reported (an M1 limitation).
        {
            let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
            let fresh = bytes(&mut ctx, "no_hook");
            assert!(matches!(
                list_to_atom_1(&mut ctx, &[fresh]),
                Err(BeamError::TermNotFound)
            ));
        }
    }

    // -- min/max and the numeric conversions ------------------------------

    #[test]
    fn min_and_max_use_the_arithmetic_order() {
        with_ctx(|ctx| {
            let one = ctx.int(1).expect("fits");
            let two = ctx.int(2).expect("fits");
            let one_f = ctx.make_float(1.0).expect("fits");
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let tuple = ctx.tuple(&[a]).expect("fits");

            assert_eq!(int_of(min_2(ctx, &[one, two]).expect("ok")), 1);
            assert_eq!(int_of(min_2(ctx, &[two, one]).expect("ok")), 1);
            assert_eq!(int_of(max_2(ctx, &[one, two]).expect("ok")), 2);
            assert_eq!(int_of(max_2(ctx, &[two, one]).expect("ok")), 2);
            // Equal in the arithmetic order: the first argument wins.
            assert_eq!(min_2(ctx, &[one_f, one]).expect("ok"), one_f);
            assert_eq!(min_2(ctx, &[one, one_f]).expect("ok"), one);
            assert_eq!(max_2(ctx, &[one, one_f]).expect("ok"), one);
            assert_eq!(max_2(ctx, &[one_f, one]).expect("ok"), one_f);
            // Any terms: atoms sort before tuples, numbers before both.
            assert_eq!(min_2(ctx, &[a, b]).expect("ok"), a);
            assert_eq!(max_2(ctx, &[a, b]).expect("ok"), b);
            assert_eq!(min_2(ctx, &[tuple, b]).expect("ok"), b);
            assert_eq!(max_2(ctx, &[tuple, b]).expect("ok"), tuple);
            assert_eq!(min_2(ctx, &[one, tuple]).expect("ok"), one);
        });
    }

    #[test]
    fn round_trunc_ceil_floor_and_float_match_the_oracle() {
        with_ctx(|ctx| {
            for (value, rounded, truncated, ceiling, flooring) in [
                (1.5f64, 2i64, 1i64, 2i64, 1i64),
                (-1.5, -2, -1, -1, -2),
                (0.5, 1, 0, 1, 0),
                (-0.5, -1, 0, 0, -1),
                (2.5, 3, 2, 3, 2),
                (3.5, 4, 3, 4, 3),
                (1.2, 1, 1, 2, 1),
                (-1.2, -1, -1, -1, -2),
                (2.0, 2, 2, 2, 2),
                (-3.0, -3, -3, -3, -3),
            ] {
                let term = ctx.make_float(value).expect("fits");
                assert_eq!(int_of(round_1(ctx, &[term]).expect("ok")), rounded, "round({value})");
                assert_eq!(int_of(trunc_1(ctx, &[term]).expect("ok")), truncated, "trunc({value})");
                assert_eq!(int_of(ceil_1(ctx, &[term]).expect("ok")), ceiling, "ceil({value})");
                assert_eq!(
                    int_of(floor_1(ctx, &[term]).expect("ok")),
                    flooring,
                    "floor({value})"
                );
                // `float/1` of a float is the same term.
                assert_eq!(float_1(ctx, &[term]).expect("ok"), term);
            }
            // An integer argument is returned unchanged by all five.
            for value in [0i64, 7, -3] {
                let term = ctx.int(value).expect("fits");
                assert_eq!(round_1(ctx, &[term]).expect("ok"), term);
                assert_eq!(trunc_1(ctx, &[term]).expect("ok"), term);
                assert_eq!(ceil_1(ctx, &[term]).expect("ok"), term);
                assert_eq!(floor_1(ctx, &[term]).expect("ok"), term);
                let converted = float_1(ctx, &[term]).expect("ok");
                assert!(access::is_float(converted));
                assert_eq!(access::float_value(converted), value as f64);
            }
            // Non-numbers are `badarg`.
            let atom = ctx.atom("a").expect("seeded");
            let empty = ctx.list(&[]).expect("fits");
            for bad in [atom, empty] {
                assert!(matches!(round_1(ctx, &[bad]), Err(BeamError::BadArg)));
                assert!(matches!(trunc_1(ctx, &[bad]), Err(BeamError::BadArg)));
                assert!(matches!(ceil_1(ctx, &[bad]), Err(BeamError::BadArg)));
                assert!(matches!(floor_1(ctx, &[bad]), Err(BeamError::BadArg)));
                assert!(matches!(float_1(ctx, &[bad]), Err(BeamError::BadArg)));
            }
            // A result past the small range is a limit (a bignum in real Erlang).
            let huge = ctx.make_float(1.0e30).expect("fits");
            assert!(matches!(round_1(ctx, &[huge]), Err(BeamError::SystemLimit)));
            // An infinite float (which Milestone-1 arithmetic can build) is never
            // turned into a saturated integer.
            let infinite = ctx.make_float(f64::INFINITY).expect("fits");
            assert!(matches!(trunc_1(ctx, &[infinite]), Err(BeamError::BadArg)));
        });
    }

    // -- tuple construction -----------------------------------------------

    #[test]
    fn make_tuple_matches_the_oracle() {
        with_ctx(|ctx| {
            let a = ctx.atom("a").expect("seeded");
            let x = ctx.atom("x").expect("seeded");
            let three = ctx.int(3).expect("fits");
            let zero = ctx.int(0).expect("fits");

            let tuple = make_tuple_2(ctx, &[three, a]).expect("ok");
            assert_eq!(access::tuple_size(tuple), 3);
            for i in 0..3 {
                assert_eq!(access::tuple_element(tuple, i), a);
            }
            let empty = make_tuple_2(ctx, &[zero, a]).expect("ok");
            assert_eq!(access::tuple_size(empty), 0);

            // `make_tuple/3` takes `{Index, Value}` pairs.
            let updates = ctx
                .list(&[
                    ctx.tuple(&[ctx.int(1).expect("fits"), a]).expect("fits"),
                    ctx.tuple(&[three, ctx.atom("b").expect("seeded")]).expect("fits"),
                ])
                .expect("fits");
            let tuple = make_tuple_3(ctx, &[three, x, updates]).expect("ok");
            assert_eq!(access::tuple_element(tuple, 0), a);
            assert_eq!(access::tuple_element(tuple, 1), x);
            assert_eq!(access::tuple_element(tuple, 2), ctx.atom("b").expect("seeded"));
            // No updates leaves the defaults.
            let none = ctx.list(&[]).expect("fits");
            let tuple = make_tuple_3(ctx, &[three, x, none]).expect("ok");
            assert_eq!(access::tuple_element(tuple, 2), x);

            // Bad arities.
            for bad in [
                ctx.int(-1).expect("fits"),
                ctx.make_float(2.0).expect("fits"),
                a,
                ctx.int(MAX_TUPLE_SIZE + 1).expect("fits"),
            ] {
                assert!(matches!(make_tuple_2(ctx, &[bad, a]), Err(BeamError::BadArg)));
                assert!(
                    matches!(make_tuple_3(ctx, &[bad, a, none]), Err(BeamError::BadArg)),
                    "make_tuple/3 arity {bad:?}"
                );
            }
            // Bad update lists: an out-of-range index, a 0 index, a non-tuple
            // element, a wrong-arity tuple, a non-integer index, a non-list.
            for update in [
                ctx.tuple(&[ctx.int(4).expect("fits"), a]).expect("fits"),
                ctx.tuple(&[zero, a]).expect("fits"),
                a,
                ctx.tuple(&[ctx.int(1).expect("fits"), a, x]).expect("fits"),
                ctx.tuple(&[ctx.make_float(1.0).expect("fits"), a]).expect("fits"),
            ] {
                let updates = ctx.list(&[update]).expect("fits");
                assert!(
                    matches!(make_tuple_3(ctx, &[three, x, updates]), Err(BeamError::BadArg)),
                    "make_tuple/3 update {update:?}"
                );
            }
            assert!(matches!(
                make_tuple_3(ctx, &[three, x, a]),
                Err(BeamError::BadArg)
            ));
            let improper = ctx
                .cons(
                    ctx.tuple(&[ctx.int(1).expect("fits"), a]).expect("fits"),
                    x,
                )
                .expect("fits");
            assert!(matches!(
                make_tuple_3(ctx, &[three, x, improper]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn append_element_matches_the_oracle() {
        with_ctx(|ctx| {
            let three = ctx.int(3).expect("fits");
            let tuple = ctx
                .tuple(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
                .expect("fits");
            let appended = append_element_2(ctx, &[tuple, three]).expect("ok");
            assert_eq!(access::tuple_size(appended), 3);
            assert_eq!(access::tuple_element(appended, 2), three);
            // The empty tuple grows to one element.
            let empty = ctx.tuple(&[]).expect("fits");
            let appended = append_element_2(ctx, &[empty, three]).expect("ok");
            assert_eq!(access::tuple_size(appended), 1);
            assert_eq!(access::tuple_element(appended, 0), three);
            // A non-tuple is `badarg`.
            let a = ctx.atom("a").expect("seeded");
            assert!(matches!(
                append_element_2(ctx, &[a, three]),
                Err(BeamError::BadArg)
            ));
        });
    }

    // -- raise/3 and node/0 -----------------------------------------------

    #[test]
    fn raise_sets_the_class_and_validates_the_stacktrace() {
        with_ctx(|ctx| {
            let reason = ctx.atom("foo").expect("seeded");
            let empty = ctx.list(&[]).expect("fits");
            let error = ctx.atom("error").expect("seeded");
            let exit = ctx.atom("exit").expect("seeded");
            let throw = ctx.atom("throw").expect("seeded");

            for (class, expected) in [
                (error, ExceptionClass::Error),
                (exit, ExceptionClass::Exit),
                (throw, ExceptionClass::Throw),
            ] {
                match raise_3(ctx, &[class, reason, empty]) {
                    Err(BeamError::Raise(raise)) => {
                        assert_eq!(raise.class, expected);
                        assert_eq!(raise.reason, beam_common::ExitReason::atom("foo"));
                    }
                    other => panic!("expected a raise, got {other:?}"),
                }
            }

            // A valid `{M,F,A,Location}` frame and a `{Fun,Args}` frame are
            // accepted; the reason still comes through.
            let frame = ctx
                .tuple(&[
                    ctx.atom("a").expect("seeded"),
                    ctx.atom("b").expect("seeded"),
                    ctx.int(1).expect("fits"),
                    empty,
                ])
                .expect("fits");
            let trace = ctx.list(&[frame]).expect("fits");
            assert!(matches!(
                raise_3(ctx, &[error, reason, trace]),
                Err(BeamError::Raise(_))
            ));

            // A non-atom class, a non-atom reason and malformed stacktraces.
            let one = ctx.int(1).expect("fits");
            assert!(matches!(
                raise_3(ctx, &[one, reason, empty]),
                Err(BeamError::BadArg)
            ));
            let bad_class = ctx.atom("a").expect("seeded");
            assert!(matches!(
                raise_3(ctx, &[bad_class, reason, empty]),
                Err(BeamError::BadArg)
            ));
            // Milestone-1 carries atom reasons only (see the module docs).
            let structured = ctx.tuple(&[reason]).expect("fits");
            assert!(matches!(
                raise_3(ctx, &[error, structured, empty]),
                Err(BeamError::BadArg)
            ));
            // The stacktrace must be a proper list of valid frames.
            for bad in [
                one,
                ctx.list(&[one]).expect("fits"),
                ctx.list(&[ctx.tuple(&[one, one]).expect("fits")]).expect("fits"),
                ctx.cons(frame, reason),
            ] {
                assert!(
                    matches!(raise_3(ctx, &[error, reason, bad]), Err(BeamError::BadArg)),
                    "stacktrace {bad:?}"
                );
            }
        });
    }

    #[test]
    fn node_is_nonode_at_nohost() {
        with_ctx(|ctx| {
            let node = node_0(ctx, &[]).expect("the atom is seeded");
            assert!(access::is_atom(node));
            assert_eq!(ctx.atoms().name(atom_val(node)), Some("nonode@nohost"));
        });
    }

    #[test]
    fn register_installs_every_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("integer_to_list", 1),
            ("integer_to_list", 2),
            ("list_to_integer", 1),
            ("list_to_integer", 2),
            ("float_to_list", 1),
            ("float_to_list", 2),
            ("list_to_float", 1),
            ("atom_to_list", 1),
            ("list_to_atom", 1),
            ("list_to_existing_atom", 1),
            ("min", 2),
            ("max", 2),
            ("round", 1),
            ("trunc", 1),
            ("float", 1),
            ("ceil", 1),
            ("floor", 1),
            ("make_tuple", 2),
            ("make_tuple", 3),
            ("append_element", 2),
            ("raise", 3),
            ("node", 0),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 22);
    }
}
