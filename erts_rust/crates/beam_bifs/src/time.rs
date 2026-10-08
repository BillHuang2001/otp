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
//! `erts_internal:time_unit/0`). A `/1` unit argument — `second`, `millisecond`,
//! `microsecond`, `nanosecond` or `native` — divides the native value by that
//! unit's ratio, **flooring toward negative infinity**, exactly as
//! `erlang:convert_time_unit/3` does (`erl_time_sup.c:2460`, verified against the
//! oracle); any other unit is `badarg` (`monotonic_time(fortnight)` => `badarg`).
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
//! [`crate::arith::int_from_i64`], which folds a value that fits back into a small
//! integer and promotes otherwise. The coarser units
//! (`second`/`millisecond`/`microsecond`) stay small in practice, and
//! `monotonic_time/1` is a process-relative, hence small, value.
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
use beam_term::{atom_val, Eterm};

use crate::access::is_atom;
use crate::arith;
use crate::context::BifContext;
use crate::error::badarg;
use crate::print::proper_list;
use crate::registry::BifRegistry;

/// A time unit accepted by the `/1` BIFs (matching `erl_time_sup.c`'s
/// `am_second`/`am_millisecond`/`am_microsecond`/`am_nanosecond`/`am_native`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimeUnit {
    /// `second`.
    Second,
    /// `millisecond`.
    Millisecond,
    /// `microsecond`.
    Microsecond,
    /// `nanosecond`.
    Nanosecond,
    /// `native` — the runtime's native unit (nanoseconds in M1).
    Native,
}

impl TimeUnit {
    /// The number of native units (nanoseconds) in one `self`.
    fn native_per_unit(self) -> i64 {
        match self {
            TimeUnit::Second => 1_000_000_000,
            TimeUnit::Millisecond => 1_000_000,
            TimeUnit::Microsecond => 1_000,
            TimeUnit::Nanosecond | TimeUnit::Native => 1,
        }
    }
}

/// The unit named by the atom `term`, or `badarg` for anything else (including a
/// non-atom and an unknown atom).
fn time_unit(ctx: &BifContext<'_>, term: Eterm) -> Result<TimeUnit> {
    let name = if is_atom(term) {
        ctx.atoms().name(atom_val(term))
    } else {
        None
    };
    match name {
        Some("second") => Ok(TimeUnit::Second),
        Some("millisecond") => Ok(TimeUnit::Millisecond),
        Some("microsecond") => Ok(TimeUnit::Microsecond),
        Some("nanosecond") => Ok(TimeUnit::Nanosecond),
        Some("native") => Ok(TimeUnit::Native),
        _ => Err(badarg()),
    }
}

/// Converts a native (nanosecond) value into `unit`, flooring toward negative
/// infinity — the `erlang:convert_time_unit/3` rounding (`erl_time_sup.c:2492`).
fn convert_native(native: i64, unit: TimeUnit) -> i64 {
    native.div_euclid(unit.native_per_unit())
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
    arith::int_from_i64(ctx, convert_native(monotonic_native(), unit))
}

/// `erlang:system_time/0`.
fn system_time_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    arith::int_from_i64(ctx, system_native()?)
}

/// `erlang:system_time/1`.
fn system_time_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let unit = time_unit(ctx, args[0])?;
    arith::int_from_i64(ctx, convert_native(system_native()?, unit))
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
    arith::int_from_i64(ctx, convert_native(offset, unit))
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
            "true", "false", "ok", "second", "millisecond", "microsecond", "nanosecond", "native",
            "positive", "monotonic", "fortnight", "bogus",
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

    #[test]
    fn every_time_unit_is_converted_consistently() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let second = ctx.atom("second").expect("seeded");
        let millisecond = ctx.atom("millisecond").expect("seeded");
        let microsecond = ctx.atom("microsecond").expect("seeded");
        let nanosecond = ctx.atom("nanosecond").expect("seeded");
        let native = ctx.atom("native").expect("seeded");

        // Bracket every conversion between two nanosecond readings.
        let lo = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));
        let sec = as_int(monotonic_time_1(&mut ctx, &[second]).expect("s"));
        let ms = as_int(monotonic_time_1(&mut ctx, &[millisecond]).expect("ms"));
        let us = as_int(monotonic_time_1(&mut ctx, &[microsecond]).expect("us"));
        let ns = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));
        let nat = as_int(monotonic_time_1(&mut ctx, &[native]).expect("native"));
        let hi = as_int(monotonic_time_1(&mut ctx, &[nanosecond]).expect("ns"));

        assert!(lo / 1_000_000_000 <= sec && sec <= hi / 1_000_000_000);
        assert!(lo / 1_000_000 <= ms && ms <= hi / 1_000_000);
        assert!(lo / 1_000 <= us && us <= hi / 1_000);
        // `nanosecond` and `native` share the identity ratio, so both equal the
        // nanosecond reading.
        assert!(lo <= ns && ns <= hi);
        assert!(lo <= nat && nat <= hi);
    }

    #[test]
    fn a_bad_unit_is_badarg() {
        let mut heap = heap();
        let atoms = seeded_atoms();
        let mut ctx = context(&mut heap, &atoms);

        let fortnight = ctx.atom("fortnight").expect("seeded");
        let not_an_atom = ctx.int(1).expect("fits");

        for arg in [fortnight, not_an_atom] {
            assert!(matches!(
                monotonic_time_1(&mut ctx, &[arg]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                system_time_1(&mut ctx, &[arg]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                time_offset_1(&mut ctx, &[arg]),
                Err(BeamError::BadArg)
            ));
        }
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
