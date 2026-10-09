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

//! The **number** helpers: the float and integer builders and the float reader.
//! An integer outside the 60-bit small range is written as an exact bignum.
//!
//! The classic [`make_float`]/[`make_integer`] allocate through the plain
//! [`ExecutionContext::alloc_words`]; their `*_rooted` twins take the machine's root
//! set. Neither number builder has an `Eterm` operand — the value is Rust-native — so
//! the rooted twins simply route their (single) allocation through the rooted seam
//! ([`Machine::alloc_rooted`](crate::machine::Machine::alloc_rooted)) with the whole
//! X file declared live. There is no operand to relocate, so no reserve-before-read
//! ordering is needed here.
use beam_common::{BeamError, Result};
use beam_term::{make_small, thing, Eterm, MAX_SMALL, MIN_SMALL};

use super::reserve;
use crate::context::{ExecutionContext, RootSet};
use crate::machine::Machine;

/// Builds a float term from `value`.
///
/// The `[HEADER_FLONUM, f64 bits]` layout (2 words) is written by
/// [`beam_term::thing::write_float`]. The pointer is valid until the next
/// [`ExecutionContext::ensure_heap`] call (the documented stability contract).
pub fn make_float(ctx: &mut dyn ExecutionContext, value: f64) -> Result<Eterm> {
    let hp = ctx.alloc_words(thing::FLOAT_SIZE_WORDS)?;
    // SAFETY: `alloc_words` returned exactly `FLOAT_SIZE_WORDS` writable words,
    // which `write_float` fully initialises.
    Ok(unsafe { thing::write_float(hp, value) })
}

/// Builds an integer term for `value`: a small int when it fits, an exact bignum
/// otherwise.
///
/// `MIN_SMALL..=MAX_SMALL` (the 60-bit immediate range, `beam_term::immediate`) is
/// materialised with [`beam_term::make_small`]. Anything outside it — reachable from
/// real `erlc` output, whose `integer`/`character` operands may carry a value in
/// `[2^59, 2^63)` — is written as an **exact bignum** with
/// [`beam_term::thing::write_big`] (`BIG_NEED_SIZE(1) = 2` words: the header plus one
/// `ErtsDigit`, with the sign in the header's `_BIG_SIGN_BIT`). A non-small `i64`
/// always fits exactly one `u64` `ErtsDigit`, so the digit slice is normalised
/// (non-empty, no leading zero) by construction.
///
/// The pointer is valid until the next [`ExecutionContext::ensure_heap`] call (the
/// documented stability contract). This is the shared constructor
/// [`crate::operand::read_operand`] uses, so a small-vs-bignum mismatch is
/// impossible.
pub fn make_integer(ctx: &mut dyn ExecutionContext, value: i64) -> Result<Eterm> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        return Ok(make_small(value));
    }
    let hp = ctx.alloc_words(beam_term::big_size_words(1))?;
    write_big_into(hp, value)
}

/// The **rooted** twin of [`make_float`]: builds a float term from `value`.
///
/// The `[HEADER_FLONUM, f64 bits]` layout (2 words) is written by
/// [`beam_term::thing::write_float`]. The block is reserved **rooted**; the pointer
/// is valid until the next collecting allocation (the documented stability contract).
pub fn make_float_rooted(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    value: f64,
) -> Result<Eterm> {
    let hp = reserve(machine, ctx, thing::FLOAT_SIZE_WORDS, RootSet::ALL_LIVE)?;
    // SAFETY: the reserve returned exactly `FLOAT_SIZE_WORDS` writable words, which
    // `write_float` fully initialises.
    Ok(unsafe { thing::write_float(hp, value) })
}

/// The **rooted** twin of [`make_integer`]: builds an integer term for `value`: a
/// small int when it fits, an exact bignum otherwise.
///
/// `MIN_SMALL..=MAX_SMALL` (the 60-bit immediate range, `beam_term::immediate`) is
/// materialised with [`beam_term::make_small`] and allocates nothing. Anything
/// outside it — reachable from real `erlc` output, whose `integer`/`character`
/// operands may carry a value in `[2^59, 2^63)` — is written as an **exact bignum**
/// with [`beam_term::thing::write_big`] (`BIG_NEED_SIZE(1) = 2` words: the header
/// plus one `ErtsDigit`, with the sign in the header's `_BIG_SIGN_BIT`) into a block
/// reserved **rooted**. A non-small `i64` always fits exactly one `u64` `ErtsDigit`,
/// so the digit slice is normalised (non-empty, no leading zero) by construction.
pub fn make_integer_rooted(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    value: i64,
) -> Result<Eterm> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        return Ok(make_small(value));
    }
    let hp = reserve(
        machine,
        ctx,
        thing::big_size_words(1),
        RootSet::ALL_LIVE,
    )?;
    write_big_into(hp, value)
}

/// The bignum writer shared by [`make_integer`], [`make_integer_rooted`] and
/// [`make_integer_unrooted`].
///
/// `hp` must point at `big_size_words(1) = 2` exclusively-owned, writable words.
fn write_big_into(hp: *mut Eterm, value: i64) -> Result<Eterm> {
    // `unsigned_abs` is total, including `i64::MIN`; a non-small value has a non-zero
    // magnitude, so there is no leading zero digit to strip.
    let digits = [value.unsigned_abs()];
    // SAFETY: `hp` addresses exactly `big_size_words(1) = 2` exclusively-owned,
    // writable words — the block `write_big` requires and fully writes (header + one
    // `ErtsDigit`). The context owns the arena and keeps it live for as long as any
    // term built from this pointer is used.
    Ok(unsafe { thing::write_big(hp, value < 0, &digits) })
}

/// Builds an integer term for an operand **without a mutable machine** — the
/// `read_operand(&Machine, ..)` compatibility path.
///
/// The read-only interpreter call sites
/// ([`crate::operand::read_operand`]) hand over only a shared `&Machine`, so this
/// lone operand read cannot take the machine's root set. It therefore allocates
/// through the **unrooted** [`ExecutionContext::alloc_words`]: it holds no
/// intermediate term across the allocation (the result is consumed immediately by
/// the caller), so it is moving-safe as long as the host does not collect in
/// `alloc_words` — the seam contract reserves collection for
/// [`ExecutionContext::alloc_words_rooted`] /
/// [`ExecutionContext::ensure_heap`], which is where the root set is available.
/// Every *builder* (`make_tuple`, `make_list`, …) instead reserves through the rooted
/// seam and materializes such an operand via [`super::Slack::integer`].
pub(crate) fn make_integer_unrooted(
    ctx: &mut dyn ExecutionContext,
    value: i64,
) -> Result<Eterm> {
    if (MIN_SMALL..=MAX_SMALL).contains(&value) {
        return Ok(make_small(value));
    }
    let hp = ctx.alloc_words(beam_term::big_size_words(1))?;
    write_big_into(hp, value)
}

/// The `f64` value of a float term; a non-float yields [`BeamError::BadArg`].
///
/// [`beam_term::thing::float_value`] does not itself reject a non-float, so the
/// type check happens here.
pub fn float_value(t: Eterm) -> Result<f64> {
    // SAFETY: `is_float` checks `is_boxed` before dereferencing the header word.
    if !unsafe { beam_term::is_float(t) } {
        return Err(BeamError::BadArg);
    }
    // SAFETY: `is_float` established that `t` is a live boxed float.
    Ok(unsafe { thing::float_value(t) })
}
