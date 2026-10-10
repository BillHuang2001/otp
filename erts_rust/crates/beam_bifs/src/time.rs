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
//! Everything else is `badarg`: an unrecognized atom (`monotonic_time(fortnight)`),
//! a non-integer (`1.0`, a list), a zero or negative integer, and a **negative**
//! bignum (the C's `big_sign`). A *positive* bignum unit is a legal unit the C
//! evaluates in arbitrary precision, which this runtime's `i128`-bounded
//! arithmetic cannot reproduce, so it answers the typed capability gap
//! [`BeamError::Raise`] `unsupported_bif` — never a wrong value (the same
//! convention as `src/os_bif.rs::os_unit`).
//!
//! # Where M1's clocks come from
//!
//! * `monotonic_time/0` reads the **Rust process monotonic clock**
//!   ([`Instant`] captured lazily on first use through a global [`OnceLock`]). Its
//!   origin is arbitrary (the first call) and it never decreases — which is all
//!   `erlang:monotonic_time/0` promises. Real ERTS maps the OS `CLOCK_MONOTONIC`
//!   onto its own epoch; that absolute value is opaque to programs, so M1 does
//!   not reproduce it.
//! * `system_time/0` reads [`SystemTime`] — **nanoseconds since the UNIX epoch**
//!   (`1970-01-01T00:00:00Z`), the oracle's meaning.
//! * `time_offset/0` is the offset for which
//!   `system_time = monotonic_time + time_offset` (the invariant the oracle
//!   exhibits), i.e. `system_time(native) - monotonic_time(native)`. Its value is
//!   arbitrary in M1 because the monotonic origin is, but the *relationship* is
//!   reproduced faithfully.
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
//! practice, while a coarse **integer** unit conversely promotes
//! (`system_time(1 bsl 58)`), and `monotonic_time/1` is a process-relative, hence
//! small, value.
//!
//! [`BeamError::SystemLimit`] remains only for a clock before 1970 or a
//! nanosecond count past `i64` (year ~2262) — never a truncated value.
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

use crate::access::{is_atom, is_big, is_small};
use crate::arith;
use crate::context::BifContext;
use crate::error::{badarg, unsupported_bif};
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// A time unit accepted by the `/1` BIFs — the atom forms of `time_unit_conversion`'s
/// switch (`erl_time_sup.c:2277-2309`) plus the integer "units per second" form
/// `erlang:convert_time_unit/3` also accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// A caller-supplied **positive** integer: that many target units per second
    /// (`system_time(1)` => seconds, `system_time(1000)` => milliseconds).
    PerSecond(i64),
}

impl TimeUnit {
    /// The number of target units in one second.
    fn per_second(self) -> i128 {
        match self {
            TimeUnit::Native | TimeUnit::PerfCounter | TimeUnit::Nanosecond => 1_000_000_000,
            TimeUnit::Microsecond => 1_000_000,
            TimeUnit::Millisecond => 1_000,
            TimeUnit::Second => 1,
            TimeUnit::PerSecond(per_second) => i128::from(per_second),
        }
    }
}

/// The unit named by `term` — exactly the set `time_unit_conversion` accepts
/// (`erl_time_sup.c:2277-2341`): the atoms
/// `native`/`perf_counter`/`second`/`seconds`/`millisecond`/`milli_seconds`/
/// `microsecond`/`micro_seconds`/`nanosecond`/`nano_seconds`, or any **positive**
/// integer (units per second).
///
/// # Errors
///
/// [`BeamError::BadArg`] for anything else — an unknown atom, a non-integer, a
/// zero or negative integer, and a negative bignum. A **positive bignum** unit is
/// a legal unit whose C conversion runs in arbitrary precision; this runtime's
/// `i128`-bounded arithmetic cannot reproduce it, so it answers the typed
/// capability gap `unsupported_bif` rather than a wrong value.
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
        // The C accepts a non-negative bignum (it traps to Erlang code, which
        // computes `ToUnit * Time div FromUnit`); a negative one is `badarg`
        // (`big_sign`).
        let (negative, _) = beam_codec::term::big_parts(term).map_err(|_| badarg())?;
        return if negative {
            Err(badarg())
        } else {
            Err(unsupported_bif())
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
/// 1_000_000_000`, flooring toward negative infinity, the
/// `erlang:convert_time_unit/3` rounding (`erts_napi_convert_time_unit`,
/// `erl_time_sup.c:2462-2495`).
///
/// The multiplication runs in `i128`: the widest accepted unit is the largest
/// small integer (`2^59 - 1`/s), so the product of a `i64` nanosecond count and
/// `per_second` stays far inside `i128`.
fn convert_native(native: i64, unit: TimeUnit) -> i128 {
    (i128::from(native) * unit.per_second()).div_euclid(1_000_000_000)
}

/// The exact integer term for a conversion result: a small integer when it fits,
/// else the heap bignum the oracle returns (a coarse integer unit such as
/// `system_time(1 bsl 58)` promotes).
fn int_from_i128(ctx: &mut BifContext<'_>, value: i128) -> Result<Eterm> {
    if let Ok(small) = i64::try_from(value) {
        return arith::int_from_i64(ctx, small);
    }
    // An `i128` magnitude is at most two 64-bit limbs, least significant first —
    // the order `arith::int_from_mag` expects.
    let magnitude = value.unsigned_abs();
    let limbs = [magnitude as u64, (magnitude >> 64) as u64];
    arith::int_from_mag(ctx, value < 0, &limbs)
}

/// The process-global monotonic clock; its origin is the first call.
fn monotonic_start() -> &'static Instant {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now)
}

/// The monotonic clock in native units (nanoseconds since process start).
///
/// Non-negative and non-decreasing. A nanosecond count only overflows `i64`
/// after ~292 years of uptime, so the saturation below is unreachable in practice
/// and keeps the value monotonic regardless.
fn monotonic_native() -> i64 {
    let elapsed = monotonic_start().elapsed();
    i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
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
    int_from_i128(ctx, convert_native(monotonic_native(), unit))
}

/// `erlang:system_time/0`.
fn system_time_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    arith::int_from_i64(ctx, system_native()?)
}

/// `erlang:system_time/1`.
fn system_time_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unit = time_unit(ctx, args[0])?;
    int_from_i128(ctx, convert_native(system_native()?, unit))
}

/// `erlang:time_offset/0` — `system_time - monotonic_time` in native units.
fn time_offset_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let offset = system_native()? - monotonic_native();
    arith::int_from_i64(ctx, offset)
}

/// `erlang:time_offset/1` — the `system_time - monotonic_time` offset, in `unit`.
fn time_offset_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unit = time_unit(ctx, args[0])?;
    let offset = system_native()? - monotonic_native();
    int_from_i128(ctx, convert_native(offset, unit))
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

/// The process-global unique-integer counter: `fetch_add(1)`, so a call yields a
/// value no other call yields.
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
    reg.register("erlang", "unique_integer", 0, unique_integer_0);
    reg.register("erlang", "unique_integer", 1, unique_integer_1);
    reg.register("erlang", "timestamp", 0, timestamp_0);
}

#[cfg(test)]
mod tests {
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

    /// A heap big enough for the tuples/lists these tests build.
    fn heap() -> Heap {
        Heap::new(256).expect("test heap")
    }

    #[test]
    fn monotonic_time_is_non_negative_and_non_decreasing() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let a = as_int(monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0"));
        let b = as_int(monotonic_time_0(&mut ctx, &[]).expect("monotonic_time/0"));
        assert!(a >= 0, "monotonic_time/0 must not be negative: {a}");
        assert!(b >= a, "monotonic_time/0 decreased: {a} then {b}");
    }

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

        // Every reading lies in the [lo, hi] bracket, and the conversion floors.
        assert!(
            lo / 1_000_000 <= ms && ms <= hi / 1_000_000,
            "monotonic_time(millisecond) = {ms} not within [{}, {}]",
            lo / 1_000_000,
            hi / 1_000_000
        );
    }

    /// Every accepted unit spelling names its exact ratio, and the conversion is
    /// `native * per_second div 1_000_000_000` flooring toward negative infinity
    /// (the `erlang:convert_time_unit/3` rounding, pinned against the oracle:
    /// `convert_time_unit(-1500000000, nanosecond, second)` => `-2`).
    #[test]
    fn every_accepted_unit_names_its_per_second_ratio() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let ctx = context(&mut heap, &atoms);

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
            assert_eq!(unit.per_second(), per_second, "wrong ratio for {unit:?}");

            // The exact conversion of a positive, a zero and a negative native
            // count — the last one floors toward negative infinity.
            assert_eq!(
                convert_native(1_500_000_000, unit),
                1_500_000_000 * per_second / 1_000_000_000
            );
            assert_eq!(convert_native(0, unit), 0);
            assert_eq!(
                convert_native(-1_500_000_000, unit),
                (-1_500_000_000 * per_second).div_euclid(1_000_000_000)
            );
        }

        // The oracle-pinned floor cases.
        assert_eq!(convert_native(-1_500_000_000, TimeUnit::Second), -2);
        assert_eq!(
            convert_native(-1_500_000_000, TimeUnit::Millisecond),
            -1_500
        );
        assert_eq!(
            convert_native(-1_500_000_000, TimeUnit::Microsecond),
            -1_500_000
        );
        assert_eq!(
            convert_native(-1_500_000_000, TimeUnit::Nanosecond),
            -1_500_000_000
        );
        assert_eq!(convert_native(-1, TimeUnit::Nanosecond), -1);
        assert_eq!(convert_native(1, TimeUnit::Nanosecond), 1);
        assert_eq!(convert_native(1, TimeUnit::Second), 0);
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
            assert!(mono >= 0.0, "monotonic_time/1 must not be negative: {mono}");

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
            // Bracket the reading between two nanosecond readings.
            let lo = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));
            let value = as_int(monotonic_time_1(&mut ctx, &[unit]).expect("unit"));
            let hi = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));
            assert!(
                lo / ns_per_unit <= value && value <= hi / ns_per_unit,
                "a reading of {value} is outside [{}, {}]",
                lo / ns_per_unit,
                hi / ns_per_unit
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

    /// A **bignum** unit: a negative one is `badarg` (the C's `big_sign`), and a
    /// positive one — a legal unit the C converts in arbitrary precision — is the
    /// typed capability gap `unsupported_bif`, never a wrong value.
    #[test]
    fn bignum_units_are_badarg_or_a_typed_gap() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        // 2^64 + 1: far past the 60-bit small-integer range.
        let limbs = [1u64, 1u64];
        let positive = arith::int_from_mag(&mut ctx, false, &limbs).expect("a bignum");
        let negative = arith::int_from_mag(&mut ctx, true, &limbs).expect("a bignum");

        assert!(matches!(
            time_unit(&ctx, positive),
            Err(BeamError::Raise(_))
        ));
        assert!(matches!(time_unit(&ctx, negative), Err(BeamError::BadArg)));
        assert!(matches!(
            system_time_1(&mut ctx, &[positive]),
            Err(BeamError::Raise(_))
        ));
        assert!(matches!(
            monotonic_time_1(&mut ctx, &[negative]),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            time_offset_1(&mut ctx, &[negative]),
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
        // scale and likewise a bignum.
        let offset = time_offset_0(&mut ctx, &[]).expect("time_offset/0");
        assert!(
            !crate::access::is_small(offset),
            "time_offset/0 must be a bignum too"
        );
        let offset_value = crate::arith::integer_to_f64(offset).expect("round-trips to f64");
        assert!(
            (offset_value - value).abs() < 1.0e18,
            "time_offset/0 = {offset_value} is not within a process-lifetime of {value}"
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

    #[test]
    fn register_installs_the_erlang_time_bifs() {
        let mut reg = BifRegistry::new();
        register(&mut reg);

        for (name, arity) in [
            ("monotonic_time", 0),
            ("monotonic_time", 1),
            ("system_time", 0),
            ("system_time", 1),
            ("time_offset", 0),
            ("time_offset", 1),
            ("unique_integer", 0),
            ("unique_integer", 1),
            ("timestamp", 0),
        ] {
            assert!(
                reg.contains("erlang", name, arity),
                "missing erlang:{name}/{arity}"
            );
        }
        assert_eq!(reg.len(), 9);
    }
}
