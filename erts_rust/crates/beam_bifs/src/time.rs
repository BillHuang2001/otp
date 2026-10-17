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

//! `erlang` time and unique-integer BIFs: `monotonic_time/0,1`,
//! `system_time/0,1`, `time_offset/0,1`, `unique_integer/0,1`, `timestamp/0`.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_time_sup.c`
//! (`monotonic_time_*`, `system_time_*`, `time_offset_*`, `timestamp_0` and
//! `time_unit_conversion`), `../../erts/emulator/beam/erl_bif_unique.c`
//! (`unique_integer_*`), and `../../erts/emulator/beam/bif.tab` (lines 100-122 —
//! the authoritative names/arities).
//!
//! # Native unit and the `/1` conversion
//!
//! Every value is produced in the runtime's **native** time unit, fixed here to
//! **nanoseconds** (`1e9` per second; this Linux host reports the same unit via
//! `erts_internal:time_unit/0`, and the same `1e9` via
//! `erts_internal:perf_counter_unit/0`).
//!
//! A `/1` unit argument names the target unit either as an atom or as a
//! **positive integer** — that many target units per second, the
//! `erlang:convert_time_unit/3` convention (`system_time(1)` => seconds,
//! `system_time(1000)` => milliseconds). The accepted atoms are the ones
//! `time_unit_conversion`'s switch lists (`erl_time_sup.c:2277-2309`):
//! `native` and `perf_counter` (both the native `1e9`/s here), `second`/`seconds`,
//! `millisecond`/`milli_seconds`, `microsecond`/`micro_seconds` and
//! `nanosecond`/`nano_seconds`. The value is
//! `native * per_second div 1_000_000_000`, **flooring toward negative infinity**,
//! exactly as `erlang:convert_time_unit/3` does (`erl_time_sup.c:2268`, verified
//! against the oracle; the rounding is `erts_napi_convert_time_unit`,
//! `erl_time_sup.c:2462-2495`).
//!
//! The integer form is **arbitrary precision**: a unit that fits a small integer
//! converts with `i128` arithmetic, while a **positive bignum** unit — which the
//! C's `default` arm traps to `erlang:convert_time_unit/3`, i.e.
//! `TU * Time div 1e9` with `TU` a bignum (`erl_time_sup.c:2330-2352`) — converts
//! in arbitrary precision through the crate's registered `erlang` arithmetic
//! ([`convert_unit_big`]). `monotonic_time(1 bsl 200)` is therefore the oracle's
//! bignum, not a capability gap.
//!
//! Everything else is `badarg`: an unrecognized atom (`monotonic_time(fortnight)`),
//! a non-integer (`1.0`, a list), a zero or negative **small** integer, a negative
//! bignum (the C's `big_sign`), and — as in the oracle, whose trap path wraps its
//! arithmetic in `try ... catch _:_ -> badarg` — a bignum unit whose product with
//! the native reading exceeds `BIG_ARITY_MAX` limbs
//! (`convert_time_unit(1000000, nanosecond, 1 bsl 4194239)` => `badarg`).
//!
//! # The ERTS monotonic epoch
//!
//! `erlang:monotonic_time/0,1` reports the **same monotonic epoch a real
//! erts-17.1 VM reports**, not a process-relative elapsed count. ERTS measures
//! monotonic time internally in `ERTS_MONOTONIC_TIME_UNIT` ticks — nanoseconds on
//! the 64-bit Unix build (`erl_unix_sys.h:193-198`) — whose *internal* origin is
//! `ERTS_MONOTONIC_BEGIN = UNIT` (`erl_time.h:235-236`), and exposes it externally
//! shifted by a fixed offset (`erl_time.h:331-332`) so that the external origin is
//!
//! ```text
//! ERTS_MONOTONIC_TIME_START_EXTERNAL
//!   = (MIN_SMALL / ERTS_MONOTONIC_TIME_UNIT) * ERTS_MONOTONIC_TIME_UNIT   (erl_time.h:312-323)
//!   = -576_460_752_000_000_000
//! ```
//!
//! with `MIN_SMALL = -(1 << 59)` (`erl_term.h:269-270`) and the C division
//! truncating toward zero. The external reading is therefore
//! `START_EXTERNAL + elapsed` — a large **negative** integer that grows toward
//! zero as the VM runs, bounded below by
//! `erlang:system_info(start_time)` and above by `(end_time)`. The *(elapsing)*
//! half is the Rust process monotonic clock ([`Instant`] captured lazily through a
//! global [`OnceLock`]), which never decreases; the epoch half is the compile-time
//! constant [`MONOTONIC_TIME_START_EXTERNAL`]. Both bounds of the external range
//! are the same interface constants `beam_rt`'s `src/info/time.rs` publishes as
//! `START_TIME`/`END_TIME` (the two crates cannot share a private const: `beam_rt`
//! depends on this crate, not the other way round — see the Handoffs note in the
//! crate `CONTEXT.md`).
//!
//! # The other clocks
//!
//! * `system_time/0` reads [`SystemTime`] — **nanoseconds since the UNIX epoch**
//!   (`1970-01-01T00:00:00Z`), the oracle's meaning.
//! * `time_offset/0` is the offset for which
//!   `system_time = monotonic_time + time_offset` (the invariant the oracle
//!   exhibits), i.e. `system_time(native) - monotonic_time(native)`. With the ERTS
//!   epoch on both sides it is the oracle's own ~74-year constant (probe:
//!   `erlang:time_offset()` ⇒ `2368668745638096205` ns), and it tracks the system
//!   clock as the two halves advance together.
//! * `timestamp/0` splits `system_time/0` into `{MegaSecs, Secs, MicroSecs}`
//!   exactly as `make_timestamp_value` does (`erl_time_sup.c:1907`).
//!
//! # Past the 60-bit small-integer range
//!
//! A native-nanosecond count since 1970 has exceeded the 60-bit small-integer
//! range (`2^59 - 1` ≈ 5.76·10^17 ns, i.e. 1988) for decades, so `system_time/0`,
//! `system_time(nanosecond)`, `system_time(native)` and both `time_offset` forms
//! return a **bignum**, exactly as real ERTS does — the value is materialized by
//! the module-local `int_from_i128`, which folds a value that fits back into a
//! small integer through [`crate::arith::int_from_i64`] and promotes otherwise.
//! The coarser units (`second`/`millisecond`/`microsecond`) stay small in
//! practice, and so do the monotonic readings of the whole/near-by units
//! (`native`/`nanosecond`: the epoch's external origin is a 60-bit small and a
//! VM would have to run ~36 years for `start_time + elapsed` to leave the small
//! range), while a coarse **integer** unit conversely promotes to a *negative*
//! bignum (`system_time(1 bsl 58)`, `monotonic_time(1 bsl 58)`).
//!
//! [`BeamError::SystemLimit`] remains only for a clock before 1970, a nanosecond
//! count past `i64` (year ~2262), or a `time_offset` that would not fit `i64`
//! (year ~2243 on this epoch) — never a truncated or wrapped value.
//!
//! # `unique_integer`
//!
//! `unique_integer/0,1` draw from one process-global [`AtomicI64`] counter
//! (`fetch_add(1)`): the raw sequence is `0, 1, 2, ...`, shifted to `1, 2, 3, ...`
//! by the `positive` modifier. The single counter makes every stream strictly
//! increasing, so the `monotonic` modifier needs no separate state. The result is
//! always an **integer** — `unique_integer/1` does *not* return a tuple (verified
//! against erts-17.1: `erlang:unique_integer([positive])` => `1186`, and the OTP
//! docs type it `integer()`). Repeated modifiers are ignored; a non-list, an
//! improper list, or an unknown modifier is `badarg` (`erl_bif_unique.c:840`).
//!
//! Documented deviation: ERTS's `unique_integer/0`/`[monotonic]` return *negative*
//! values from the non-positive space (so they never collide with the `positive`
//! values); M1's single non-negative counter returns values `>= 0`. Uniqueness and
//! monotonicity hold; the exact integers are opaque to programs.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use beam_common::{BeamError, Result};
use beam_term::{atom_val, signed_value, Eterm};

use crate::access::{is_atom, is_big, is_integer, is_small};
use crate::arith;
use crate::context::BifContext;
use crate::error::badarg;
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// A time unit accepted by the `/1` BIFs — the atom forms of `time_unit_conversion`'s
/// switch (`erl_time_sup.c:2277-2309`) plus the integer "units per second" form
/// `erlang:convert_time_unit/3` also accepts, small or bignum.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TimeUnit {
    /// `native` — the runtime's native unit (nanoseconds in M1, `1e9`/s).
    Native,
    /// `perf_counter` — the perf-counter unit (also `1e9`/s on this host,
    /// `erts_internal:perf_counter_unit/0`).
    PerfCounter,
    /// `nanosecond` / the deprecated `nano_seconds`.
    Nanosecond,
    /// `microsecond` / the deprecated `micro_seconds`.
    Microsecond,
    /// `millisecond` / the deprecated `milli_seconds`.
    Millisecond,
    /// `second` / the deprecated `seconds`.
    Second,
    /// A caller-supplied **small positive** integer: that many target units per
    /// second (`system_time(1)` => seconds, `system_time(1000)` => milliseconds).
    PerSecond(i64),
    /// A caller-supplied **positive bignum**: that many target units per second,
    /// held as a little-endian `u64` magnitude — the arbitrary-precision form the
    /// C converts in Erlang code (`erl_time_sup.c:2338-2352`).
    PerSecondBig(Vec<u64>),
}

/// A time unit's arithmetic image: the **units per second** ratio
/// `erlang:convert_time_unit/3` computes with (`FU` and `TU` in
/// `../../erts/preloaded/src/erlang.erl:5340-5371`).
///
/// [`TimeUnit`] is the same decision in the shape the `/1` clocks need — one unit at
/// a time; this is the pair form `/3` needs, split the same way: every accepted atom
/// and any positive small integer is a ratio `i128` holds, while a caller-supplied
/// bignum unit is its limb magnitude.
///
/// `Small`/`Big` never name the same ratio, so the derived `Eq` is the oracle's
/// `FU =:= TU`: every `Small` value comes from a small integer (at most `2^59 - 1`)
/// or from one of the four named atoms, and a bignum unit is by definition not a
/// small integer.
#[derive(Debug, Clone, PartialEq, Eq)]
enum UnitRatio {
    /// Units per second, for every accepted atom and a positive small integer.
    Small(i128),
    /// A positive-bignum unit: its little-endian `u64` magnitude, units per second.
    Big(Vec<u64>),
}

impl TimeUnit {
    /// The units-per-second ratio this unit names.
    fn ratio(&self) -> UnitRatio {
        match self {
            TimeUnit::PerSecondBig(magnitude) => UnitRatio::Big(magnitude.clone()),
            TimeUnit::Native | TimeUnit::PerfCounter | TimeUnit::Nanosecond => {
                UnitRatio::Small(1_000_000_000)
            }
            TimeUnit::Microsecond => UnitRatio::Small(1_000_000),
            TimeUnit::Millisecond => UnitRatio::Small(1_000),
            TimeUnit::Second => UnitRatio::Small(1),
            TimeUnit::PerSecond(per_second) => UnitRatio::Small(i128::from(*per_second)),
        }
    }
}
/// The unit named by `term` — exactly the set `time_unit_conversion` accepts
/// (`erl_time_sup.c:2277-2341`): the atoms
/// `native`/`perf_counter`/`second`/`seconds`/`millisecond`/`milli_seconds`/
/// `microsecond`/`micro_seconds`/`nanosecond`/`nano_seconds`, or any **positive**
/// integer (units per second) — a small integer or a bignum.
///
/// # Errors
///
/// [`BeamError::BadArg`] for anything else — an unknown atom, a non-integer, a
/// zero or negative small integer, and a **negative** bignum (the C's `big_sign`).
/// A *positive* bignum is a legal unit ([`TimeUnit::PerSecondBig`]); only a unit
/// whose product with the native reading overflows the bignum cap is `badarg`
/// ([`convert_unit_big`]).
fn time_unit(ctx: &BifContext<'_>, term: Eterm) -> Result<TimeUnit> {
    if is_small(term) {
        let value = signed_value(term);
        // `time_unit_conversion`'s `signed_val(term) <= 0` arm.
        if value <= 0 {
            return Err(badarg());
        }
        return Ok(TimeUnit::PerSecond(value));
    }
    if is_big(term) {
        // The C accepts a non-negative bignum (its `default` arm traps to
        // `erlang:convert_time_unit/3`, which computes `ToUnit * Time div
        // FromUnit` in arbitrary precision); a negative one is `badarg`
        // (`big_sign`). A bignum term is never zero, so a non-negative one is
        // positive.
        let (negative, magnitude) = beam_codec::term::big_parts(term).map_err(|_| badarg())?;
        return if negative {
            Err(badarg())
        } else {
            Ok(TimeUnit::PerSecondBig(magnitude.to_vec()))
        };
    }
    if is_atom(term) {
        return match ctx.atoms().name(atom_val(term)) {
            Some("native") => Ok(TimeUnit::Native),
            Some("perf_counter") => Ok(TimeUnit::PerfCounter),
            Some("second" | "seconds") => Ok(TimeUnit::Second),
            Some("millisecond" | "milli_seconds") => Ok(TimeUnit::Millisecond),
            Some("microsecond" | "micro_seconds") => Ok(TimeUnit::Microsecond),
            Some("nanosecond" | "nano_seconds") => Ok(TimeUnit::Nanosecond),
            _ => Err(badarg()),
        };
    }
    Err(badarg())
}

/// Converts a native (nanosecond) value into `unit` — `native * per_second div
/// 1_000_000_000`, **flooring toward negative infinity**, the
/// `erlang:convert_time_unit/3` rounding (`erts_napi_convert_time_unit`,
/// `erl_time_sup.c:2462-2495`; `Time < 0` is the `TU*Time - (FU - 1)` arm of
/// `erlang.erl`).
///
/// The multiplication runs in `i128` for every unit whose `per_second` names one:
/// the widest accepted ratio is the largest small integer (`2^59 - 1`/s), so the
/// product of an `i64` nanosecond count and `per_second` stays far inside `i128`.
/// A **bignum** unit is converted in arbitrary precision by [`convert_unit_big`].
fn convert_unit(ctx: &mut BifContext<'_>, native: i64, unit: &TimeUnit) -> Result<Eterm> {
    let per_second = match unit {
        // The C's `default` arm traps to `erlang:convert_time_unit/3`: no `i128`
        // can name this unit.
        TimeUnit::PerSecondBig(magnitude) => return convert_unit_big(ctx, native, magnitude),
        TimeUnit::Native | TimeUnit::PerfCounter | TimeUnit::Nanosecond => 1_000_000_000,
        TimeUnit::Microsecond => 1_000_000,
        TimeUnit::Millisecond => 1_000,
        TimeUnit::Second => 1,
        TimeUnit::PerSecond(per_second) => i128::from(*per_second),
    };
    int_from_i128(ctx, (i128::from(native) * per_second).div_euclid(1_000_000_000))
}

/// The **bignum** unit conversion: `native * per_second div 1_000_000_000`,
/// exact and floor-rounded in arbitrary precision.
///
/// This is the C's `trap_to_erlang_code` arm (`erl_time_sup.c:2338-2352`), which
/// hands the conversion to `erlang:convert_time_unit/3` — `TU * Time div FU` with
/// `FU = 1_000_000_000` and `TU` the unit, plus the `TU*Time - (FU - 1)`
/// adjustment that turns the truncating `div` into a floor for a negative `Time`
/// (`erlang.erl:5365-5375`). The arithmetic is the crate's registered
/// `erlang:'*'/2`, `erlang:'-'/2` and `erlang:'div'/2` implementations
/// ([`arith_operators`]): one exact bignum multiply and one exact bignum divide,
/// with no second copy of bignum math and no new dependency.
///
/// This module's clocks (`erlang:monotonic_time/1`/`system_time/1`/`time_offset/1`)
/// and [`crate::os_bif`]'s `os:system_time/1` share this one implementation: the
/// bignum-unit path is byte-identical at both call sites.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a product past `BIG_ARITY_MAX` limbs — the only
/// arithmetic failure left here (both operands are integers and the divisor is
/// `1_000_000_000`) — which is the class the oracle reports too: its own
/// `try ... catch _:_ -> badarg` folds the `system_limit` away, so
/// `convert_time_unit(1000000, nanosecond, 1 bsl 4194239)` is `badarg` in the
/// oracle and here.
pub(crate) fn convert_unit_big(
    ctx: &mut BifContext<'_>,
    native: i64,
    per_second: &[u64],
) -> Result<Eterm> {
    // Every failure of the chain below is the class the oracle reports: its
    // `convert_time_unit/3` body runs inside `try ... catch _:_ -> badarg`, so the
    // `system_limit` an oversized product raises surfaces as `badarg`.
    big_conversion(ctx, native, per_second).map_err(trap_arith_class)
}

/// The `TU * Time div FU` chain of `erlang.erl:5365-5375` with `FU =
/// 1_000_000_000`: `TU * Time` for a non-negative `Time`, `TU * Time - (FU - 1)`
/// for a negative one (which turns `div`'s truncation into a floor).
fn big_conversion(ctx: &mut BifContext<'_>, native: i64, per_second: &[u64]) -> Result<Eterm> {
    let operators = arith_operators();
    let native_term = arith::int_from_i64(ctx, native)?;
    let per_second_term = arith::int_from_mag(ctx, false, per_second)?;
    let product = operators.call("erlang", "*", 2, ctx, &[native_term, per_second_term])?;
    let numerator = if native < 0 {
        let adjust = arith::int_from_i64(ctx, 1_000_000_000 - 1)?;
        operators.call("erlang", "-", 2, ctx, &[product, adjust])?
    } else {
        product
    };
    let divisor = arith::int_from_i64(ctx, 1_000_000_000)?;
    operators.call("erlang", "div", 2, ctx, &[numerator, divisor])
}

/// The `(TU * Time [- (FU - 1)]) div FU` conversion of `erlang.erl:5372-5379`, in
/// machine arithmetic when every operand fits.
///
/// `TU` is a positive small integer (at most `2^59 - 1` units/s) and `Time` a small
/// integer, so their product is at most about `2^118` — `i128` holds it with four bits
/// to spare, and the `- (FU - 1)` adjustment cannot push it out either. The division
/// is Rust's truncating `/` — Erlang's `div` — and the adjustment turns its truncation
/// into the floor for a negative `Time`.
///
/// Anything that does **not** fit — a bignum unit ratio, a bignum `Time`, or both —
/// goes to [`big_ratio_conversion`], whose result is exact.
fn convert_ratio_pair(
    ctx: &mut BifContext<'_>,
    time: Eterm,
    from: &UnitRatio,
    to: &UnitRatio,
) -> Result<Eterm> {
    if let (UnitRatio::Small(fu), UnitRatio::Small(tu)) = (from, to) {
        if is_small(time) {
            let value = i128::from(signed_value(time));
            let scaled = tu * value;
            let numerator = if value < 0 {
                scaled - (fu - 1)
            } else {
                scaled
            };
            return int_from_i128(ctx, numerator / fu);
        }
    }
    big_ratio_conversion(ctx, time, from, to)
}

/// The `TU * Time [ - (FU - 1) ] div FU` pair conversion in arbitrary precision —/// `/3`'s shape, for a bignum unit ratio, a bignum `Time`, or both.
///
/// [`big_conversion`] is the `native * per_second div 1_000_000_000` shape the `/1`
/// clocks need; this is the two-unit shape `erlang:convert_time_unit/3` computes.
/// Both run the crate's one arbitrary-precision multiply/divide implementation
/// ([`arith_operators`] → [`crate::arith`]) — no bignum math is duplicated here.
///
/// # Errors
///
/// [`BeamError::BadArg`] for a product past `BIG_ARITY_MAX` limbs: the oracle's own
/// body runs inside `try ... catch _:_ -> badarg`, so the `system_limit` its
/// arithmetic raises surfaces as `badarg` ([`trap_arith_class`]).
fn big_ratio_conversion(
    ctx: &mut BifContext<'_>,
    time: Eterm,
    from: &UnitRatio,
    to: &UnitRatio,
) -> Result<Eterm> {
    big_ratio_chain(ctx, time, from, to).map_err(trap_arith_class)
}

/// The `TU * Time div FU` chain of `erlang.erl:5376-5379`, with the `- (FU - 1)`
/// adjustment that turns `div`'s truncation into a floor for a negative `Time`.
fn big_ratio_chain(
    ctx: &mut BifContext<'_>,
    time: Eterm,
    from: &UnitRatio,
    to: &UnitRatio,
) -> Result<Eterm> {
    let operators = arith_operators();
    let fu = ratio_term(ctx, from)?;
    let tu = ratio_term(ctx, to)?;
    let product = operators.call("erlang", "*", 2, ctx, &[tu, time])?;
    let numerator = if integer_is_negative(time)? {
        let one = arith::int_from_i64(ctx, 1)?;
        let adjust = operators.call("erlang", "-", 2, ctx, &[fu, one])?;
        operators.call("erlang", "-", 2, ctx, &[product, adjust])?
    } else {
        product
    };
    operators.call("erlang", "div", 2, ctx, &[numerator, fu])
}

/// The integer term for a unit ratio — the shared small-int-or-heap-bignum
/// promotion convention ([`int_from_i128`], `arith::int_from_mag`).
fn ratio_term(ctx: &mut BifContext<'_>, ratio: &UnitRatio) -> Result<Eterm> {
    match ratio {
        UnitRatio::Small(per_second) => int_from_i128(ctx, *per_second),
        UnitRatio::Big(magnitude) => arith::int_from_mag(ctx, false, magnitude),
    }
}

/// Whether an **integer** term is negative — the `Time < 0` arm of
/// `erlang.erl:5376`. The caller has already rejected a non-integer `Time`.
///
/// # Errors
///
/// [`BeamError::BadArg`] if `term` is neither a small integer nor a readable bignum
/// (unreachable from [`convert_ratio_pair`], which reaches here only for an integer).
fn integer_is_negative(term: Eterm) -> Result<bool> {
    if is_small(term) {
        return Ok(signed_value(term) < 0);
    }
    let (negative, _) = beam_codec::term::big_parts(term).map_err(|_| badarg())?;
    Ok(negative)
}

/// The crate's registered `erlang` arithmetic operators, for the
/// arbitrary-precision conversion a bignum time unit needs
/// ([`convert_unit_big`]).
///
/// Built once from [`crate::arith::register`] — the same `fn`s
/// [`BifRegistry::with_core_bifs`] installs — so the multi-limb multiply and the
/// Knuth-D divide stay the crate's single implementation. The registry (rather
/// than [`crate::arith`]'s private helpers) is the arithmetic entry point a
/// [`BifContext`] can be handed to, exactly as the runtime dispatches them.
fn arith_operators() -> &'static BifRegistry {    static OPERATORS: OnceLock<BifRegistry> = OnceLock::new();
    OPERATORS.get_or_init(|| {
        let mut operators = BifRegistry::new();
        arith::register(&mut operators);
        operators
    })
}

/// The error class `erlang:convert_time_unit/3` reports: its whole body runs
/// inside `try ... catch _:_ -> error_with_info(badarg, ...)`
/// (`erlang.erl:5339`), so the `system_limit` an oversized product raises
/// surfaces as `badarg` — `convert_time_unit(1000000, nanosecond, 1 bsl 4194239)`
/// ⇒ `badarg` in the oracle (probe: system OTP 29.1.1 / erts-17.1). Any other
/// error is a genuine capability gap and is passed through untouched.
fn trap_arith_class(error: BeamError) -> BeamError {
    match error {
        BeamError::BadArith | BeamError::SystemLimit => badarg(),
        other => other,
    }
}

/// The exact integer term for a conversion result: a small integer when it fits,
/// else the heap bignum the oracle returns (the `crate::arith` promotion
/// convention).
pub(crate) fn int_from_i128(ctx: &mut BifContext<'_>, value: i128) -> Result<Eterm> {
    if let Ok(small) = i64::try_from(value) {
        return arith::int_from_i64(ctx, small);
    }
    // An `i128` magnitude is at most two 64-bit limbs, least significant first —
    // the order `arith::int_from_mag` expects.
    let magnitude = value.unsigned_abs();
    let limbs = [magnitude as u64, (magnitude >> 64) as u64];
    arith::int_from_mag(ctx, value < 0, &limbs)
}

/// `ERTS_MONOTONIC_TIME_UNIT` (`erl_time.h:282-283`): the compile-time monotonic
/// time unit of the 64-bit Unix build, one **nanosecond** per tick
/// (`erl_unix_sys.h:193-198`) — the native unit this module is fixed to.
const MONOTONIC_TIME_UNIT: i64 = 1_000_000_000;

/// `MIN_SMALL` (`erl_term.h:269-270`): the most negative 60-bit small integer,
/// `-(1 << 59)`. Taken from the frozen term crate rather than restated, so the
/// epoch below cannot drift from the small-integer representation.
const MIN_SMALL: i64 = beam_term::MIN_SMALL;

/// `ERTS_MONOTONIC_TIME_START_EXTERNAL` (`erl_time.h:312-323`): the external
/// origin of ERTS monotonic time, i.e. the value `erlang:monotonic_time/0`
/// reports at the instant the VM starts. `(MIN_SMALL / UNIT) * UNIT` with the C's
/// **truncating** division, which is why it is `MIN_SMALL`'s own unit multiple
/// (`erl_time.h:286-288`) — `-576_460_752_000_000_000` on this build (the same
/// constant `beam_rt` answers for `erlang:system_info(start_time)`).
const MONOTONIC_TIME_START_EXTERNAL: i64 =
    (MIN_SMALL / MONOTONIC_TIME_UNIT) * MONOTONIC_TIME_UNIT;

// The C's own invariant, checked when this module compiles: the epoch *need* be a
// multiple of `ERTS_MONOTONIC_TIME_UNIT` (`erl_time.h:286-288`), and every reading
// must stay `> MIN_SMALL` (a small integer) at the instant the VM starts.
const _: () = assert!(MONOTONIC_TIME_START_EXTERNAL % MONOTONIC_TIME_UNIT == 0);
const _: () = assert!(MONOTONIC_TIME_START_EXTERNAL > MIN_SMALL);

/// The process-global monotonic clock that measures **elapsed** time: the
/// [`Instant`] captured on first use.
///
/// This instant is an implementation detail, not what a caller sees — the elapsed
/// count it produces is reported on the ERTS monotonic epoch by
/// [`monotonic_native`].
fn monotonic_start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// `erlang:monotonic_time/0`'s value in native units: the **ERTS monotonic
/// epoch**, `ERTS_MONOTONIC_TIME_START_EXTERNAL + elapsed` (nanoseconds).
///
/// The origin is the compile-time [`MONOTONIC_TIME_START_EXTERNAL`] (`-576460752e9`,
/// what `erlang:system_info(start_time)` answers), so the reading is a large
/// **negative** integer that grows toward zero as the VM runs — exactly the shape
/// the oracle reports (probe, OTP 29.1.1/erts-17.1: `erlang:monotonic_time()` ⇒
/// `-576460751768187266`, `erlang:monotonic_time(second)` ⇒ `-576460752`). It is
/// non-decreasing, and every value is `> system_info(start_time)`.
///
/// The addition is `saturating_add` for two independent reasons: the elapsed count
/// is itself saturated at `i64::MAX` (reachable only after ~292 years of uptime),
/// and the epoch constant is negative, so no wrap can ever turn a monotonic clock
/// into a decreasing one — an unreachable saturation is still better than a
/// wrapping reading.
///
/// The result fits a 60-bit small integer for the first ~36 years of a VM's life
/// (`START_EXTERNAL` is a small, and the small range ends `1.15e18` ns above it);
/// past that `arith::int_from_i64` promotes it to a bignum, as the C's
/// `make_time_val` does (`erl_time_sup.c:2133-2146`).
fn monotonic_native() -> i64 {
    let elapsed = monotonic_start().elapsed();
    let elapsed = i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX);
    MONOTONIC_TIME_START_EXTERNAL.saturating_add(elapsed)
}
/// `system_time` in native units: nanoseconds since the UNIX epoch.
///
/// # Errors
///
/// [`BeamError::SystemLimit`] when the clock is before 1970 (no valid epoch
/// duration) or the nanosecond count would overflow `i64` (year ~2262).
fn system_native() -> Result<i64> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BeamError::SystemLimit)?;
    i64::try_from(since_epoch.as_nanos()).map_err(|_| BeamError::SystemLimit)
}

/// `erlang:monotonic_time/0`.
fn monotonic_time_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    arith::int_from_i64(ctx, monotonic_native())
}

/// `erlang:monotonic_time/1`.
fn monotonic_time_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unit = time_unit(ctx, args[0])?;
    convert_unit(ctx, monotonic_native(), &unit)
}

/// `erlang:system_time/0`.
fn system_time_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    arith::int_from_i64(ctx, system_native()?)
}

/// `erlang:system_time/1`.
fn system_time_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unit = time_unit(ctx, args[0])?;
    convert_unit(ctx, system_native()?, &unit)
}

/// The `system_time - monotonic_time` offset in native units — the value
/// `erlang:time_offset/0,1` reports.
///
/// With the ERTS monotonic epoch the monotonic reading is a large **negative**
/// integer, so this difference is `system_time + |monotonic_time|` — the oracle's
/// own ~74-year constant (probe: `erlang:time_offset()` ⇒
/// `2368668745638096205` ns against a system time of `1792207993869922845` ns).
///
/// # Errors
///
/// [`BeamError::SystemLimit`] from [`system_native`], and for a difference past
/// `i64` — a system clock later than ~year 2243 on this epoch, i.e. the same class
/// of "past `i64`" refusal `system_time/0` reports ~19 years further on. Checked
/// rather than wrapped, so an unrepresentable offset is a typed error and never a
/// silently wrong constant.
fn time_offset_native() -> Result<i64> {
    system_native()?
        .checked_sub(monotonic_native())
        .ok_or(BeamError::SystemLimit)
}

/// `erlang:time_offset/0` — `system_time - monotonic_time` in native units.
fn time_offset_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let offset = time_offset_native()?;
    arith::int_from_i64(ctx, offset)
}

/// `erlang:time_offset/1` — the `system_time - monotonic_time` offset, in `unit`.
fn time_offset_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unit = time_unit(ctx, args[0])?;
    let offset = time_offset_native()?;
    convert_unit(ctx, offset, &unit)
}

/// `erlang:timestamp/0` — `{MegaSecs, Secs, MicroSecs}`, from `system_time/0`
/// (microseconds since the epoch), split as `make_timestamp_value` does
/// (`erl_time_sup.c:1907`).
fn timestamp_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let micros = system_native()?.div_euclid(1_000);
    let mega_secs = micros.div_euclid(1_000_000_000_000);
    let total_secs = micros.div_euclid(1_000_000);
    let secs = total_secs - mega_secs * 1_000_000;
    let micro_secs = micros - total_secs * 1_000_000;

    let mega = ctx.int(mega_secs)?;
    let sec = ctx.int(secs)?;
    let micro = ctx.int(micro_secs)?;
    ctx.tuple(&[mega, sec, micro])
}

/// `erlang:convert_time_unit(Time, FromUnit, ToUnit)` — `Time` expressed from
/// `FromUnit` in `ToUnit`.
///
/// Not a `bif.tab` entry: the oracle implements it as **Erlang code**
/// (`../../erts/preloaded/src/erlang.erl:5338-5384`), which resolves each unit to its
/// units-per-second ratio and computes `TU * Time div FU` (with the `- (FU - 1)`
/// adjustment below for a negative `Time`). It is therefore registered here as a
/// stdlib fast path, exactly like the sibling `erlang:memory/0,1`. Its
/// `native`/`perf_counter` ratios come from `erts_internal:time_unit()`/
/// `perf_counter_unit()` — both `1_000_000_000` on this host, i.e. exactly the native
/// unit this module is fixed to ([`TimeUnit`]).
///
/// The two-unit identity arm (`FU =:= TU` **and** an integer `Time`) returns `Time`
/// **unchanged**, a bignum `Time` included; every other shape is the conversion
/// above. The whole body runs inside `try ... catch _:_ -> error_with_info(badarg, …)`
/// (`erlang.erl:5381-5383`), so each failure below is `badarg` in the oracle and here.
///
/// # Errors
///
/// [`BeamError::BadArg`] for an unknown unit atom, a zero or negative unit, a unit
/// that is neither an atom nor a positive integer, a `Time` that is not an integer
/// (the oracle's `TU * Time` on a non-integer raises `badarith`, which its own catch
/// folds to `badarg`), and an arithmetic product past the bignum cap
/// ([`trap_arith_class`]).
fn convert_time_unit_3(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let time = args[0];
    let from = time_unit(ctx, args[1])?.ratio();
    let to = time_unit(ctx, args[2])?.ratio();

    // `case (FU =:= TU) andalso erlang:is_integer(Time) of true -> Time; ...`, and its
    // `false` arm's `TU * Time ... div FU`, which raises on any non-integer `Time`.
    if !is_integer(time) {
        return Err(badarg());
    }
    if from == to {
        return Ok(time);
    }
    convert_ratio_pair(ctx, time, &from, &to)
}

/// The process-global unique-integer counter: `fetch_add(1)`, so a call yields a/// value no other call yields.
static UNIQUE_COUNTER: AtomicI64 = AtomicI64::new(0);

/// The next unique integer; `positive` maps the raw `0, 1, 2, ...` to
/// `1, 2, 3, ...` so every returned value is `> 0`.
fn next_unique_integer(positive: bool) -> i64 {
    let raw = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    if positive {
        raw + 1
    } else {
        raw
    }
}

/// `erlang:unique_integer/0` — equivalent to the empty modifier list.
fn unique_integer_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    ctx.int(next_unique_integer(false))
}

/// `erlang:unique_integer/1` — `ModifierList :: [positive | monotonic]`.
fn unique_integer_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    // A non-list or an improper list is `badarg` (`erl_bif_unique.c:862`).
    let modifiers = proper_list(args[0]).ok_or_else(badarg)?;

    let mut positive = false;
    for modifier in modifiers {
        if !is_atom(modifier) {
            return Err(badarg());
        }
        match ctx.atoms().name(atom_val(modifier)) {
            Some("positive") => positive = true,
            // `monotonic` requires strictly increasing values, which the single
            // global counter already provides for every option set — no separate
            // state is needed (see the module docs).
            Some("monotonic") => {}
            // An unknown atom (or a non-modifier atom) is `badarg`.
            _ => return Err(badarg()),
        }
    }

    ctx.int(next_unique_integer(positive))
}

/// Registers the `erlang` time/unique-integer BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "monotonic_time", 0, monotonic_time_0);
    reg.register("erlang", "monotonic_time", 1, monotonic_time_1);
    reg.register("erlang", "system_time", 0, system_time_0);
    reg.register("erlang", "system_time", 1, system_time_1);
    reg.register("erlang", "time_offset", 0, time_offset_0);
    reg.register("erlang", "time_offset", 1, time_offset_1);
    reg.register("erlang", "convert_time_unit", 3, convert_time_unit_3);
    reg.register("erlang", "unique_integer", 0, unique_integer_0);
    reg.register("erlang", "unique_integer", 1, unique_integer_1);
    reg.register("erlang", "timestamp", 0, timestamp_0);
}

#[cfg(test)]mod tests {
    use super::*;

    use std::collections::HashSet;

    use beam_alloc::Heap;
    use beam_process::ProcessId;
    use beam_term::{signed_value, AtomTable};

    fn seeded_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "second",
            "seconds",
            "millisecond",
            "milli_seconds",
            "microsecond",
            "micro_seconds",
            "nanosecond",
            "nano_seconds",
            "native",
            "perf_counter",
            "positive",
            "monotonic",
            "fortnight",
            "bogus",
        ]);
        atoms
    }

    fn context<'a>(heap: &'a mut Heap, atoms: &'a AtomTable) -> BifContext<'a> {
        BifContext::new(heap, atoms, ProcessId::new(1, 0))
    }

    /// The `i64` value of a small-integer result term.
    fn as_int(term: Eterm) -> i64 {
        assert!(
            crate::access::is_small(term),
            "expected a small integer result, got a non-small term"
        );
        signed_value(term)
    }

    /// The `f64` value of an integer result term, small integer or bignum (the
    /// native-nanosecond readings are past the 60-bit small range).
    fn as_number(term: Eterm) -> f64 {
        arith::integer_to_f64(term).expect("an integer result")
    }

    /// The exact integer value of a conversion result, in base 10, through the
    /// crate's own bignum renderer (`integer_digits` — the renderer
    /// `integer_to_list/1` uses). The oracle goldens below are written in this
    /// shape.
    fn decimal(term: Eterm) -> String {
        crate::numeric::integer_digits(term, 10).expect("an integer result")
    }

    /// The `i128` value of a conversion result. Every conversion the cases below
    /// compare numerically fits `i128` (the ratios are at most `1e9`/s and the
    /// native counts at most `1.5e9`).
    fn convert_int(ctx: &mut BifContext<'_>, native: i64, unit: &TimeUnit) -> i128 {
        let term = convert_unit(ctx, native, unit).expect("an accepted unit");
        decimal(term).parse().expect("an i128-sized result")
    }

    /// The `PerSecondBig` unit for `1 bsl bits` — the arbitrary-precision "units per
    /// second" form (`1 bsl 60` and above is a bignum).
    fn unit_from_shift(ctx: &mut BifContext<'_>, bits: usize) -> TimeUnit {
        let mut limbs = vec![0u64; bits / 64 + 1];
        limbs[bits / 64] = 1u64 << (bits % 64);
        let term = arith::int_from_mag(ctx, false, &limbs).expect("a bignum");
        time_unit(ctx, term).expect("a legal unit")
    }
    /// A heap big enough for the tuples/lists these tests build.
    fn heap() -> Heap {
        Heap::new(256).expect("test heap")
    }

    /// `ERTS_MONOTONIC_TIME_START_EXTERNAL` — the value
    /// `erlang:system_info(start_time)` answers (probe: `-576460752000000000`).
    const ERTS_START: i64 = MONOTONIC_TIME_START_EXTERNAL;

    /// `ERTS_MONOTONIC_TIME_END_EXTERNAL` — the value `erlang:system_info(end_time)`
    /// answers (probe: OTP 29.1.1 / erts-17.1 ⇒ `8646911283000000000`). The mirror of
    /// `beam_rt`'s `START_TIME`/`END_TIME`, derived from the same C macros here so the
    /// crate owns no second hand-written copy of the numbers.
    const ERTS_END: i64 = ERTS_START
        + ((i64::MAX / MONOTONIC_TIME_UNIT) * MONOTONIC_TIME_UNIT - MONOTONIC_TIME_UNIT);

    #[test]
    fn the_epoch_constants_are_the_erts_interface_values() {
        // The C `ERTS_MONOTONIC_TIME_START_EXTERNAL`/`_END_EXTERNAL` of the 64-bit Unix
        // build, cross-checked against `erlang:system_info(start_time|end_time)` on OTP
        // 29.1.1 / erts-17.1 (`-576460752000000000 8646911283000000000`).
        assert_eq!(ERTS_START, -576_460_752_000_000_000);
        assert_eq!(ERTS_END, 8_646_911_283_000_000_000);
        assert!(ERTS_START < 0 && ERTS_START > beam_term::MIN_SMALL);
        assert!(ERTS_END > beam_term::MAX_SMALL, "end_time is a bignum");
    }

    /// `monotonic_time/0` sits **inside the ERTS monotonic epoch**: above
    /// `system_info(start_time)` and below `system_info(end_time)` — so it is a large
    /// *negative* small integer just after a VM starts (probe:
    /// `erlang:monotonic_time()` ⇒ `-576460751768187266`), and it never decreases.
    #[test]
    fn monotonic_time_is_inside_the_erts_epoch_and_non_decreasing() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let a = as_int(monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0"));
        let b = as_int(monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0"));
        assert!(a > ERTS_START, "monotonic_time/0 {a} must be above start_time {ERTS_START}");
        assert!(a < ERTS_END, "monotonic_time/0 {a} must be below end_time {ERTS_END}");
        assert!(b >= a, "monotonic_time/0 decreased: {a} then {b}");
        // Negative by design: the epoch origin is `start_time`, so a reading only
        // turns non-negative after ~18 years of uptime.
        assert!(a < 0, "monotonic_time/0 must be negative on the ERTS epoch: {a}");
        // The elapsed half is this run's own uptime — seconds at most, never the
        // ~36 years the small-integer headroom allows.
        let elapsed = a - ERTS_START;
        assert!(
            (0..1_000_000_000_000).contains(&elapsed),
            "monotonic_time/0 is {elapsed} ns into the epoch — not a plausible uptime"
        );
    }

    /// `monotonic_time(millisecond)` is the **floor** of the bracketing native
    /// readings. The epoch makes every reading negative, where `/` truncates toward
    /// zero and would be off by one — `div_euclid` is the ERTS rounding.
    #[test]
    fn monotonic_millisecond_tracks_nanoseconds() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let nanosecond = ctx.atom("nanosecond").expect("seeded");
        let millisecond = ctx.atom("millisecond").expect("seeded");

        let lo = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));
        let ms = as_int(monotonic_time_1(&mut ctx, &[millisecond]).expect("ms"));
        let hi = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));

        assert!(lo <= hi, "the native clock went backwards: {lo} then {hi}");
        // Every reading lies in the floored [lo, hi] bracket.
        assert!(
            lo.div_euclid(1_000_000) <= ms && ms <= hi.div_euclid(1_000_000),
            "monotonic_time(millisecond) = {ms} not within [{}, {}]",
            lo.div_euclid(1_000_000),
            hi.div_euclid(1_000_000)
        );
        // ... and truncation is *observably* wrong on this epoch, which is what makes
        // the `div_euclid` above load-bearing rather than cosmetic: a reading half a
        // millisecond below a whole millisecond is `-1` ms, not `0`.
        let sub_millisecond = -500_000_i64;
        assert_eq!(sub_millisecond.div_euclid(1_000_000), -1);
        assert_eq!(sub_millisecond / 1_000_000, 0);
        // The real reading is negative too, so it takes the same branch.
        assert!(lo < 0 && hi < 0, "the ERTS epoch readings are negative");
    }

    /// `/1` consistency, driven through the **registry**: `monotonic_time(U)` is
    /// `erlang:convert_time_unit(monotonic_time(), native, U)` for every accepted
    /// unit — the atom set plus a positive integer unit (`1000` ⇒ milliseconds).
    ///
    /// The two forms are separate clock reads, so the pin brackets the `/1` reading
    /// with two native ones and requires the registered `/3` conversion of both to
    /// enclose it: a `/1` that were scaled or rounded differently (truncation instead
    /// of the floor on this now-negative epoch, say) leaves the bracket.
    #[test]
    fn monotonic_time_1_agrees_with_the_registered_convert_time_unit() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut registry = BifRegistry::new();
        register(&mut registry);
        let mut ctx = context(&mut heap, &atoms);

        let native = ctx.atom("native").expect("seeded");
        let thousand = ctx.int(1000).expect("fits");
        let units = [
            ("second", ctx.atom("second").expect("seeded")),
            ("millisecond", ctx.atom("millisecond").expect("seeded")),
            ("microsecond", ctx.atom("microsecond").expect("seeded")),
            ("nanosecond", ctx.atom("nanosecond").expect("seeded")),
            ("native", native),
            ("perf_counter", ctx.atom("perf_counter").expect("seeded")),
            ("1000", thousand),
        ];
        for (label, unit) in units {
            let lo = monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0");
            let value = monotonic_time_1(&mut ctx, &[unit]).expect("monotonic_time/1");
            let hi = monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0");
            let converted = |ctx: &mut BifContext<'_>, time: Eterm| {
                decimal(
                    registry
                        .call("erlang", "convert_time_unit", 3, ctx, &[time, native, unit])
                        .expect("erlang:convert_time_unit/3"),
                )
                .parse::<i128>()
                .expect("an i128-sized conversion")
            };
            let (converted_lo, value, converted_hi) = (
                converted(&mut ctx, lo),
                decimal(value).parse::<i128>().expect("an i128-sized reading"),
                converted(&mut ctx, hi),
            );
            assert!(
                converted_lo <= value && value <= converted_hi,
                "monotonic_time({label}) = {value} is outside \
                 convert_time_unit({}, native, {label}) = [{converted_lo}, {converted_hi}]",
                decimal(lo)
            );
        }
    }

    /// The ERTS epoch keeps the oracle's `system_time() = monotonic_time() +
    /// time_offset()` invariant **and** puts the epoch magnitude into the offset:
    /// `time_offset() - system_time()` is `|start_time|` less this VM's uptime
    /// (probe: `1792207993869922845 + 576460752000000000 ≈ 2368668745…`).
    ///
    /// Both values are bignums on this epoch, so they are compared as `f64` — its
    /// granularity at ~2.4e18 is a few hundred nanoseconds, far below the tolerances
    /// the read skew between the four clock calls forces.
    #[test]
    fn the_offset_carries_the_erts_epoch() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let mono1 = as_number(monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0"));
        let sys = as_number(system_time_0(&mut ctx, &[]).expect("system_time/0"));
        let off = as_number(time_offset_0(&mut ctx, &[]).expect("time_offset/0"));
        let mono2 = as_number(monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0"));

        // The reads happen microseconds apart; the bracket measures how far apart, and
        // one millisecond of slack covers a preemption between two BIF calls.
        let span = (mono2 - mono1).max(0.0) + 1.0e6;
        assert!(
            (off - (sys - mono1)).abs() <= span && (off - (sys - mono2)).abs() <= span,
            "system_time ({sys}) is not monotonic_time + time_offset ({off}) \
             within the {span} ns the reads span"
        );
        // The epoch itself: `off - sys` is `|start_time|` minus the uptime (a
        // sub-second value here), so an offset measured from *zero* would be off by
        // the whole `-576460752e9`.
        let epoch = -ERTS_START as f64;
        assert!(
            (off - sys - epoch).abs() <= span + 1.0e9,
            "time_offset - system_time = {} is not the ERTS epoch {epoch}",
            off - sys
        );
    }    /// Every accepted unit spelling names its exact ratio, and the conversion is
    /// `native * per_second div 1_000_000_000` flooring toward negative infinity
    /// (the `erlang:convert_time_unit/3` rounding, pinned against the oracle:
    /// `convert_time_unit(-1500000000, nanosecond, second)` => `-2`).
    #[test]
    fn every_accepted_unit_names_its_per_second_ratio() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);
        let native = ctx.atom("native").expect("seeded");
        let perf_counter = ctx.atom("perf_counter").expect("seeded");
        let nanosecond = ctx.atom("nanosecond").expect("seeded");
        let nano_seconds = ctx.atom("nano_seconds").expect("seeded");
        let microsecond = ctx.atom("microsecond").expect("seeded");
        let micro_seconds = ctx.atom("micro_seconds").expect("seeded");
        let millisecond = ctx.atom("millisecond").expect("seeded");
        let milli_seconds = ctx.atom("milli_seconds").expect("seeded");
        let second = ctx.atom("second").expect("seeded");
        let seconds = ctx.atom("seconds").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let thousand = ctx.int(1000).expect("fits");
        let million = ctx.int(1_000_000).expect("fits");
        let nano = ctx.int(1_000_000_000).expect("fits");
        let two = ctx.int(2).expect("fits");
        let nine_nine_nine = ctx.int(999).expect("fits");

        let cases = [
            (native, 1_000_000_000i128),
            (perf_counter, 1_000_000_000),
            (nanosecond, 1_000_000_000),
            (nano_seconds, 1_000_000_000),
            (microsecond, 1_000_000),
            (micro_seconds, 1_000_000),
            (millisecond, 1_000),
            (milli_seconds, 1_000),
            (second, 1),
            (seconds, 1),
            // The integer forms of `erlang:convert_time_unit/3`: units per second.
            (one, 1),
            (thousand, 1_000),
            (million, 1_000_000),
            (nano, 1_000_000_000),
            (two, 2),
            (nine_nine_nine, 999),
        ];
        for (term, per_second) in cases {
            let unit = time_unit(&ctx, term).expect("an accepted unit");
            // The ratio is the value one whole second converts to.
            assert_eq!(
                convert_int(&mut ctx, 1_000_000_000, &unit),
                per_second,
                "wrong ratio for {unit:?}"
            );

            // The exact conversion of a positive, a zero and a negative native
            // count — the last one floors toward negative infinity.
            assert_eq!(
                convert_int(&mut ctx, 1_500_000_000, &unit),
                1_500_000_000 * per_second / 1_000_000_000
            );
            assert_eq!(convert_int(&mut ctx, 0, &unit), 0);
            assert_eq!(
                convert_int(&mut ctx, -1_500_000_000, &unit),
                (-1_500_000_000 * per_second).div_euclid(1_000_000_000)
            );
        }

        // The oracle-pinned floor cases.
        assert_eq!(convert_int(&mut ctx, -1_500_000_000, &TimeUnit::Second), -2);
        assert_eq!(
            convert_int(&mut ctx, -1_500_000_000, &TimeUnit::Millisecond),
            -1_500
        );
        assert_eq!(
            convert_int(&mut ctx, -1_500_000_000, &TimeUnit::Microsecond),
            -1_500_000
        );
        assert_eq!(
            convert_int(&mut ctx, -1_500_000_000, &TimeUnit::Nanosecond),
            -1_500_000_000
        );
        assert_eq!(convert_int(&mut ctx, -1, &TimeUnit::Nanosecond), -1);
        assert_eq!(convert_int(&mut ctx, 1, &TimeUnit::Nanosecond), 1);
        assert_eq!(convert_int(&mut ctx, 1, &TimeUnit::Second), 0);
    }

    /// Every alias is accepted by all three `/1` BIFs and reads the clock with its
    /// canonical ratio (`seconds`/`second`, `milli_seconds`/`millisecond`,
    /// `micro_seconds`/`microsecond`, `nano_seconds`/`nanosecond`,
    /// `perf_counter`/`native`).
    #[test]
    fn every_alias_is_accepted_by_all_three_unit_bifs() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        // (alias, the number of nanoseconds in one of it)
        let aliases = [
            (ctx.atom("second").expect("seeded"), 1.0e9f64),
            (ctx.atom("seconds").expect("seeded"), 1.0e9),
            (ctx.atom("millisecond").expect("seeded"), 1.0e6),
            (ctx.atom("milli_seconds").expect("seeded"), 1.0e6),
            (ctx.atom("microsecond").expect("seeded"), 1.0e3),
            (ctx.atom("micro_seconds").expect("seeded"), 1.0e3),
            (ctx.atom("nanosecond").expect("seeded"), 1.0),
            (ctx.atom("nano_seconds").expect("seeded"), 1.0),
            (ctx.atom("native").expect("seeded"), 1.0),
            (ctx.atom("perf_counter").expect("seeded"), 1.0),
        ];

        for (arg, ns_per_unit) in aliases {
            let mono = as_number(monotonic_time_1(&mut ctx, &[arg]).expect("monotonic_time/1"));
            // The monotonic epoch is negative, and every reading lies between
            // `system_info(start_time)` and `(end_time)` expressed in the same unit
            // (both constants are exact multiples of any whole-second unit).
            let epoch_start = ERTS_START as f64 / ns_per_unit;
            let epoch_end = ERTS_END as f64 / ns_per_unit;
            assert!(
                mono >= epoch_start && mono < epoch_end,
                "monotonic_time/1 ({mono}) is outside the ERTS epoch [{epoch_start}, {epoch_end})"
            );

            let sys = as_number(system_time_1(&mut ctx, &[arg]).expect("system_time/1"));
            assert!(sys > 0.0, "system_time/1 must be positive: {sys}");

            let off = as_number(time_offset_1(&mut ctx, &[arg]).expect("time_offset/1"));
            assert!(off > 0.0, "time_offset/1 must be positive: {off}");

            // `time_offset = system_time - monotonic_time` (the invariant the
            // oracle exhibits). The three reads happen within microseconds of each
            // other and each is floored to a whole unit, so a generous
            // ten-milliseconds-worth of the unit covers the flooring.
            let gap = (off - (sys - mono)).abs();
            assert!(
                gap <= 2.0 + 1.0e7 / ns_per_unit,
                "time_offset/1 ({off}) vs system_time/1 ({sys}) - monotonic_time/1 ({mono})"
            );
        }
    }

    /// The aliases share a ratio with their canonical spelling: each alias reads
    /// the same coarse bracket as the canonical unit (and `perf_counter` the same
    /// as `native`). A wrongly-scaled alias (say `seconds` mapped to `1e9`/s)
    /// falls outside the bracket.
    #[test]
    fn every_time_unit_is_converted_consistently() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let second = ctx.atom("second").expect("seeded");
        let seconds = ctx.atom("seconds").expect("seeded");
        let millisecond = ctx.atom("millisecond").expect("seeded");
        let milli_seconds = ctx.atom("milli_seconds").expect("seeded");
        let microsecond = ctx.atom("microsecond").expect("seeded");
        let micro_seconds = ctx.atom("micro_seconds").expect("seeded");
        let nanosecond = ctx.atom("nanosecond").expect("seeded");
        let nano_seconds = ctx.atom("nano_seconds").expect("seeded");
        let native = ctx.atom("native").expect("seeded");
        let perf_counter = ctx.atom("perf_counter").expect("seeded");

        // (unit, the number of nanoseconds in one of it)
        let cases = [
            (second, 1_000_000_000i64),
            (seconds, 1_000_000_000),
            (millisecond, 1_000_000),
            (milli_seconds, 1_000_000),
            (microsecond, 1_000),
            (micro_seconds, 1_000),
            (nanosecond, 1),
            (nano_seconds, 1),
            (native, 1),
            (perf_counter, 1),
        ];
        for (unit, ns_per_unit) in cases {
            // Bracket the reading between two nanosecond readings. The monotonic
            // epoch is negative, so the bracket must **floor** (`div_euclid`), not
            // truncate toward zero, exactly as `convert_time_unit/3` does.
            let lo = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));
            let value = as_int(monotonic_time_1(&mut ctx, &[unit]).expect("unit"));
            let hi = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));
            assert!(
                lo.div_euclid(ns_per_unit) <= value && value <= hi.div_euclid(ns_per_unit),
                "a reading of {value} is outside [{}, {}]",
                lo.div_euclid(ns_per_unit),
                hi.div_euclid(ns_per_unit)
            );
        }
    }

    #[test]
    fn a_bad_unit_is_badarg() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let fortnight = ctx.atom("fortnight").expect("seeded");
        // A float, a list and a tuple are not units; neither is `0` or a negative
        // integer (`time_unit_conversion`'s `signed_val(term) <= 0`).
        let a_float = ctx.make_float(1.0).expect("fits");
        let a_list = ctx.list(&[]).expect("fits");
        let zero = ctx.int(0).expect("fits");
        let a_tuple = ctx.tuple(&[zero]).expect("fits");
        let minus_one = ctx.int(-1).expect("fits");
        let minus_thousand = ctx.int(-1000).expect("fits");

        for arg in [
            fortnight,
            a_float,
            a_list,
            a_tuple,
            zero,
            minus_one,
            minus_thousand,
        ] {
            assert!(
                matches!(monotonic_time_1(&mut ctx, &[arg]), Err(BeamError::BadArg)),
                "monotonic_time/1 accepted a bad unit"
            );
            assert!(
                matches!(system_time_1(&mut ctx, &[arg]), Err(BeamError::BadArg)),
                "system_time/1 accepted a bad unit"
            );
            assert!(
                matches!(time_offset_1(&mut ctx, &[arg]), Err(BeamError::BadArg)),
                "time_offset/1 accepted a bad unit"
            );
        }
    }

    /// A **bignum** unit: a negative one is `badarg` (the C's `big_sign`), while a
    /// positive one is a legal unit every `/1` BIF accepts and converts in
    /// arbitrary precision ([`convert_unit_big`]) — never a capability gap.
    #[test]
    fn a_positive_bignum_unit_is_a_legal_unit_and_a_negative_one_is_badarg() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        // 2^64 + 1: far past the 60-bit small-integer range.
        let limbs = [1u64, 1u64];
        let positive = arith::int_from_mag(&mut ctx, false, &limbs).expect("a bignum");
        let negative = arith::int_from_mag(&mut ctx, true, &limbs).expect("a bignum");

        let unit = time_unit(&ctx, positive).expect("a positive bignum is a legal unit");
        assert_eq!(unit, TimeUnit::PerSecondBig(vec![1, 1]));

        // A whole second is exactly the unit's own ratio, zero stays zero, and a
        // negative reading floors toward negative infinity.
        assert_eq!(convert_int(&mut ctx, 1_000_000_000, &unit), (1i128 << 64) + 1);
        assert_eq!(convert_int(&mut ctx, 0, &unit), 0);
        assert_eq!(
            convert_int(&mut ctx, -1_500_000_000, &unit),
            (-1_500_000_000i128 * ((1i128 << 64) + 1)).div_euclid(1_000_000_000)
        );

        // Every `/1` BIF accepts the positive unit (the reading is a bignum: epoch
        // seconds times `2^64 + 1`) and rejects the negated one as `badarg`.
        assert!(monotonic_time_1(&mut ctx, &[positive]).is_ok());
        assert!(system_time_1(&mut ctx, &[positive]).is_ok());
        assert!(time_offset_1(&mut ctx, &[positive]).is_ok());
        assert!(matches!(
            monotonic_time_1(&mut ctx, &[negative]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            system_time_1(&mut ctx, &[negative]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            time_offset_1(&mut ctx, &[negative]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(time_unit(&ctx, negative), Err(BeamError::BadArg)));
    }

    /// The bignum-unit conversion is exactly
    /// `erlang:convert_time_unit(Time, nanosecond, Unit)` in arbitrary precision:
    /// every golden below is the oracle's own output, including the
    /// floor-toward-negative-infinity rounding of a negative reading (probe: system
    /// OTP 29.1.1 / erts-17.1).
    #[test]
    fn bignum_units_convert_exactly_like_convert_time_unit() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let unit_200 = unit_from_shift(&mut ctx, 200);
        let unit_60 = unit_from_shift(&mut ctx, 60);
        let unit_70 = unit_from_shift(&mut ctx, 70);

        let cases: [(i64, &TimeUnit, &str); 8] = [
            (
                1_000_000,
                &unit_200,
                "1606938044258990275541962092341162602522202993782792835301",
            ),
            (
                1,
                &unit_200,
                "1606938044258990275541962092341162602522202993782792",
            ),
            (0, &unit_200, "0"),
            (
                -1_000_000,
                &unit_200,
                "-1606938044258990275541962092341162602522202993782792835302",
            ),
            (
                -1,
                &unit_200,
                "-1606938044258990275541962092341162602522202993782793",
            ),
            (1_000_000_001, &unit_60, "1152921505759768480"),
            (-1_000_000_001, &unit_60, "-1152921505759768481"),
            (1_500_000_000, &unit_70, "1770887431076116955136"),
        ];
        for (native, unit, expected) in cases {
            let term = convert_unit(&mut ctx, native, unit).expect("a legal unit");
            assert_eq!(decimal(term), expected, "{native} ns as `{unit:?}`");
        }
    }

    /// The `BIG_ARITY_MAX` boundary: `1 bsl 4194239` is the largest legal bignum
    /// (65535 limbs) and converts, while a reading whose product with it needs one
    /// limb more is `badarg` — the oracle's own behaviour, its
    /// `erlang:convert_time_unit/3` folding the `system_limit` into `badarg`
    /// (`convert_time_unit(1000000, nanosecond, 1 bsl 4194239)`).
    #[test]
    fn an_oversized_bignum_unit_product_is_badarg() {
        // A `BIG_ARITY_MAX`-sized bignum is 65536 words, and the unit, the product
        // and the quotient are on this heap at once.
        let mut heap = Heap::new(1 << 19).expect("the boundary heap");
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let unit = unit_from_shift(&mut ctx, 4_194_239);

        let term = convert_unit(&mut ctx, 1, &unit).expect("the largest legal unit");
        let (negative, digits) = beam_codec::term::big_parts(term).expect("a bignum");
        assert!(!negative, "a positive reading converts to a positive value");
        assert_eq!(
            digits.len(),
            65_535,
            "`1 bsl 4194239 div 1e9` still fills BIG_ARITY_MAX limbs"
        );

        // `1000000 * (1 bsl 4194239)` needs one limb more than the cap.
        assert!(matches!(
            convert_unit(&mut ctx, 1_000_000, &unit),
            Err(BeamError::BadArg)
        ));
    }
    /// A coarse **integer** unit converts exactly like the oracle (units per
    /// second), and a result past the 60-bit small range promotes to a bignum.
    #[test]
    fn integer_units_convert_and_promote() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let second = ctx.atom("second").expect("seeded");
        let millisecond = ctx.atom("millisecond").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let thousand = ctx.int(1000).expect("fits");
        let two = ctx.int(2).expect("fits");
        let very_coarse = ctx.int(1 << 58).expect("a small integer");

        let secs = as_int(system_time_1(&mut ctx, &[second]).expect("second"));
        let by_one = as_int(system_time_1(&mut ctx, &[one]).expect("1"));
        assert_eq!(by_one, secs, "`1` is the second unit");

        // The integer forms read the same coarse bracket as their atom spelling.
        for (atom_unit, integer_unit) in [(second, one), (millisecond, thousand)] {
            let lo = as_int(system_time_1(&mut ctx, &[atom_unit]).expect("atom"));
            let value = as_int(system_time_1(&mut ctx, &[integer_unit]).expect("integer"));
            let hi = as_int(system_time_1(&mut ctx, &[atom_unit]).expect("atom"));
            assert!(
                lo <= value && value <= hi,
                "the integer and atom spellings of a unit disagree: {value} vs [{lo}, {hi}]"
            );
        }

        // `2` units per second is twice the second reading.
        let by_two = as_int(system_time_1(&mut ctx, &[two]).expect("2"));
        assert!(
            (by_two - 2 * secs).abs() <= 1,
            "system_time(2) = {by_two} is not 2 x {secs}"
        );

        // A unit of `2^58`/s (the value is `seconds * 2^58`, so it is far past the
        // small range) is materialized as a heap bignum — never a saturated or
        // truncated value.
        let promoted = system_time_1(&mut ctx, &[very_coarse]).expect("2^58");
        let value = as_number(promoted);
        let expected = (secs as f64) * ((1u64 << 58) as f64);
        assert!(
            beam_codec::term::big_parts(promoted).is_ok(),
            "a value past 2^59 must be a heap bignum"
        );
        assert!(
            (value - expected).abs() < expected * 0.01,
            "system_time(2^58) = {value} is not the expected magnitude {expected}"
        );
    }

    #[test]
    fn system_time_is_a_plausible_epoch_value() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let second = ctx.atom("second").expect("seeded");
        let millisecond = ctx.atom("millisecond").expect("seeded");

        // Between 2020-01-01 and 2100-01-01.
        let secs = as_int(system_time_1(&mut ctx, &[second]).expect("system_time(second)"));
        assert!(
            (1_577_836_800..4_102_444_800).contains(&secs),
            "system_time(second) = {secs} is not a plausible epoch value"
        );

        let ms = as_int(system_time_1(&mut ctx, &[millisecond]).expect("system_time(ms)"));
        assert!(ms >= secs * 1_000, "millisecond form must be the coarser");
    }

    #[test]
    fn native_nanosecond_values_promote_to_a_bignum() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        // An epoch-nanosecond value exceeds the 60-bit small range for any date
        // after 1988, so `system_time/0` (and `time_offset/0`) promote to a
        // *bignum*, exactly as the oracle does — never a truncated value and never
        // `system_limit`.
        let term = system_time_0(&mut ctx, &[]).expect("system_time/0");
        assert!(
            !crate::access::is_small(term),
            "a post-1988 epoch-nanosecond count must be a bignum"
        );
        let (negative, digits) = beam_codec::term::big_parts(term).expect("a bignum");
        assert!(!negative, "system_time/0 must not be negative");
        assert!(!digits.is_empty(), "a bignum has at least one limb");

        // Its magnitude is a plausible epoch nanoseconds value (2020..2100).
        let value = crate::arith::integer_to_f64(term).expect("round-trips to f64");
        assert!(
            (1_577_836_800_000_000_000.0..4_102_444_800_000_000_000.0).contains(&value),
            "system_time/0 = {value} is not a plausible epoch value"
        );

        // `time_offset/0` is `system_time - monotonic_time`, so it is the same
        // scale and likewise a bignum — and, on the ERTS epoch, it exceeds the
        // system time by the whole `|start_time|` (less this VM's uptime).
        let offset = time_offset_0(&mut ctx, &[]).expect("time_offset/0");
        assert!(
            !crate::access::is_small(offset),
            "time_offset/0 must be a bignum too"
        );
        let offset_value = crate::arith::integer_to_f64(offset).expect("round-trips to f64");
        assert!(
            (offset_value - value - (-ERTS_START as f64)).abs() < 1.0e9,
            "time_offset/0 = {offset_value} is not {value} plus the epoch {}",
            -ERTS_START
        );
    }

    #[test]
    fn time_offset_reproduces_system_minus_monotonic() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let millisecond = ctx.atom("millisecond").expect("seeded");
        let mono = as_int(monotonic_time_1(&mut ctx, &[millisecond]).expect("mono"));
        let off = as_int(time_offset_1(&mut ctx, &[millisecond]).expect("offset"));
        let sys = as_int(system_time_1(&mut ctx, &[millisecond]).expect("sys"));

        // The invariant `system_time = monotonic_time + time_offset` holds up to
        // the flooring and the few microseconds between the three clock reads.
        assert!(
            (off - (sys - mono)).abs() <= 2,
            "time_offset({off}) vs system({sys}) - monotonic({mono})"
        );
    }

    #[test]
    fn timestamp_is_a_mega_sec_micro_sec_triple() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let ts = timestamp_0(&mut ctx, &[]).expect("timestamp/0");
        assert!(crate::access::is_tuple(ts), "timestamp/0 must be a tuple");
        assert_eq!(crate::access::tuple_size(ts), 3);

        let mega = as_int(crate::access::tuple_element(ts, 0));
        let sec = as_int(crate::access::tuple_element(ts, 1));
        let micro = as_int(crate::access::tuple_element(ts, 2));
        assert!(mega >= 0, "megasecs: {mega}");
        assert!((0..1_000_000).contains(&sec), "secs: {sec}");
        assert!((0..1_000_000).contains(&micro), "microsecs: {micro}");

        // Reconstructed microseconds agree with `system_time(microsecond)`.
        let total_us = mega * 1_000_000_000_000 + sec * 1_000_000 + micro;
        let microsecond = ctx.atom("microsecond").expect("seeded");
        let sys_us = as_int(system_time_1(&mut ctx, &[microsecond]).expect("sys us"));
        assert!(
            (total_us - sys_us).abs() < 1_000_000,
            "timestamp {total_us} us vs system_time {sys_us} us"
        );
    }

    #[test]
    fn unique_integer_0_values_are_distinct() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let mut seen = HashSet::new();
        for _ in 0..1_000 {
            let value = as_int(unique_integer_0(&mut ctx, &[]).expect("unique_integer/0"));
            assert!(value >= 0, "unique_integer/0 must be non-negative: {value}");
            assert!(seen.insert(value), "duplicate unique integer {value}");
        }
    }

    #[test]
    fn unique_integer_1_matches_the_oracle_shape() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let positive = ctx.atom("positive").expect("seeded");
        let monotonic = ctx.atom("monotonic").expect("seeded");

        // `[positive]` returns a positive *integer* (never a `{Int, Bool}` tuple).
        let pos_opts = ctx.list(&[positive]).expect("fits");
        let p = as_int(unique_integer_1(&mut ctx, &[pos_opts]).expect("positive"));
        assert!(p > 0, "unique_integer([positive]) = {p} is not positive");

        // `[monotonic]` is strictly increasing.
        let mono_opts = ctx.list(&[monotonic]).expect("fits");
        let mut prev = i64::MIN;
        for _ in 0..8 {
            let v = as_int(unique_integer_1(&mut ctx, &[mono_opts]).expect("monotonic"));
            assert!(v > prev, "unique_integer([monotonic]) not increasing: {v} after {prev}");
            prev = v;
        }

        // `[positive, monotonic]` is positive *and* strictly increasing.
        let pm_opts = ctx.list(&[positive, monotonic]).expect("fits");
        let mut prev = 0;
        for _ in 0..8 {
            let v = as_int(unique_integer_1(&mut ctx, &[pm_opts]).expect("positive, monotonic"));
            assert!(v > 0 && v > prev, "unique_integer([positive, monotonic]) = {v} after {prev}");
            prev = v;
        }

        // `[]` is the same generator as `unique_integer/0`.
        let empty = ctx.list(&[]).expect("fits");
        let e = as_int(unique_integer_1(&mut ctx, &[empty]).expect("empty list"));
        assert!(e >= 0);

        // A repeated valid modifier is ignored.
        let repeated = ctx.list(&[positive, positive]).expect("fits");
        assert!(as_int(unique_integer_1(&mut ctx, &[repeated]).expect("repeated")) > 0);
    }

    #[test]
    fn unique_integer_1_rejects_bad_modifier_lists() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let positive = ctx.atom("positive").expect("seeded");
        let bogus = ctx.atom("bogus").expect("seeded");

        let unknown = ctx.list(&[bogus]).expect("fits");
        assert!(matches!(
            unique_integer_1(&mut ctx, &[unknown]),
            Err(BeamError::BadArg)
        ));

        let not_a_list = ctx.int(1).expect("fits");
        assert!(matches!(
            unique_integer_1(&mut ctx, &[not_a_list]),
            Err(BeamError::BadArg)
        ));

        // `[positive | bogus]` (improper) is `badarg`.
        let improper = ctx.cons(positive, bogus).expect("fits");
        assert!(matches!(
            unique_integer_1(&mut ctx, &[improper]),
            Err(BeamError::BadArg)
        ));

        // A non-atom element is `badarg`.
        let one = ctx.int(1).expect("fits");
        let non_atom = ctx.list(&[one]).expect("fits");
        assert!(matches!(
            unique_integer_1(&mut ctx, &[non_atom]),
            Err(BeamError::BadArg)
        ));
    }

    /// The bignum `±(1 bsl bits)`, built with the crate's own promotion helper.
    fn bignum_term(ctx: &mut BifContext<'_>, negative: bool, bits: usize) -> Eterm {
        let mut limbs = vec![0u64; bits / 64 + 1];
        limbs[bits / 64] = 1u64 << (bits % 64);
        arith::int_from_mag(ctx, negative, &limbs).expect("a bignum")
    }

    /// Calls `erlang:convert_time_unit/3`.
    fn convert(ctx: &mut BifContext<'_>, time: Eterm, from: Eterm, to: Eterm) -> Result<Eterm> {
        convert_time_unit_3(ctx, &[time, from, to])
    }

    /// A `convert_time_unit/3` unit argument: a seeded atom, or a caller-supplied
    /// integer ratio (units per second).
    enum Unit<'a> {
        Atom(&'a str),
        Int(i64),
    }

    /// The oracle goldens of `erlang:convert_time_unit/3` whose result fits a small
    /// integer, every one probed against OTP 29.1.1 / erts-17.1.
    #[test]
    fn convert_time_unit_3_matches_the_oracle_small_int_goldens() {
        use Unit::{Atom, Int};

        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        // `(Time, FromUnit, ToUnit) => oracle result`. The unit spellings cover each
        // modern atom and its deprecated alias, the `FU =:= TU` identity (including a
        // distinct spelling of the same ratio), a caller-supplied integer ratio, and
        // the two native aliases (both `1e9`/s here).
        let cases: &[(i64, Unit, Unit, i64)] = &[
            (1000, Atom("second"), Atom("microsecond"), 1_000_000_000),
            (1000, Atom("nanosecond"), Atom("microsecond"), 1),
            (5, Atom("second"), Atom("seconds"), 5),
            (1_500_000_000, Atom("nanosecond"), Int(1_000_000_000), 1_500_000_000),
            (-1, Atom("second"), Atom("millisecond"), -1_000),
            (-1500, Atom("millisecond"), Atom("second"), -2),
            (-3, Atom("millisecond"), Int(2), -1),
            (-4, Atom("millisecond"), Int(2), -1),
            (-1, Atom("nanosecond"), Atom("second"), -1),
            (-1_000_000_001, Atom("nanosecond"), Atom("second"), -2),
            (-1, Atom("second"), Atom("nanosecond"), -1_000_000_000),
            (1, Atom("seconds"), Atom("second"), 1),
            (1, Atom("milli_seconds"), Atom("millisecond"), 1),
            (1, Atom("micro_seconds"), Atom("microsecond"), 1),
            (1, Atom("nano_seconds"), Atom("nanosecond"), 1),
            (1, Atom("native"), Atom("second"), 0),
            (1, Atom("perf_counter"), Atom("nanosecond"), 1),
            (1, Int(1000), Atom("millisecond"), 1),
            (1, Atom("native"), Int(1_000_000_000), 1),
            (1, Int(1000), Int(1000), 1),
            (1_000_000, Atom("microsecond"), Atom("native"), 1_000_000_000),
        ];

        for &(time, ref from, ref to, expected) in cases {
            let from = match from {
                Atom(name) => ctx.atom(name).expect("seeded"),
                Int(value) => ctx.int(*value).expect("fits"),
            };
            let to = match to {
                Atom(name) => ctx.atom(name).expect("seeded"),
                Int(value) => ctx.int(*value).expect("fits"),
            };
            let time = ctx.int(time).expect("fits");
            let got = convert(&mut ctx, time, from, to)
                .unwrap_or_else(|err| panic!("convert_time_unit({time:?},{from:?},{to:?}) => {err:?}"));
            assert_eq!(
                as_int(got),
                expected,
                "convert_time_unit({time:?}, {from:?}, {to:?})"
            );
        }
    }    /// The same BIF for values or ratios past the 60-bit small range: the oracle
    /// returns the exact bignum (or the identity `Time`), never a truncated one.
    #[test]
    fn convert_time_unit_3_promotes_to_bignums_exactly_like_the_oracle() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let second = ctx.atom("second").expect("seeded");
        let nanosecond = ctx.atom("nanosecond").expect("seeded");
        let unit_200 = bignum_term(&mut ctx, false, 200);
        let negative_70 = bignum_term(&mut ctx, true, 70);
        let big_time = bignum_term(&mut ctx, false, 100);
        let huge = bignum_term(&mut ctx, false, 70); // 2^70, the 1e21-second reading below

        // (Time, From, To) => oracle decimal.
        let cases: &[(Eterm, Eterm, Eterm, &str)] = &[
            // `convert_time_unit(1 bsl 200, second, nanosecond)`.
            (
                unit_200,
                second,
                nanosecond,
                "1606938044258990275541962092341162602522202993782792835301376000000000",
            ),
            // `convert_time_unit(1 bsl 200, nanosecond, second)` (floor).
            (
                unit_200,
                nanosecond,
                second,
                "1606938044258990275541962092341162602522202993782792",
            ),
            // `convert_time_unit(-2^70, second, nanosecond)`.
            (
                negative_70,
                second,
                nanosecond,
                "-1180591620717411303424000000000",
            ),
            // `convert_time_unit(-2^70, nanosecond, second)` (floor).
            (negative_70, nanosecond, second, "-1180591620718"),            // `convert_time_unit(2^100, second, seconds)` — the identity arm, which
            // returns `Time` itself.
            (big_time, second, second, "1267650600228229401496703205376"),
            // A bignum ratio past `i128`'s resolution of the product: `1 * 2^200 / 2^200`.
            (ctx.int(1).expect("fits"), unit_200, second, "0"),
        ];

        for &(time, from, to, expected) in cases {
            let got = convert(&mut ctx, time, from, to)
                .unwrap_or_else(|err| panic!("convert_time_unit(...) => {err:?}"));
            assert_eq!(decimal(got), expected);
        }

        // `2^70 seconds` in nanoseconds: a bignum `Time` scaled by a bignum ratio, so
        // both operand shapes of the arbitrary-precision chain are exercised.
        let huge_ns = convert(&mut ctx, huge, second, second).expect("the identity arm");
        assert_eq!(huge_ns, huge, "the identity arm returns the very same term");
        let scaled = convert(&mut ctx, huge, second, nanosecond).expect("a bignum Time");
        assert_eq!(decimal(scaled), "1180591620717411303424000000000");
    }

    /// A `Time` that is not an integer can never convert: the oracle's `TU * Time`
    /// raises `badarith` inside the body's own `try`, whose catch folds it to `badarg`
    /// — including when `FU =:= TU` (that arm needs `is_integer(Time)`).
    #[test]
    fn convert_time_unit_3_rejects_a_non_integer_time_as_badarg() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let second = ctx.atom("second").expect("seeded");
        let millisecond = ctx.atom("millisecond").expect("seeded");
        let float = ctx.make_float(1.5).expect("fits");
        let atom = ctx.atom("bogus").expect("seeded");

        for (time, from, to) in [
            (float, second, second),
            (float, second, millisecond),
            (atom, second, millisecond),
        ] {
            assert!(
                matches!(convert(&mut ctx, time, from, to), Err(BeamError::BadArg)),
                "a non-integer Time must be badarg"
            );
        }
    }

    /// Every failing unit argument the oracle reports as `badarg`: an unknown atom, a
    /// zero or negative unit, and a unit that is neither an atom nor an integer.
    #[test]
    fn convert_time_unit_3_rejects_bad_units_as_badarg() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let second = ctx.atom("second").expect("seeded");
        let fortnight = ctx.atom("fortnight").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let zero = ctx.int(0).expect("fits");
        let negative = ctx.int(-5).expect("fits");

        // (Time, From, To) — the failing argument in either unit position.
        for (from, to) in [
            (fortnight, second),
            (second, fortnight),
            (zero, second),
            (second, zero),
            (negative, second),
            (one, negative),
        ] {
            assert!(
                matches!(convert(&mut ctx, one, from, to), Err(BeamError::BadArg)),
                "convert_time_unit(1, {from:?}, {to:?}) must be badarg"
            );
        }

        // A non-atom, non-integer unit: a list in either position.
        let list = ctx.list(&[one]).expect("fits");
        assert!(matches!(convert(&mut ctx, one, list, second), Err(BeamError::BadArg)));
        assert!(matches!(convert(&mut ctx, one, second, list), Err(BeamError::BadArg)));
    }

    #[test]
    fn register_installs_the_erlang_time_bifs() {        let mut reg = BifRegistry::new();
        register(&mut reg);

        for (name, arity) in [
            ("monotonic_time", 0),
            ("monotonic_time", 1),
            ("system_time", 0),
            ("system_time", 1),
            ("time_offset", 0),
            ("time_offset", 1),
            ("convert_time_unit", 3),
            ("unique_integer", 0),
            ("unique_integer", 1),
            ("timestamp", 0),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 10);
    }}
