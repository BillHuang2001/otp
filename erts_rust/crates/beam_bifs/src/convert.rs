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

//! Pure `erlang` conversions and numeric/term helpers that need no binaries:
//! `integer_to_list/1,2`, `list_to_integer/1,2`, `float_to_list/1,2`,
//! `list_to_float/1`, `atom_to_list/1`, `list_to_atom/1`,
//! `list_to_existing_atom/1`, `min/2`, `max/2`, `round/1`, `trunc/1`, `float/1`,
//! `ceil/1`, `floor/1`, `make_tuple/2,3`, `append_element/2`, `raise/3`,
//! `node/0,1`.
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
//!   `1295`; `""`, `" 12"`, `"12.0"`, `"-"`, `"2"` base 2 ⇒ `badarg`. Each element
//!   is read as its **byte** (`big.c:4298,4331` narrows `unsigned_val` to the C
//!   `byte`), never required to *be* a byte: `[49,304]` spells `"10"` ⇒ `10`,
//!   `[304]` spells `"0"` ⇒ `0` and `[-207]` spells `"1"` ⇒ `1`.
//! * `float_to_list(1.5)` ⇒ `"1.50000000000000000000e+00"` (the default is
//!   `{scientific, SYS_DEFAULT_FLOAT_DECIMALS = 20}`); `{decimals, 2}` ⇒
//!   `"1.50"`; `{decimals, 0}` ⇒ `"2"` for `1.5` and `"3"` for `2.5` (the fixed
//!   formatter rounds halves away from zero); `compact` only shortens a
//!   `{decimals, N}` result (`float_to_list(1.0, [{decimals,2}, compact])` ⇒
//!   `"1.0"`), never the legacy/scientific form.
//! * `list_to_float("1.5")` ⇒ `1.5`, `("1.5e3")` ⇒ `1500.0`, `("1,5")` ⇒ `1.5`
//!   (the comma radix is accepted); `"1"`, `"1."`, `".5"`, `"1e3"`, `" 1.5"` and
//!   an overflow such as `"1.0e309"` ⇒ `badarg`.
//! * `float_to_list(F, [short])` ⇒ the oracle's shortest round-trip rendering:
//!   `(1.0)` ⇒ `"1.0"`, `(1.0e10)` ⇒ `"1.0e10"`, `(1.0e-4)` ⇒ `"0.0001"`,
//!   `(123456789.0)` ⇒ `"123456789.0"`, `(9007199254740992.0)` ⇒
//!   `"9.007199254740992e15"`, `(-0.0)` ⇒ `"-0.0"` ([`crate::numeric`] pins the
//!   notation, exponent-spelling and exact-tie rules).//! * `min(1.0, 1)` ⇒ `1.0` and `max(1, 1.0)` ⇒ `1`: both use the arithmetic term
//!   order and return the *first* argument when the two compare equal
//!   (`min(A,B) when A > B -> B; min(A,_) -> A.` in `erlang.erl`).
//! * `round(1.5)` ⇒ `2`, `round(-1.5)` ⇒ `-2`, `round(2.5)` ⇒ `3` (halves away
//!   from zero); `trunc(-1.7)` ⇒ `-1`; `ceil(-1.2)` ⇒ `-1`; `floor(-1.2)` ⇒ `-2`;
//!   `float(1)` ⇒ `1.0`; every one of them returns an integer argument unchanged.
//! * `make_tuple(3, a)` ⇒ `{a,a,a}`; `make_tuple(3, x, [{1,a},{3,c}])` ⇒
//!   `{a,x,c}` (the third argument is a list of `{Index, Value}` 2-tuples, *not*
//!   a list of values); an index outside `1..=Arity` ⇒ `badarg`;
//!   `append_element({1,2}, 3)` ⇒ `{1,2,3}`.
//! * `node()` ⇒ `nonode@nohost`, and `node(X)` ⇒ `nonode@nohost` for a **local**
//!   pid/port/reference. `node(X)` of an **external** (another node's)
//!   pid/port/reference returns the node atom embedded in the term; any other
//!   argument (`42`, `{a}`, `foo`, `1.0`, `<<>>`) is `badarg`.
//!
//! # Documented Milestone-1 gaps (never a silently wrong result)
//!
//! * **Integer values promote to bignums** for `integer_to_list/1,2`,
//!   `list_to_integer/1,2`, `float/1` and `round`/`trunc`/`ceil`/`floor` — a value
//!   outside the 60-bit small range becomes an arbitrary-precision bignum exactly
//!   as the oracle's does (`list_to_integer` of a 30-digit string, `round/1` of
//!   `1.0e30`). Other integer-producing BIFs may still report
//!   [`BeamError::SystemLimit`](beam_common::BeamError::SystemLimit).
//! * **The rendering buffer bound is the oracle's.** `float_to_list/2` is
//!   `badarg` for a rendering longer than the C's 255 characters *plus* its
//!   terminating NUL — unreachable for the `short` form (24 characters at most)
//!   and reachable only for an extreme `{decimals, N}`.
//! * **`list_to_atom` interprets its elements as Unicode code points** and needs
//!   an installed [`AtomInterner`](crate::context::AtomInterner); without one an
//!   unknown name reports
//!   [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound).

use core::cmp::Ordering;

use beam_codec::term as codec_term;
use beam_common::{AtomName, BeamError, ExceptionClass, Result, SourceLocation, StackFrame, StackTrace};
use beam_term::compare::cmp_arith;
use beam_term::{atom_val, is_byte, make_atom, signed_value, AtomTable, Eterm, NIL};

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::{
    badarg, raise_error, raise_exit, raise_term, raise_throw, system_limit, unsupported_bif,
};
use crate::numeric::{
    base_arg, format_scientific, integer_digits, parse_float, parse_float_options, parse_integer,
    render_float, small_arg, DEFAULT_FLOAT_DECIMALS,
};
use crate::print::proper_list;
use crate::reason::exit_reason_from_term;
use crate::registry::BifRegistry;

/// `MAX_ARITYVAL` (`../../erts/emulator/beam/erl_term.h`): the largest tuple
/// arity, also `ERTS_MAX_TUPLE_SIZE`.
const MAX_TUPLE_SIZE: i64 = (1 << 24) - 1;

/// The maximum atom name length in *characters* (erts-17.1); longer names are
/// `system_limit` (`list_to_atom`).
const MAX_ATOM_CHARS: usize = 255;

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

/// `integer_to_list(Integer)` — base 10.
fn integer_to_list_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let rendered = integer_digits(args[0], 10)?;
    ascii_list(ctx, &rendered)
}

/// `integer_to_list(Integer, Base)` for `2 =< Base =< 36`.
fn integer_to_list_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let base = base_arg(args[1])?;
    let rendered = integer_digits(args[0], base)?;
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

/// `byte c = unsigned_val(CAR(list))` (`big.c:4298,4331`): the element as the C
/// `byte` variable it is read into — the low eight bits of the two's-complement
/// 60-bit value, so an element outside `0..=255` (or a negative one) wraps.
fn element_byte(elem: Eterm) -> u8 {
    (signed_value(elem) as u64 & 0xFF) as u8
}

/// `digits_per_small(base)` (`big.c:4112-4127`): how many base-`base` digits fit a
/// small integer — the largest `n` with `base^n <= 2^59` (`SMALL_BITS - 1`, the
/// 59 magnitude bits the sign bit leaves). This *derives* the 64-bit row of the
/// C's `digits_per_small_lookup`; the tests below pin it against the published
/// table.
fn digits_per_small(base: u32) -> usize {
    const SMALL_MAGNITUDE: u128 = 1 << 59;
    let mut count = 0;
    let mut power = 1u128;
    while power * u128::from(base) <= SMALL_MAGNITUDE {
        power *= u128::from(base);
        count += 1;
    }
    count
}

/// How many digits `erts_internal:list_to_integer/2` reads as significant: it
/// consumes the optional sign and the leading zeros first (`big.c:4285-4313`), so
/// `"007"` counts as one digit.
fn significant_digits(bytes: &[u8]) -> usize {
    let digits = match bytes.first() {
        Some(b'-' | b'+') => &bytes[1..],
        _ => bytes,
    };
    let zeros = digits.iter().take_while(|&&byte| byte == b'0').count();
    digits.len() - zeros
}

/// `list_to_integer/1,2` — the `erlang.erl` wrapper (`erlang.erl:4117-4137`,
/// `4171-4190`) over `erts_internal:list_to_integer/2` (`big.c:4258-4355`).
///
/// Each element is read as its **byte** (see [`element_byte`]), never required to
/// *be* a byte: `[49,304]` spells `"10"` and is `10`, `[304]` spells `"0"` and is
/// `0`, `[-207]` spells `"1"` and is `1`.
///
/// The wrapper accepts only the internal BIF's `{Int, []}` shape — `no_integer`,
/// `not_a_list` and every `{Int, Rest}` with a non-empty rest are `error:badarg`,
/// which is what a non-small element, an improper list, a bad digit or trailing
/// garbage all produce. The one shape that re-reads the **whole** list is `big`:
/// the internal BIF stops after [`digits_per_small`]`+ 1` digits and the wrapper
/// then goes through `erlang:list_to_binary/1`, which refuses every element
/// outside `0..=255`. Pinned live (erts-17.1):
///
/// ```text
/// list_to_integer([49,304])                        ⇒ 10
/// list_to_integer([304])                           ⇒ 0
/// list_to_integer([-207])                          ⇒ 1
/// list_to_integer([304] ++ lists:duplicate(17,$0))    ⇒ 0
/// list_to_integer([49,304] ++ lists:duplicate(16,$0))  ⇒ ERROR badarg  (18 digits)
/// list_to_integer([49,304] ++ lists:duplicate(15,$0))  ⇒ 10000000000000000
/// ```
fn list_to_integer(ctx: &mut BifContext<'_>, list: Eterm, base: u32) -> Result<Eterm> {
    let elems = proper_list(list).ok_or_else(badarg)?;
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in &elems {
        // The scan breaks on `is_not_small` and leaves it behind, so a non-small
        // element is always a non-empty rest ⇒ `error:badarg`.
        if !access::is_small(*elem) {
            return Err(badarg());
        }
        bytes.push(element_byte(*elem));
    }
    let text: String = bytes.iter().map(|&byte| char::from(byte)).collect();
    let (negative, magnitude) = parse_integer(&text, base)?;
    // The `big` arm: a digit run longer than a small integer holds makes the
    // wrapper re-read the whole list as a byte string, so an element the
    // truncating scan would have accepted (a `304` standing for `$0`) is fatal.
    if significant_digits(&bytes) > digits_per_small(base)
        && elems
            .iter()
            .any(|elem| !(0..=255).contains(&signed_value(*elem)))
    {
        return Err(badarg());
    }
    arith::int_from_mag(ctx, negative, &magnitude)
}

// ---------------------------------------------------------------------------
// float_to_list/1,2
// ---------------------------------------------------------------------------

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

/// `list_to_float(String)` — `intlist_to_buf` (`utils.c:2827-2849`) copies the
/// list into a NUL-terminated `char` buffer handed to `sys_chars_to_double`.
///
/// Two C details decide the accept/reject set, and they differ from
/// [`list_to_integer`]'s scan on purpose:
///
/// * Every element must **be** a byte, i.e. `is_byte` (`utils.c:2840`): the
///   `byte c = unsigned_val(CAR(list))` store is guarded by `!is_byte` first, so
///   an element outside `0..=255` (or a negative one) is `error:badarg`. Unlike
///   `list_to_integer`, this reader must **not** truncate through
///   [`element_byte`]: `list_to_float([49,46,304])` is `badarg`, never `1.0`.
/// * The buffer is a C string, so an embedded NUL element terminates the text —
///   while the elements *after* it are still byte-checked (`intlist_to_buf` walks
///   the whole list before `sys_chars_to_double` ever sees it).
fn list_to_float_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let elems = proper_list(args[0]).ok_or_else(badarg)?;
    let mut bytes = Vec::with_capacity(elems.len());
    for elem in &elems {
        if !is_byte(*elem) {
            return Err(badarg());
        }
        // `is_byte` proves the value is in `0..=255`, so the cast is exact.
        bytes.push(signed_value(*elem) as u8);
    }
    // The embedded-NUL truncation: `list_to_float([49,46,53,0,50])` ⇒ `1.5`.
    let text: String = bytes
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| char::from(byte))
        .collect();
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

/// A rounded/truncated integer result, with `round` the rounding operation
/// (`f64::round`/`f64::trunc`/`f64::ceil`/`f64::floor`).
///
/// An **integer** argument is returned unchanged (the oracle's
/// `round(576460752303423487)` ⇒ `576460752303423487`; going through `f64` would
/// round it to `576460752303423488`). A float result within the 60-bit small
/// range stays a small integer and anything larger becomes the exact bignum the
/// oracle returns (`round(1.0e30)`), so the promotion reaches far past `i64`.
///
/// The value must be finite (an infinite or NaN float cannot exist in Erlang —
/// Milestone-1 arithmetic can still build one, and it is `badarg` here rather
/// than a saturated, silently wrong integer).
///
/// # Errors
///
/// [`BeamError::BadArg`](beam_common::BeamError::BadArg) for a non-finite float
/// or a non-number argument.
fn rounded_result(
    ctx: &mut BifContext<'_>,
    term: Eterm,
    round: fn(f64) -> f64,
) -> Result<Eterm> {
    if !access::is_number(term) {
        return Err(badarg());
    }
    if access::is_small(term) || access::is_big(term) {
        return Ok(term);
    }
    let value = access::float_value(term);
    if !value.is_finite() {
        return Err(badarg());
    }
    arith::int_from_f64(ctx, round(value))
}

/// `round(Number)` — halves away from zero (`round(2.5)` is `3`).
fn round_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    rounded_result(ctx, args[0], f64::round)
}

/// `trunc(Number)` — toward zero (`trunc(-1.7)` is `-1`).
fn trunc_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    rounded_result(ctx, args[0], f64::trunc)
}

/// `ceil(Number)`.
fn ceil_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    rounded_result(ctx, args[0], f64::ceil)
}

/// `floor(Number)`.
fn floor_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    rounded_result(ctx, args[0], f64::floor)
}

/// `float(Number)` — an integer becomes a float; a float is returned as is.
///
/// An integer of arbitrary precision converts to the nearest `f64`, ties to even
/// (the C `big_to_double`), and a magnitude the oracle refuses is `badarg`
/// (`float(1 bsl 2000)`).
fn float_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    match args[0] {
        term if access::is_float(term) => Ok(term),
        term if access::is_integer(term) => ctx.make_float(arith::integer_to_f64(term)?),
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
// raise/3, node/0 and node/1
// ---------------------------------------------------------------------------

/// The exception class of `raise/3`'s first argument, which the C matches by
/// identity against `am_error`/`am_exit`/`am_throw` (`bif.c:1563-1572`); `None`
/// for every other term.
fn exception_class(atoms: &AtomTable, class: Eterm) -> Option<ExceptionClass> {
    if !access::is_atom(class) {
        return None;
    }
    match atoms.name(atom_val(class)) {
        Some("error") => Some(ExceptionClass::Error),
        Some("exit") => Some(ExceptionClass::Exit),
        Some("throw") => Some(ExceptionClass::Throw),
        _ => None,
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

// ---------------------------------------------------------------------------
// raise/3's stacktrace argument → StackTrace
// ---------------------------------------------------------------------------

/// The name of the atom `term`, as a term-independent [`AtomName`].
///
/// # Errors
///
/// [`unsupported_bif`] for a non-atom, or for an atom whose index `atoms` does not
/// name. A name the table lacks cannot be rendered without inventing one, and the
/// frame would then be a wrong frame.
fn frame_atom_name(atoms: &AtomTable, term: Eterm) -> Result<AtomName> {
    if !access::is_atom(term) {
        return Err(unsupported_bif());
    }
    match atoms.name(atom_val(term)) {
        Some(name) => Ok(AtomName::new(name.to_string())),
        None => Err(unsupported_bif()),
    }
}

/// The value of the `{Key, Value}` pair at the head of a frame location: the pair
/// must be a 2-tuple whose first element is exactly the atom `key`.
///
/// # Errors
///
/// [`unsupported_bif`] for anything else — a non-tuple, a wrong arity, a non-atom
/// key, or a key naming some other atom (the location's keys are `file` and
/// `line`, and the oracle accepts any term here while a [`SourceLocation`] can only
/// express those two).
fn location_pair_value(atoms: &AtomTable, pair: Eterm, key: &str) -> Result<Eterm> {
    if !access::is_tuple(pair) || access::tuple_size(pair) != 2 {
        return Err(unsupported_bif());
    }
    let tag = access::tuple_element(pair, 0);
    if !access::is_atom(tag) || atoms.name(atom_val(tag)) != Some(key) {
        return Err(unsupported_bif());
    }
    Ok(access::tuple_element(pair, 1))
}

/// The [`SourceLocation`] a frame's `Location` term names, or `None` for the empty
/// list the oracle normalizes an absent location to.
///
/// A location is exactly `[{file, FileName}, {line, Line}]` — those two pairs, in
/// that order — where `FileName` is a **char list** of Unicode scalar values
/// (`{file,"lists.erl"}`, never an atom) and `Line` an integer in `0..=u32::MAX`.
///
/// # Errors
///
/// [`unsupported_bif`] for a location that passes [`is_location`] (any list does)
/// but is not that exact shape: an atom/other file name, a missing or extra pair,
/// reversed order, a non-integer or out-of-`u32`-range `line`, or a char list
/// element that is not a Unicode scalar value. The oracle echoes every such shape
/// verbatim, but a [`SourceLocation`] cannot express it and a truncated or invented
/// location would be a wrong result.
fn decode_location(atoms: &AtomTable, location: Eterm) -> Result<Option<SourceLocation>> {
    if location == NIL {
        return Ok(None);
    }
    let pairs = proper_list(location).ok_or_else(unsupported_bif)?;
    if pairs.len() != 2 {
        return Err(unsupported_bif());
    }

    let name = location_pair_value(atoms, pairs[0], "file")?;
    let chars = proper_list(name).ok_or_else(unsupported_bif)?;
    let mut file = String::with_capacity(chars.len());
    for ch in chars {
        let code = if access::is_small(ch) { signed_value(ch) } else { -1 };
        let code = u32::try_from(code)
            .ok()
            .and_then(char::from_u32)
            .ok_or_else(unsupported_bif)?;
        file.push(code);
    }

    let line = location_pair_value(atoms, pairs[1], "line")?;
    let line = if access::is_small(line) {
        u32::try_from(signed_value(line)).ok()
    } else {
        None
    }
    .ok_or_else(unsupported_bif)?;

    Ok(Some(SourceLocation {
        file: AtomName::new(file),
        line,
    }))
}

/// The [`StackFrame`] one validated stacktrace frame names.
///
/// Only `{Module, Function, Arity}` and `{Module, Function, Arity, Location}` are
/// representable: `Module` and `Function` are atoms (their names are read from
/// `atoms`), the 3rd element is an **integer in `0..=255`** ([`StackFrame`] carries
/// a `u8` arity, and the loader/engine render an arity, never an argument list) and
/// the location is exactly what [`decode_location`] accepts.
///
/// # Errors
///
/// [`unsupported_bif`] for every frame the oracle accepts but a [`StackFrame`]
/// cannot express — an `erl_eval`-style `{Fun, Args[, Location]}` **call frame**
/// (it names no module/function), a 3rd element that is an argument list or any
/// other non-`0..=255` value (`-1`, `256`, a float), an unnameable module/function,
/// or a location [`decode_location`] rejects. Never a truncated or invented frame.
fn decode_stacktrace_frame(atoms: &AtomTable, frame: Eterm) -> Result<StackFrame> {
    // Restates `decode_stacktrace`'s precondition: every frame was accepted by
    // `is_stacktrace_frame` before this runs, so this is the frame shape guard that
    // makes the tuple access below safe.
    if !is_stacktrace_frame(frame) {
        return Err(unsupported_bif());
    }
    let (module_term, function_term, arity_term, location_term) = match access::tuple_size(frame) {
        3 => {
            let first = access::tuple_element(frame, 0);
            if access::is_function(first) {
                // `{Fun, Args, Location}` — a call frame.
                return Err(unsupported_bif());
            }
            (
                first,
                access::tuple_element(frame, 1),
                access::tuple_element(frame, 2),
                NIL,
            )
        }
        4 => (
            access::tuple_element(frame, 0),
            access::tuple_element(frame, 1),
            access::tuple_element(frame, 2),
            access::tuple_element(frame, 3),
        ),
        // `{Fun, Args}` — the remaining accepted shape, a call frame too.
        _ => return Err(unsupported_bif()),
    };
    let module = frame_atom_name(atoms, module_term)?;
    let function = frame_atom_name(atoms, function_term)?;
    let arity = if access::is_small(arity_term) {
        u8::try_from(signed_value(arity_term)).ok()
    } else {
        None
    }
    .ok_or_else(unsupported_bif)?;
    Ok(StackFrame {
        module,
        function,
        arity,
        location: decode_location(atoms, location_term)?,
    })
}

/// Decodes a **validated** `raise/3` stacktrace argument into the frames the engine
/// renders verbatim.
///
/// The empty list decodes to the **empty** [`StackTrace`], which leaves the engine's
/// supplied-trace latch unarmed (so `raise(_, _, [])` keeps its derived stacktrace);
/// every other element must decode, and one that cannot is the typed gap (see
/// [`decode_stacktrace_frame`]).
///
/// # Errors
///
/// [`unsupported_bif`] for an unrepresentable frame; the trace itself has already
/// passed [`is_stacktrace`], so a non-list never reaches the `proper_list` failure.
fn decode_stacktrace(atoms: &AtomTable, stacktrace: Eterm) -> Result<StackTrace> {
    let frames = proper_list(stacktrace).ok_or_else(unsupported_bif)?;
    let mut decoded = StackTrace::new();
    for frame in frames {
        decoded.push(decode_stacktrace_frame(atoms, frame)?);
    }
    Ok(decoded)
}

/// Attaches the decoded `stacktrace` to the raise `err` carries.
///
/// Both callers in [`raise_3`] pass a raise (`raise_term` and the three `raise_*`
/// builders always build one), so the [`BeamError::Raise`] arm is the one taken; any
/// other error is returned unchanged rather than wrapped in a fabricated raise.
fn attach_stacktrace(err: BeamError, stacktrace: StackTrace) -> BeamError {
    match err {
        BeamError::Raise(raise) => BeamError::Raise(raise.with_stacktrace(stacktrace)),
        other => other,
    }
}

/// `raise(Class, Reason, Stacktrace)` (`raise_3`, `bif.c:1549-1696`).
///
/// The reason can be **any term**: an atom reason keeps its exact
/// [`ExitReason`] (with no live slot), and a non-atom one is raised as
/// [`ExitReason::Structured`] carrying the argument term itself in the
/// [`Raise::live_reason`](beam_common::Raise::live_reason) slot (see
/// [`crate::reason`]). Pinned live (erts-17.1):
/// `raise(error, {a,b}, [])` ⇒ `error:{a,b}`, `raise(exit, {shutdown,x}, [])` ⇒
/// `exit:{shutdown,x}`, `raise(throw, {a}, [])` ⇒ `throw:{a}`, and
/// `raise(error, foo, [])` ⇒ `error:foo`.
///
/// Unlike every other BIF here, an invalid argument shape is **returned**, not
/// raised: `bif.c` ends each failure path in `return am_badarg` and `erlang.erl`
/// documents it ("it has no return value unless the arguments are invalid, in
/// which case the function returns the error reason `badarg`"). Pinned live:
///
/// ```text
/// try erlang:raise(foo, foo, [])     catch C:R -> {C,R} end  ⇒ badarg  (*returned*)
/// try erlang:raise(1, foo, [])       catch C:R -> {C,R} end  ⇒ badarg  (*returned*)
/// try erlang:raise(error, foo, [42]) catch C:R -> {C,R} end  ⇒ badarg  (*returned*)
/// try erlang:raise(error, foo, x)    catch C:R -> {C,R} end  ⇒ badarg  (*returned*)
/// ```
///
/// A **valid** trace is decoded into the returned raise's
/// [`StackTrace`](beam_common::StackTrace) ([`decode_stacktrace`]), which the engine
/// renders verbatim instead of deriving the frames its raise snapshot would
/// (`build_stacktrace` returns a supplied trace unchanged). The empty list decodes
/// to the empty trace, so `raise(_, _, [])` is unchanged. Pinned live (erts-17.1):
///
/// ```text
/// raise(error, foo, [{lists,map,2,[{file,"lists.erl"},{line,1234}]}])
///   ⇒ the trace is echoed verbatim
/// raise(error, foo, [{m,f,1}])  /  [{m,f,1,[]}]  ⇒  [{m,f,1,[]}]
/// ```
///
/// A trace element the oracle echoes but a
/// [`StackFrame`](beam_common::StackFrame) cannot express (an `erl_eval`-style
/// `{Fun,Args}` **call** frame, an argument list or an out-of-`0..=255` value in the
/// arity slot, a location that is not exactly `[{file,CharList},{line,Integer}]`) is
/// raised as [`unsupported_bif`], never as the returned `badarg`: the argument *is*
/// valid, so it is a capability gap, and an invented frame would be a wrong result.
fn raise_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // The class is resolved first (`bif.c:1563-1572`), then the stacktrace shape
    // (`bif.c:1580-1624`); either failure is the returned `badarg` atom. The class
    // atom is resolved through `intern_atom`, so the runtime's `AtomInterner`
    // supplies it for a module whose table does not carry it.
    let Some(class) = exception_class(ctx.atoms(), args[0]) else {
        return ctx.intern_atom("badarg");
    };
    if !is_stacktrace(args[2]) {
        return ctx.intern_atom("badarg");
    }
    // The 3rd argument is a *valid* stacktrace: decode it into the frames the engine
    // renders verbatim. Encoding stops here for an element no frame can express.
    let stacktrace = decode_stacktrace(ctx.atoms(), args[2])?;
    let err = if !access::is_atom(args[1]) {
        // `c_p->fvalue = value` (`bif.c:1564-1571`): a non-atom reason is the
        // argument term itself, whatever the class.
        raise_term(class, args[1])
    } else {
        let reason = exit_reason_from_term(args[1], ctx.atoms());
        match class {
            ExceptionClass::Error => raise_error(reason),
            ExceptionClass::Exit => raise_exit(reason),
            ExceptionClass::Throw => raise_throw(reason),
            // `ExceptionClass` is `#[non_exhaustive]`; `exception_class` above only
            // ever produces the three classes handled here, so this arm is
            // unreachable (defaulting to `error` keeps it total without a panic).
            _ => raise_error(reason),
        }
    };
    Err(attach_stacktrace(err, stacktrace))
}

/// The atom naming the **local** node — the node every internal pid/port/ref
/// belongs to.
///
/// Ground truth `erl_node_container_utils.h:74-76,93`: an internal node container
/// has an *implicit* reference to this node, so `node_container_node_name(x)` is
/// `internal_node_name(x) = erts_this_node->sysname`.
///
/// Resolved through [`BifContext::local_node`] (the optional
/// [`LocalNode`](crate::context::LocalNode) seam), which falls back to the
/// Milestone-1 runtime's fixed, non-distributed identity
/// `(nonode@nohost, creation 0)` when no hook is installed. `node/0` and the local
/// arm of `node/1` share this helper so the two can never disagree.
///
/// # Errors
///
/// [`BeamError::TermNotFound`](beam_common::BeamError::TermNotFound) when no hook
/// is installed and the atom table does not hold `nonode@nohost` (an unseeded
/// table), or when an installed hook reports the identity unknown. Every node of
/// the oracle is nameable, so this can only mean the Rust runtime is not fully
/// wired; fabricating an atom here would be a wrong result.
fn local_node_atom(ctx: &mut BifContext<'_>) -> Result<Eterm> {
    match ctx.local_node() {
        Some((node, _creation)) => Ok(make_atom(node)),
        None => Err(BeamError::TermNotFound),
    }
}

/// `node()` — the local node's name atom (`dist.c:6253-6256`:
/// `BIF_RET(erts_this_dist_entry->sysname)`).
///
/// Milestone-1 never starts distribution, so the identity is the fixed
/// `nonode@nohost` (through the [`LocalNode`](crate::context::LocalNode) seam; see
/// [`local_node_atom`]).
///
/// # Errors
///
/// As [`local_node_atom`].
fn node_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    local_node_atom(ctx)
}

/// `node(X)` — the node atom of the pid, port or reference `X`
/// (`dist.c:6242-6248`):
///
/// ```c
/// if (is_not_node_container(BIF_ARG_1)) BIF_ERROR(BIF_P, BADARG);
/// BIF_RET(node_container_node_name(BIF_ARG_1));
/// ```
///
/// `is_node_container(x) = is_internal(x) || is_external(x)` (`is_internal` =
/// internal pid/port/ref, `erl_node_container_utils.h:86-91`). An **internal**
/// container belongs to the local node ([`local_node_atom`]); for an **external**
/// one `node_container_node_name(x)` is `external_node_name(x)` — the node atom
/// stored in the term's frozen `[header, node, next, data]` block
/// (`erl_node_container_utils.h:74-76,94`), which
/// `beam_term::thing::external_{pid,port,ref}_parts` reads back and the safe
/// `beam_codec::term` wrappers expose.
///
/// Pinned to the oracle (OTP 29.1.1 / erts-17.1): `node(self())`,
/// `node(list_to_pid("<0.1.0>"))`, `node(make_ref())` and a port from
/// `open_port` are all `nonode@nohost`; `binary_to_term` of a `NEW_PID_EXT`
/// (`88`)/`NEW_PORT_EXT` (`89`)/`NEWER_REFERENCE_EXT` (`90`) naming `remote@host`
/// is `remote@host`; `42`, `{a}`, `foo`, `1.0` and `<<>>` are `badarg`.
///
/// # Errors
///
/// [`BeamError::BadArg`] when `X` is not a pid, port or reference; otherwise as
/// [`local_node_atom`].
fn node_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let term = args[0];
    if access::is_internal_pid(term)
        || access::is_internal_port(term)
        || access::is_internal_ref(term)
    {
        return local_node_atom(ctx);
    }
    // The `external_*_parts` readers are the safe inverses of the writers in
    // `beam_term::thing`; the node slot (`word 1`) holds the sysname atom term
    // itself (the documented M1 substitution for the C's node-registry pointer).
    if access::is_external_pid(term) {
        let (node, _, _, _) = codec_term::external_pid_parts(term)?;
        return Ok(node);
    }
    if access::is_external_port(term) {
        let (node, _, _) = codec_term::external_port_parts(term)?;
        return Ok(node);
    }
    if access::is_external_ref(term) {
        let (node, _, _) = codec_term::external_ref_parts(term)?;
        return Ok(node);
    }
    Err(badarg())
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
    reg.register("erlang", "node", 1, node_1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{Atom, AtomTable};
    use std::cell::RefCell;
    use std::rc::Rc;

    /// An [`AtomInterner`](crate::context::AtomInterner) over its own table; the
    /// table *is* the shared atom table in a real runtime (see `context.rs`). The
    /// `Rc<RefCell<..>>` lets the test inspect the interned names while the
    /// context still holds the (mutable) interner hook.
    struct Interner {
        table: Rc<RefCell<AtomTable>>,
    }

    impl crate::context::AtomInterner for Interner {
        fn intern(&mut self, name: &str) -> Atom {
            self.table.borrow_mut().intern(name)
        }
    }

    /// The names the tests build terms from; `nonode@nohost`, `error`, `exit`
    /// and `throw` are the ones the BIFs look up themselves, `badarg` is the atom
    /// `raise/3` returns for a bad argument shape, `normal` a well-known
    /// [`beam_common::ExitReason`], and
    /// `decimals`/`scientific`/`compact`/`short` are the `float_to_list`
    /// option tags.
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
            "foo",
            "nonode@nohost",
            "error",
            "exit",
            "throw",
            "badarg",
            "normal",
            "decimals",
            "scientific",
            "compact",
            "short",
        ]);
        atoms
    }

    fn heap() -> Heap {
        // Big enough for the longest list a test builds (two 256-character
        // byte lists plus their surroundings).
        Heap::new(8192).expect("test heap")
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
                ("-73", 36, -255),
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
            // `304` is read as its byte `$0`: "10" ⇒ 10 (the oracle truncates, it
            // does not require `0..=255`).
            assert_eq!(
                int_of(list_to_integer_1(ctx, &[big]).expect("parses")),
                10
            );
            // Every element is narrowed to its low byte, a negative one included.
            for (elements, expected) in [
                (vec![304i64], 0i64),
                (vec![305], 1),
                (vec![-207], 1),
                (vec![48, 49, 304], 10),
                (vec![304, 49], 1),
                (vec![304, 48, 48], 0),
                (vec![256 + 49, 256 + 50], 12),
                (vec![48], 0),
                (vec![48, 48], 0),
            ] {
                let mut terms = Vec::new();
                for value in &elements {
                    terms.push(ctx.int(*value).expect("fits"));
                }
                let list = ctx.list(&terms).expect("fits");
                assert_eq!(
                    int_of(list_to_integer_1(ctx, &[list]).expect("parses")),
                    expected,
                    "list_to_integer({elements:?})"
                );
            }
            // A byte whose low eight bits are not a digit for the base is
            // `badarg` (`[-1]` reads `255`), as is a doubled sign.
            for elements in [vec![-1i64], vec![-48], vec![43, 45, 49]] {
                let mut terms = Vec::new();
                for value in &elements {
                    terms.push(ctx.int(*value).expect("fits"));
                }
                let list = ctx.list(&terms).expect("fits");
                assert!(
                    matches!(list_to_integer_1(ctx, &[list]), Err(BeamError::BadArg)),
                    "list_to_integer({elements:?})"
                );
            }
            let maybe_float = ctx.make_float(1.0).expect("fits");
            let with_float = ctx
                .list(&[ctx.int(49).expect("fits"), maybe_float])
                .expect("fits");
            assert!(matches!(
                list_to_integer_1(ctx, &[with_float]),
                Err(BeamError::BadArg)
            ));
            let bignum =
                beam_codec::term::make_big(ctx.heap(), false, &[1, 0]).expect("a bignum");
            let with_bignum = ctx.list(&[ctx.int(49).expect("fits"), bignum]).expect("fits");
            assert!(matches!(
                list_to_integer_1(ctx, &[with_bignum]),
                Err(BeamError::BadArg)
            ));
            // The `big` arm of the `erlang.erl` wrapper re-reads the whole list
            // through `list_to_binary/1` (0..=255 only) once the digit run passes
            // `digits_per_small(10) = 17`.
            let eighteen = 18;
            let with_tail: Vec<Eterm> = (0..eighteen)
                .map(|i| ctx.int(if i == 0 { 49 } else { 48 }).expect("fits"))
                .collect();
            let all_digits = ctx.list(&with_tail).expect("fits");
            assert_eq!(
                int_of(list_to_integer_1(ctx, &[all_digits]).expect("parses")),
                100_000_000_000_000_000
            );
            let mut wide = with_tail.clone();
            wide[1] = ctx.int(304).expect("fits");
            let wide = ctx.list(&wide).expect("fits");
            assert!(
                matches!(list_to_integer_1(ctx, &[wide]), Err(BeamError::BadArg)),
                "18 digits with a 304 element"
            );
            // A value past the small range is the exact bignum the oracle builds.
            let huge = bytes(ctx, "99999999999999999999999999");
            let term = list_to_integer_1(ctx, &[huge]).expect("promotes");
            assert!(access::is_big(term), "expected a bignum");
            let (negative, digits) = beam_codec::term::big_parts(term).expect("a bignum");
            assert!(!negative);
            assert_eq!(
                crate::numeric::mag_to_digits(negative, digits, 10),
                "99999999999999999999999999"
            );
        });
    }

    /// `digits_per_small/1` is the 64-bit `digits_per_small_lookup` row of
    /// `big.c:4120-4123` (bases 2..=36) — the derivation is pinned against the C's
    /// published table.
    #[test]
    fn digits_per_small_matches_the_c_lookup_table() {
        const C_TABLE: [usize; 35] = [
            59, 37, 29, 25, 22, 21, 19, 18, 17, 17, 16, 15, 15, 15, 14, 14, 14, 13, 13, 13, 13, 13,
            12, 12, 12, 12, 12, 12, 12, 11, 11, 11, 11, 11, 11,
        ];
        for base in 2..=36u32 {
            assert_eq!(
                digits_per_small(base),
                C_TABLE[base as usize - 2],
                "digits_per_small({base})"
            );
            // The definition itself: `base^n` fits a small, `base^(n+1)` does not.
            let n = digits_per_small(base) as u32;
            assert!(u128::from(base).pow(n) <= 1 << 59);
            assert!(u128::from(base).pow(n + 1) > 1 << 59);
        }
        // `0x0007` counts one significant digit; the sign and the zeros do not.
        assert_eq!(significant_digits(b"007"), 1);
        assert_eq!(significant_digits(b"-007"), 1);
        assert_eq!(significant_digits(b"+0"), 0);
        assert_eq!(significant_digits(b"0"), 0);
        assert_eq!(significant_digits(b"1000"), 4);
    }

    // -- float_to_list / list_to_float ------------------------------------

    #[test]
    // `3.14159` is a six-digit *float* (the oracle renders
    // `"3.14158999999999988262e+00"`), not `f64::consts::PI` — the literal is the
    // subject under test, so it must not be swapped for the constant.
    #[allow(clippy::approx_constant)]
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

            // `short` is the shortest round-trip form (the oracle's `[short]`).
            let short_opts = options(ctx, &[short]);
            for (value, expected) in [
                (1.0f64, "1.0"),
                (1.0e10, "1.0e10"),
                (1.0e-4, "0.0001"),
                (1000.0, "1.0e3"),
                (123456789.0, "123456789.0"),
                (9_007_199_254_740_992.0, "9.007199254740992e15"),
                (0.0, "0.0"),
                (-0.0, "-0.0"),
                (1.0 / 3.0, "0.3333333333333333"),
            ] {
                let value_term = ctx.make_float(value).expect("fits");
                let rendered = float_to_list_2(ctx, &[value_term, short_opts]).expect("renders");
                assert_eq!(spell(rendered), expected, "float_to_list({value:?}, [short])");
            }
            // A later option still wins over `short`.
            let d_two = decimals(ctx, 2);
            let short_then_decimals = options(ctx, &[short, d_two]);
            assert_eq!(
                spell(float_to_list_2(ctx, &[value, short_then_decimals]).expect("ok")),
                "1.50"
            );
            let s_one = scientific(ctx, 1);
            let scientific_then_short = options(ctx, &[s_one, short]);
            assert_eq!(
                spell(float_to_list_2(ctx, &[value, scientific_then_short]).expect("ok")),
                "1.5"
            );
            // Unsupported/invalid options.
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
            let two_f = ctx.make_float(2.0).expect("fits");
            let float_precision = ctx
                .tuple(&[
                    beam_term::make_atom(atoms().lookup("decimals").expect("seeded")),
                    two_f,
                ])
                .expect("fits");
            let opts = options(ctx, &[float_precision]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, opts]),
                Err(BeamError::BadArg)
            ));
            let minus_one = decimals(ctx, -1);
            let negative = options(ctx, &[minus_one]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, negative]),
                Err(BeamError::BadArg)
            ));
            // The oracle's rendering buffer is 256 bytes: 255 characters fit.
            let d253 = decimals(ctx, 253);
            let fits = options(ctx, &[d253]);
            assert_eq!(
                spell(float_to_list_2(ctx, &[value, fits]).expect("fits")).len(),
                255
            );
            let d254 = decimals(ctx, 254);
            let overflow = options(ctx, &[d254]);
            assert!(matches!(
                float_to_list_2(ctx, &[value, overflow]),
                Err(BeamError::BadArg)
            ));
            let s249 = scientific(ctx, 249);
            let fits = options(ctx, &[s249]);
            assert_eq!(
                spell(float_to_list_2(ctx, &[value, fits]).expect("fits")).len(),
                255
            );
            let s250 = scientific(ctx, 250);
            let overflow = options(ctx, &[s250]);
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
                let list = bytes(ctx, text);
                let term = list_to_float_1(ctx, &[list]).expect("parses");
                assert!(access::is_float(term), "{text:?} is not a float");
                assert_eq!(access::float_value(term), expected, "list_to_float({text:?})");
            }
            // Underflow to zero is accepted (the C `errno == ERANGE` special case).
            let tiny = bytes(ctx, "1.0e-400");
            let underflow = list_to_float_1(ctx, &[tiny]).expect("parses");
            assert_eq!(access::float_value(underflow), 0.0);

            for text in ["", "1", "1.", ".5", "1e3", "1.5e", "1.5.5", " 1.5", "abc", "1.5e309"] {
                let list = bytes(ctx, text);
                assert!(
                    matches!(list_to_float_1(ctx, &[list]), Err(BeamError::BadArg)),
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
            table: Rc::new(RefCell::new(AtomTable::new())),
        };
        let interned = Rc::clone(&interner.table);
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
            assert_eq!(interned.borrow().name(atom_val(term)), Some("brand_new"));

            // The empty atom `''` is legal, and code points round-trip.
            let empty = ctx.list(&[]).expect("fits");
            let term = list_to_atom_1(&mut ctx, &[empty]).expect("interns");
            assert_eq!(interned.borrow().name(atom_val(term)), Some(""));
            let points = ctx
                .list(&[ctx.int(255).expect("fits"), ctx.int(300).expect("fits")])
                .expect("fits");
            let term = list_to_atom_1(&mut ctx, &[points]).expect("interns");
            let name = interned.borrow().name(atom_val(term)).expect("interned").to_string();
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
            // A result past the small range is the exact bignum the oracle builds
            // (`round(1.0e18)` ⇒ `1000000000000000000`, `round(1.0e30)` ⇒
            // `1000000000000000019884624838656`).
            for (value, expected) in [
                (1.0e18f64, "1000000000000000000"),
                (1.0e30, "1000000000000000019884624838656"),
                (-1.0e30, "-1000000000000000019884624838656"),
            ] {
                let term = ctx.make_float(value).expect("fits");
                for result in [
                    round_1(ctx, &[term]).expect("round"),
                    trunc_1(ctx, &[term]).expect("trunc"),
                    ceil_1(ctx, &[term]).expect("ceil"),
                    floor_1(ctx, &[term]).expect("floor"),
                ] {
                    assert!(access::is_big(result), "{value}: expected a bignum");
                    let (negative, digits) =
                        beam_codec::term::big_parts(result).expect("a bignum");
                    assert_eq!(
                        crate::numeric::mag_to_digits(negative, digits, 10),
                        expected,
                        "{value}"
                    );
                }
            }
            // A small integer past `2^53` is returned *unchanged*, not rounded
            // through `f64` (`round(576460752303423487)` in the oracle).
            let odd = ctx.int(576_460_752_303_423_487).expect("fits");
            assert_eq!(round_1(ctx, &[odd]).expect("ok"), odd);
            assert_eq!(trunc_1(ctx, &[odd]).expect("ok"), odd);
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
            let first = ctx.tuple(&[ctx.int(1).expect("fits"), a]).expect("fits");
            let second = ctx.tuple(&[three, ctx.atom("b").expect("seeded")]).expect("fits");
            let updates = ctx.list(&[first, second]).expect("fits");
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
            let one_f = ctx.make_float(1.0).expect("fits");
            for update in [
                ctx.tuple(&[ctx.int(4).expect("fits"), a]).expect("fits"),
                ctx.tuple(&[zero, a]).expect("fits"),
                a,
                ctx.tuple(&[ctx.int(1).expect("fits"), a, x]).expect("fits"),
                ctx.tuple(&[one_f, a]).expect("fits"),
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
            let pair = ctx.tuple(&[ctx.int(1).expect("fits"), a]).expect("fits");
            let improper = ctx.cons(pair, x).expect("fits");
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

            // A bad class or a malformed stacktrace is the *returned* atom
            // `badarg` (`bif.c:1694-1695`), never a raise.
            let badarg = ctx.atom("badarg").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let bad_class = ctx.atom("a").expect("seeded");
            for args in [
                [one, reason, empty],
                [bad_class, reason, empty],
                [bad_class, one, empty],
            ] {
                assert_eq!(
                    raise_3(ctx, &args).expect("the badarg atom is returned"),
                    badarg,
                    "raise/3 {args:?}"
                );
            }
            // The stacktrace must be a proper list of valid frames.
            let two_tuple = ctx.tuple(&[one, one]).expect("fits");
            let nested = ctx.list(&[two_tuple]).expect("fits");
            let improper_trace = ctx.cons(frame, reason).expect("fits");
            for bad in [one, ctx.list(&[one]).expect("fits"), nested, improper_trace] {
                assert_eq!(
                    raise_3(ctx, &[error, reason, bad]).expect("the badarg atom is returned"),
                    badarg,
                    "stacktrace {bad:?}"
                );
            }
            // ... and the returned atom must not be fabricated: without the name
            // (or an interner) the failure is the typed `TermNotFound`.
            let mut heap = Heap::new(64).expect("test heap");
            let mut bare = AtomTable::new();
            bare.seed(["error", "foo"]);
            let mut ctx = BifContext::new(&mut heap, &bare, ProcessId::new(1, 0));
            let error = ctx.atom("error").expect("seeded");
            let foo = ctx.atom("foo").expect("seeded");
            let empty = ctx.list(&[]).expect("fits");
            let nope = ctx.atom("nope");
            assert!(matches!(nope, Err(BeamError::TermNotFound)));
            let bad_class = ctx.int(1).expect("fits");
            assert!(matches!(
                raise_3(&mut ctx, &[bad_class, foo, empty]),
                Err(BeamError::TermNotFound)
            ));
            assert!(matches!(
                raise_3(&mut ctx, &[error, foo, bad_class]),
                Err(BeamError::TermNotFound)
            ));
        });
    }

    /// A non-atom reason is the reason **term** itself, in every class — the
    /// live-reason slot of [`beam_common::Raise`] (`bif.c:1564-1571` stores
    /// `fvalue = value`, whatever it is).
    #[test]
    fn raise_carries_a_non_atom_reason_term() {
        with_ctx(|ctx| {
            let empty = ctx.list(&[]).expect("fits");
            let error = ctx.atom("error").expect("seeded");
            let exit = ctx.atom("exit").expect("seeded");
            let throw = ctx.atom("throw").expect("seeded");
            let a = ctx.atom("a").expect("seeded");
            let reason = ctx.tuple(&[a]).expect("fits");
            for (class, expected) in [
                (error, ExceptionClass::Error),
                (exit, ExceptionClass::Exit),
                (throw, ExceptionClass::Throw),
            ] {
                match raise_3(ctx, &[class, reason, empty]) {
                    Err(BeamError::Raise(raise)) => {
                        assert_eq!(raise.class, expected);
                        assert_eq!(raise.reason, beam_common::ExitReason::Structured);
                        assert_eq!(raise.reason.as_atom(), None);
                        let live = *raise
                            .live_reason()
                            .expect("the reason term is attached")
                            .downcast_ref::<Eterm>()
                            .expect("the payload is the live reason term");
                        assert_eq!(live, reason);
                    }
                    other => panic!("expected a raise, got {other:?}"),
                }
            }
            // An integer reason survives too; the atom path keeps `Atom(..)` and
            // carries no live slot.
            let seven = ctx.int(7).expect("fits");
            match raise_3(ctx, &[exit, seven, empty]) {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.class, ExceptionClass::Exit);
                    assert_eq!(raise.reason, beam_common::ExitReason::Structured);
                    assert_eq!(
                        raise
                            .live_reason()
                            .and_then(|live| live.downcast_ref::<Eterm>().copied()),
                        Some(seven)
                    );
                }
                other => panic!("expected a raise, got {other:?}"),
            }
            let foo = ctx.atom("foo").expect("seeded");
            match raise_3(ctx, &[error, foo, empty]) {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.reason, beam_common::ExitReason::atom("foo"));
                    assert!(raise.live_reason().is_none(), "an atom reason has no term");
                }
                other => panic!("expected a raise, got {other:?}"),
            }
            // The well-known atoms keep their dedicated variants.
            let normal = ctx.intern_atom("normal").expect("interned");
            match raise_3(ctx, &[exit, normal, empty]) {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.reason, beam_common::ExitReason::Normal);
                    assert!(raise.live_reason().is_none());
                }
                other => panic!("expected a raise, got {other:?}"),
            }
        });
    }

    // -- raise/3's stacktrace decoding -------------------------------------

    /// The frame names the stacktrace tests build from: the shared table plus the
    /// module/function/location names a frame needs (`file`/`line` are the location
    /// keys).
    fn trace_atoms() -> AtomTable {
        let mut atoms = atoms();
        atoms.seed(["m", "f", "n", "g", "file", "line"]);
        atoms
    }

    /// Runs `f` over a fresh context whose table also holds the frame names.
    fn with_trace_ctx<R>(f: impl FnOnce(&mut BifContext<'_>) -> R) -> R {
        let mut heap = heap();
        let atoms = trace_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx)
    }

    /// The raise `result` carries.
    fn trace_raise(result: Result<Eterm>) -> beam_common::Raise {
        match result {
            Err(BeamError::Raise(raise)) => raise,
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    /// Asserts `result` is the typed `unsupported_bif` capability gap for a trace
    /// element the oracle accepts but a frame cannot express — an `error`-class raise
    /// whose reason is that atom, **never** the returned `badarg` atom (the argument
    /// is valid, so it is a gap, not an argument error).
    fn assert_unsupported_bif(result: Result<Eterm>, what: &str) {
        assert!(
            !matches!(&result, Err(BeamError::BadArg)),
            "{what}: a *valid* trace element was reported as the returned badarg"
        );
        match result {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, ExceptionClass::Error, "{what}: the class");
                assert_eq!(
                    raise.reason,
                    beam_common::ExitReason::atom("unsupported_bif"),
                    "{what}: the typed gap"
                );
            }
            other => panic!("{what}: expected the typed unsupported_bif, got {other:?}"),
        }
    }

    /// A `[{file, FileName}, {line, N}]` location with an ASCII `file` char list.
    fn location_term(ctx: &mut BifContext<'_>, file: &str, line: i64) -> Eterm {
        let file_key = ctx.atom("file").expect("seeded");
        let line_key = ctx.atom("line").expect("seeded");
        let name = bytes(ctx, file);
        let line = ctx.int(line).expect("fits");
        let file_pair = ctx.tuple(&[file_key, name]).expect("fits");
        let line_pair = ctx.tuple(&[line_key, line]).expect("fits");
        ctx.list(&[file_pair, line_pair]).expect("fits")
    }

    /// A `{Module, Function, Arity[, Location]}` frame.
    fn frame_term(
        ctx: &mut BifContext<'_>,
        module: &str,
        function: &str,
        arity: i64,
        location: Option<Eterm>,
    ) -> Eterm {
        let module = ctx.atom(module).expect("seeded");
        let function = ctx.atom(function).expect("seeded");
        let arity = ctx.int(arity).expect("fits");
        match location {
            Some(location) => ctx.tuple(&[module, function, arity, location]).expect("fits"),
            None => ctx.tuple(&[module, function, arity]).expect("fits"),
        }
    }

    /// A one-frame trace list.
    fn trace_of(ctx: &mut BifContext<'_>, frame: Eterm) -> Eterm {
        ctx.list(&[frame]).expect("fits")
    }

    /// The full decode: `{M,F,A,[{file,Name},{line,N}]}` and a bare `{M,F,A}`
    /// (whose absent location decodes to `None`, i.e. the `[]` the oracle
    /// normalizes to), in the trace's own order.
    #[test]
    fn raise_decodes_every_frame_of_a_stacktrace() {
        with_trace_ctx(|ctx| {
            let error = ctx.atom("error").expect("seeded");
            let foo = ctx.atom("foo").expect("seeded");
            let location = location_term(ctx, "lists.erl", 1234);
            let first = frame_term(ctx, "m", "f", 2, Some(location));
            let second = frame_term(ctx, "n", "g", 1, None);
            let trace = ctx.list(&[first, second]).expect("fits");

            let raise = trace_raise(raise_3(ctx, &[error, foo, trace]));
            assert_eq!(raise.class, ExceptionClass::Error);
            assert_eq!(raise.reason, beam_common::ExitReason::atom("foo"));
            assert_eq!(
                raise.stacktrace.frames,
                vec![
                    StackFrame {
                        module: AtomName::new("m"),
                        function: AtomName::new("f"),
                        arity: 2,
                        location: Some(SourceLocation {
                            file: AtomName::new("lists.erl"),
                            line: 1234,
                        }),
                    },
                    StackFrame {
                        module: AtomName::new("n"),
                        function: AtomName::new("g"),
                        arity: 1,
                        location: None,
                    },
                ]
            );
        });
    }

    /// An absent location and the explicit `[]` are both the absent location (the
    /// oracle renders both as the `[]` `Location` slot).
    #[test]
    fn raise_decodes_an_absent_or_empty_location_as_no_location() {
        with_trace_ctx(|ctx| {
            let error = ctx.atom("error").expect("seeded");
            let foo = ctx.atom("foo").expect("seeded");
            let empty = ctx.list(&[]).expect("fits");
            let expected = vec![StackFrame::new(AtomName::new("m"), AtomName::new("f"), 1)];
            let absent = frame_term(ctx, "m", "f", 1, None);
            let explicit_empty = frame_term(ctx, "m", "f", 1, Some(empty));
            for frame in [absent, explicit_empty] {
                let trace = trace_of(ctx, frame);
                let raise = trace_raise(raise_3(ctx, &[error, foo, trace]));
                assert_eq!(raise.stacktrace.frames, expected);
            }
        });
    }

    /// The empty trace decodes to the empty [`StackTrace`], which leaves the
    /// engine's supplied-trace latch unarmed — today's behaviour, byte for byte.
    #[test]
    fn raise_decodes_an_empty_stacktrace_as_empty() {
        with_trace_ctx(|ctx| {
            let error = ctx.atom("error").expect("seeded");
            let foo = ctx.atom("foo").expect("seeded");
            let empty = ctx.list(&[]).expect("fits");
            let raise = trace_raise(raise_3(ctx, &[error, foo, empty]));
            assert!(raise.stacktrace.is_empty());
            assert_eq!(raise.stacktrace.len(), 0);
            assert_eq!(raise.stacktrace, StackTrace::new());
        });
    }

    /// An **atom** reason keeps its exact [`beam_common::ExitReason`] and carries no
    /// live slot, and still carries the decoded frames.
    #[test]
    fn raise_carries_the_trace_with_an_atom_reason() {
        with_trace_ctx(|ctx| {
            let throw = ctx.atom("throw").expect("seeded");
            let normal = ctx.atom("normal").expect("seeded");
            let frame = frame_term(ctx, "m", "f", 3, None);
            let trace = trace_of(ctx, frame);
            let raise = trace_raise(raise_3(ctx, &[throw, normal, trace]));
            assert_eq!(raise.class, ExceptionClass::Throw);
            assert_eq!(raise.reason, beam_common::ExitReason::Normal);
            assert!(raise.live_reason().is_none(), "an atom reason has no term");
            assert_eq!(
                raise.stacktrace.frames,
                vec![StackFrame::new(AtomName::new("m"), AtomName::new("f"), 3)]
            );
        });
    }

    /// A **non-atom** reason carries the term itself in the live-reason slot *and*
    /// the decoded frames — the two are orthogonal.
    #[test]
    fn raise_carries_the_trace_with_a_non_atom_reason() {
        with_trace_ctx(|ctx| {
            let exit = ctx.atom("exit").expect("seeded");
            let a = ctx.atom("a").expect("seeded");
            let reason = ctx.tuple(&[a]).expect("fits");
            let location = location_term(ctx, "m.erl", 7);
            let frame = frame_term(ctx, "m", "f", 1, Some(location));
            let trace = trace_of(ctx, frame);
            let raise = trace_raise(raise_3(ctx, &[exit, reason, trace]));
            assert_eq!(raise.class, ExceptionClass::Exit);
            assert_eq!(raise.reason, beam_common::ExitReason::Structured);
            assert_eq!(
                raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<Eterm>().copied()),
                Some(reason)
            );
            assert_eq!(
                raise.stacktrace.frames,
                vec![StackFrame {
                    module: AtomName::new("m"),
                    function: AtomName::new("f"),
                    arity: 1,
                    location: Some(SourceLocation {
                        file: AtomName::new("m.erl"),
                        line: 7,
                    }),
                }]
            );
        });
    }

    /// Every trace element the oracle echoes verbatim but a [`StackFrame`] cannot
    /// express is the typed `unsupported_bif` raise — never the returned `badarg`
    /// (the argument *is* valid) and never a truncated or invented frame. Ground
    /// truth (erts-17.1, each shape echoed back unchanged by the oracle):
    ///
    /// ```text
    /// [{m,f,[a,b],[]}]                    ⇒ [{m,f,[a,b],[]}]
    /// [{m,f,256}]   /  [{m,f,-1}]         ⇒ accepted verbatim
    /// [{fun(X)->X end,[1]}]               ⇒ [{#Fun<..>,[1],[]}]
    /// [{m,f,1,[{file,m},{line,1}]}]       ⇒ accepted verbatim (atom file name)
    /// [{m,f,1,[{file,"x"}]}]              ⇒ accepted verbatim (one pair)
    /// [{m,f,1,[{file,"x"},{line,1},{a,b}]}] ⇒ accepted verbatim (three pairs)
    /// [{m,f,1,[{line,1},{file,"x"}]}]     ⇒ accepted verbatim (reversed)
    /// [{m,f,1,[{file,"x"},{line,-1}]}]    ⇒ accepted verbatim
    /// [{m,f,1,[{file,[1114112]},{line,1}]}] ⇒ accepted verbatim
    /// ```
    #[test]
    fn raise_reports_unrepresentable_trace_elements_as_unsupported_bif() {
        with_trace_ctx(|ctx| {
            let error = ctx.atom("error").expect("seeded");
            let foo = ctx.atom("foo").expect("seeded");
            let a = ctx.atom("a").expect("seeded");
            let b = ctx.atom("b").expect("seeded");
            let m = ctx.atom("m").expect("seeded");
            let f = ctx.atom("f").expect("seeded");
            let file = ctx.atom("file").expect("seeded");
            let line = ctx.atom("line").expect("seeded");
            let one = ctx.int(1).expect("fits");
            let args = ctx.list(&[a, b]).expect("fits");
            let empty = ctx.list(&[]).expect("fits");
            let file_name = bytes(ctx, "x");

            // A real `EXPORT_EXT` fun, so `{Fun, Args[, Location]}` is a *valid*
            // frame (the oracle's `erl_eval` call-frame shape).
            let fun = codec_term::make_fun_external(ctx.heap(), 1, atom_val(f), atom_val(m))
                .expect("the external fun fits");
            let call_two = ctx.tuple(&[fun, args]).expect("fits");
            let call_three = ctx.tuple(&[fun, args, empty]).expect("fits");

            // `[{file, "x"}, {line, 1}]` and the malformed variants of it.
            let file_pair = ctx.tuple(&[file, file_name]).expect("fits");
            let line_pair = ctx.tuple(&[line, one]).expect("fits");
            let other_pair = ctx.tuple(&[a, b]).expect("fits");
            let bad_key_pair = ctx.tuple(&[ctx.atom("foo").expect("seeded"), file_name]).expect("fits");
            let atom_name_pair = ctx.tuple(&[file, a]).expect("fits");
            let non_int_line = ctx.tuple(&[line, a]).expect("fits");
            let huge_line = ctx.int(4_294_967_296).expect("fits");
            let huge_line_pair = ctx.tuple(&[line, huge_line]).expect("fits");
            let huge_code = ctx.int(1_114_112).expect("fits");
            let huge_code_name = ctx.list(&[huge_code]).expect("fits");
            let huge_code_pair = ctx.tuple(&[file, huge_code_name]).expect("fits");
            let float = ctx.make_float(1.0).expect("fits");
            let two_fifty_six = ctx.int(256).expect("fits");
            let minus_one = ctx.int(-1).expect("fits");

            // One `{m, f, A, Location}` frame with an already-built location list.
            let frame = |ctx: &mut BifContext<'_>, arity: Eterm, pairs: &[Eterm]| {
                let location = ctx.list(pairs).expect("fits");
                ctx.tuple(&[m, f, arity, location]).expect("fits")
            };
            let arity_list = ctx.tuple(&[m, f, args]).expect("fits");
            let arity_float = ctx.tuple(&[m, f, float]).expect("fits");
            let arity_256 = ctx.tuple(&[m, f, two_fifty_six]).expect("fits");
            let arity_negative = ctx.tuple(&[m, f, minus_one]).expect("fits");

            let atom_file = frame(ctx, one, &[atom_name_pair, line_pair]);
            let one_pair = frame(ctx, one, &[file_pair]);
            let three_pairs = frame(ctx, one, &[file_pair, line_pair, other_pair]);
            let reversed = frame(ctx, one, &[line_pair, file_pair]);
            let non_integer_line = frame(ctx, one, &[file_pair, non_int_line]);
            let line_above_u32 = frame(ctx, one, &[file_pair, huge_line_pair]);
            let wrong_key = frame(ctx, one, &[bad_key_pair, line_pair]);
            let huge_code_point = frame(ctx, one, &[huge_code_pair, line_pair]);

            let traces = [
                ("{Fun,Args} — an erl_eval call frame", trace_of(ctx, call_two)),
                ("{Fun,Args,Location} — an erl_eval call frame", trace_of(ctx, call_three)),
                ("an argument list in the arity slot", trace_of(ctx, arity_list)),
                ("a float in the arity slot", trace_of(ctx, arity_float)),
                ("256 in the arity slot", trace_of(ctx, arity_256)),
                ("-1 in the arity slot", trace_of(ctx, arity_negative)),
                ("an atom file name", trace_of(ctx, atom_file)),
                ("a one-pair location", trace_of(ctx, one_pair)),
                ("a three-pair location", trace_of(ctx, three_pairs)),
                ("a reversed location", trace_of(ctx, reversed)),
                ("a non-integer line", trace_of(ctx, non_integer_line)),
                ("a line above u32::MAX", trace_of(ctx, line_above_u32)),
                ("a wrong location key", trace_of(ctx, wrong_key)),
                ("an out-of-range file code point", trace_of(ctx, huge_code_point)),
            ];

            for (what, trace) in traces {
                assert_unsupported_bif(raise_3(ctx, &[error, foo, trace]), what);
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
            ("node", 1),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 23);
    }
}
