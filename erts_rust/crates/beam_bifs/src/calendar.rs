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

//! Calendar and clock BIFs for `erlang`: `date/0`, `time/0`, `now/0`,
//! `universaltime/0`, `localtime/0`, `posixtime_to_universaltime/1`,
//! `universaltime_to_posixtime/1`, `universaltime_to_localtime/1` and
//! `localtime_to_universaltime/2`.
//!
//! Ground truth (read-only): the entries in
//! `../../erts/emulator/beam/bif.tab` and the implementations in
//! `../../erts/emulator/beam/bif.c` (`date_0`, `universaltime_0`, `localtime_0`,
//! `time_to_parts`, `localtime_to_universaltime_2`, `universaltime_to_localtime_1`,
//! `universaltime_to_posixtime_1`, `posixtime_to_universaltime_1`, `now_0`) and
//! `../../erts/emulator/beam/erl_time_sup.c` (`gregday`, `seconds_to_univ`,
//! `univ_to_seconds`, `univ_to_local`, `local_to_univ`, `get_*`).
//!
//! # Roster
//!
//! `erlang:date/0`, `erlang:time/0`, `erlang:now/0`, `erlang:universaltime/0`,
//! `erlang:localtime/0`, `erlang:posixtime_to_universaltime/1`,
//! `erlang:universaltime_to_posixtime/1`, `erlang:universaltime_to_localtime/1`
//! and `erlang:localtime_to_universaltime/2`.
//!
//! # Split of work
//!
//! The pure calendar arithmetic — `gregday`, `seconds_to_univ`, `univ_to_seconds`,
//! the `mdays`/leap-year rules and every range check — is safe Rust here, exactly
//! transcribing the C. Only the timezone/DST-dependent operations
//! (`localtime_r`/`gmtime_r`/`mktime`) live in the [`localtime`] submodule, which
//! is this crate's `#[allow(unsafe_code)]` bridge to the C time library.
//!
//! # The arguments and the `badarg` cases
//!
//! A datetime is the tuple `{{Year, Month, Day}, {Hour, Minute, Second}}` whose
//! six components must all be small integers (`time_to_parts`, `bif.c:3906`); any
//! other shape or a non-integer component is `badarg`. `universaltime_to_posixtime/1`
//! additionally requires the C's calendar range `1600`-based year and
//! `1..=31`/`0..=23`/`0..=59` fields (`univ_to_seconds`, `erl_time_sup.c:1704`), and
//! the two local conversions require the C `BASEYEAR` (`1902`) instead
//! (`univ_to_local`/`local_to_univ`). `posixtime_to_universaltime/1` accepts **any**
//! `Sint64` (a negative count is a pre-1970 date) and rejects a float or an
//! out-of-`i64` bignum with `badarg` (`term_to_Sint64`, `big.c:3702`).
//! `localtime_to_universaltime/2`'s second argument is the atom `true`, `false` or
//! `undefined` (the DST hint); anything else is `badarg` (`bif.c:3952`).

mod localtime;

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use beam_common::{BeamError, Result};
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::arith;
use crate::context::BifContext;
use crate::error::badarg;
use crate::registry::BifRegistry;

use localtime::Tm;

/// A datetime every one of whose C range checks applies from this year on: erts'
/// `BASEYEAR` (`erl_time_sup.c:1600`) — the earliest year erts is sure every
/// platform handles through `mktime`/`localtime_r`.
const BASE_YEAR: i64 = 1902;

/// The earliest year `univ_to_seconds` accepts (`erl_time_sup.c:1707`).
const UNIVERSAL_MIN_YEAR: i64 = 1600;

/// The C `INT_MAX - 1` upper year bound (`erl_time_sup.c:1707`).
const MAX_YEAR: i64 = i32::MAX as i64 - 1;

const SECONDS_PER_MINUTE: i64 = 60;
const SECONDS_PER_HOUR: i64 = 60 * SECONDS_PER_MINUTE;
const SECONDS_PER_DAY: i64 = 24 * SECONDS_PER_HOUR;

/// Days in each month (`erl_time_sup.c:1590`'s `mdays[]`); index 0 is unused.
const MDAYS: [i64; 13] = [0, 31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];

/// `is_leap_year(y)` (`erl_time_sup.c:1594`).
fn is_leap_year(year: i64) -> bool {
    ((year % 4 == 0) && (year % 100 != 0)) || (year % 400 == 0)
}

/// The C `gregday` (`erl_time_sup.c:1644`): days since `1600-01-01` minus the
/// `135140` days from there to the Unix epoch, i.e. seconds' worth of days since
/// `1970-01-01`. `month` must be in `1..=12`.
fn gregday(year: i64, month: i64, day: i64) -> i64 {
    let mut ndays: i64 = 0;
    let gyear = year - 1600;
    if gyear > 0 {
        let pyear = gyear - 1;
        ndays = (pyear / 4) - (pyear / 100) + (pyear / 400) + pyear * 365 + 366;
    }
    for m in 1..month {
        ndays += MDAYS[m as usize];
    }
    if is_leap_year(year) && month > 2 {
        ndays += 1;
    }
    ndays + day - 1 - 135140
}

/// The C `mdays[month]` plus the leap-day rule, for a validated `month`.
fn days_in_month(year: i64, month: i64) -> i64 {
    if month == 2 && is_leap_year(year) {
        29
    } else {
        MDAYS[month as usize]
    }
}

/// The C range check shared by `univ_to_seconds`/`univ_to_local`/`local_to_univ`
/// (`erl_time_sup.c:1707`,`:1808`,`:1744`), with the caller's minimum year.
fn datetime_in_range(t: &Tm, min_year: i64) -> bool {
    if !(1..=12).contains(&t.month) {
        return false;
    }
    (min_year..=MAX_YEAR).contains(&t.year)
        && (1..=days_in_month(t.year, t.month)).contains(&t.day)
        && (0..=23).contains(&t.hour)
        && (0..=59).contains(&t.minute)
        && (0..=59).contains(&t.second)
}

/// The C `time_to_parts` (`bif.c:3906`): `{{Y,M,D},{H,Mi,S}}` with six small
/// integers, or `None` for any other shape/type.
fn time_to_parts(term: Eterm) -> Option<Tm> {
    if !access::is_tuple(term) || access::tuple_size(term) != 2 {
        return None;
    }
    let [year, month, day] = tuple_three_smalls(access::tuple_element(term, 0))?;
    let [hour, minute, second] = tuple_three_smalls(access::tuple_element(term, 1))?;
    Some(Tm {
        year,
        month,
        day,
        hour,
        minute,
        second,
    })
}

/// A 3-tuple of small integers as an `[i64; 3]`, or `None` (`time_to_parts`'
/// per-component `is_not_small` test).
fn tuple_three_smalls(term: Eterm) -> Option<[i64; 3]> {
    if !access::is_tuple(term) || access::tuple_size(term) != 3 {
        return None;
    }
    let mut out = [0i64; 3];
    for (index, slot) in out.iter_mut().enumerate() {
        let element = access::tuple_element(term, index);
        if !access::is_small(element) {
            return None;
        }
        *slot = beam_term::signed_value(element);
    }
    Some(out)
}

/// The datetime tuple `{{Y,M,D},{H,Mi,S}}` built on `ctx`'s heap.
fn datetime_tuple(ctx: &mut BifContext<'_>, t: Tm) -> Result<Eterm> {
    let year = ctx.int(t.year)?;
    let month = ctx.int(t.month)?;
    let day = ctx.int(t.day)?;
    let date = ctx.tuple(&[year, month, day])?;

    let hour = ctx.int(t.hour)?;
    let minute = ctx.int(t.minute)?;
    let second = ctx.int(t.second)?;
    let time = ctx.tuple(&[hour, minute, second])?;

    ctx.tuple(&[date, time])
}

/// The C `seconds_to_univ` (`erl_time_sup.c:1669`): the days-from-civil inverse
/// (Howard Hinnant's algorithm), transcribed with the C's truncating integer
/// division. Total for every `i64`; `i64::MIN` yields year `-292277022657`.
fn seconds_to_univ(time: i64) -> Tm {
    let mut days = time / SECONDS_PER_DAY;
    let mut secs = time % SECONDS_PER_DAY;
    if secs < 0 {
        days -= 1;
        secs += SECONDS_PER_DAY;
    }

    let tmp = secs % SECONDS_PER_HOUR;
    let hour = secs / SECONDS_PER_HOUR;
    let minute = tmp / SECONDS_PER_MINUTE;
    let second = tmp % SECONDS_PER_MINUTE;

    days += 719468;
    let mut y = (10000 * days + 14780) / 3652425;
    let mut rem = days - (365 * y + y / 4 - y / 100 + y / 400);
    if rem < 0 {
        y -= 1;
        rem = days - (365 * y + y / 4 - y / 100 + y / 400);
    }
    let mi = (100 * rem + 52) / 3060;
    let month = (mi + 2) % 12 + 1;
    let year = y + (mi + 2) / 12;
    let day = rem - (mi * 306 + 5) / 10 + 1;

    Tm {
        year,
        month,
        day,
        hour,
        minute,
        second,
    }
}

/// The C `univ_to_seconds` (`erl_time_sup.c:1704`): seconds since the epoch, or
/// `None` when a field is out of range.
fn univ_to_seconds(t: &Tm) -> Option<i64> {
    if !datetime_in_range(t, UNIVERSAL_MIN_YEAR) {
        return None;
    }
    let days = gregday(t.year, t.month, t.day);
    Some(
        SECONDS_PER_DAY * days
            + SECONDS_PER_HOUR * t.hour
            + SECONDS_PER_MINUTE * t.minute
            + t.second,
    )
}

/// The C `the_clock` expression inside `univ_to_local` (`erl_time_sup.c:1820`).
fn univ_clock(t: &Tm) -> i64 {
    t.second + SECONDS_PER_MINUTE * (t.minute + SECONDS_PER_MINUTE * (t.hour + 24 * gregday(t.year, t.month, t.day)))
}

/// The C `term_to_Sint64` (`big.c:3702`): the `i64` value of a small integer or a
/// bignum that fits, else `badarg`.
fn term_to_i64(term: Eterm) -> Result<i64> {
    if access::is_small(term) {
        return Ok(beam_term::signed_value(term));
    }
    if !access::is_big(term) {
        return Err(badarg());
    }
    let (negative, digits) = beam_codec::term::big_parts(term)?;
    // A normalized bignum with more than one 64-bit limb always exceeds `i64`.
    let magnitude = match digits {
        [] => 0u64,
        [single] => *single,
        _ => return Err(badarg()),
    };
    if negative {
        match magnitude.cmp(&((i64::MAX as u64) + 1)) {
            std::cmp::Ordering::Greater => Err(badarg()),
            std::cmp::Ordering::Equal => Ok(i64::MIN),
            std::cmp::Ordering::Less => Ok(-(magnitude as i64)),
        }
    } else if magnitude > i64::MAX as u64 {
        Err(badarg())
    } else {
        Ok(magnitude as i64)
    }
}

/// `erlang:universaltime/0` — `{{Y,M,D},{H,Mi,S}}` in UTC.
fn universaltime_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    datetime_tuple(ctx, localtime::now_utc()?)
}

/// `erlang:localtime/0` — `{{Y,M,D},{H,Mi,S}}` in the local timezone.
fn localtime_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    datetime_tuple(ctx, localtime::now_local()?)
}

/// `erlang:date/0` — `{Y,M,D}` in the local timezone.
fn date_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let t = localtime::now_local()?;
    let year = ctx.int(t.year)?;
    let month = ctx.int(t.month)?;
    let day = ctx.int(t.day)?;
    ctx.tuple(&[year, month, day])
}

/// `erlang:time/0` — `{H,Mi,S}` in the local timezone.
fn time_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let t = localtime::now_local()?;
    let hour = ctx.int(t.hour)?;
    let minute = ctx.int(t.minute)?;
    let second = ctx.int(t.second)?;
    ctx.tuple(&[hour, minute, second])
}

/// `erlang:posixtime_to_universaltime/1` — a POSIX (or negative pre-epoch) second
/// count as `{{Y,M,D},{H,Mi,S}}`.
fn posixtime_to_universaltime_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let time = term_to_i64(args[0])?;
    datetime_tuple(ctx, seconds_to_univ(time))
}

/// `erlang:universaltime_to_posixtime/1` — the second count of a UTC datetime.
fn universaltime_to_posixtime_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let t = time_to_parts(args[0]).ok_or_else(badarg)?;
    let seconds = univ_to_seconds(&t).ok_or_else(badarg)?;
    arith::int_from_i64(ctx, seconds)
}

/// `erlang:universaltime_to_localtime/1` — a UTC datetime in the local timezone,
/// or `badarg` outside the C's `BASEYEAR` calendar range.
fn universaltime_to_localtime_1(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let t = time_to_parts(args[0]).ok_or_else(badarg)?;
    if !datetime_in_range(&t, BASE_YEAR) {
        return Err(badarg());
    }
    let converted = localtime::localtime_of(univ_clock(&t))?;
    datetime_tuple(ctx, converted)
}

/// `erlang:localtime_to_universaltime/2` — a local datetime in UTC, with the
/// second argument the atom `true`/`false`/`undefined` (the DST hint).
fn localtime_to_universaltime_2(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let isdst = dst_hint(ctx, args[1])?;
    let t = time_to_parts(args[0]).ok_or_else(badarg)?;
    if !datetime_in_range(&t, BASE_YEAR) {
        return Err(badarg());
    }
    let converted = localtime::local_to_utc(t, isdst)?;
    datetime_tuple(ctx, converted)
}

/// The C `dst`/`am_true`/`am_false`/`am_undefined` dispatch (`bif.c:3952`) as the
/// `mktime` `tm_isdst` value: `1` (`true`), `0` (`false`), `-1` (`undefined`).
fn dst_hint(ctx: &BifContext<'_>, term: Eterm) -> Result<libc::c_int> {
    let name = if access::is_atom(term) {
        ctx.atoms().name(atom_val(term))
    } else {
        None
    };
    match name {
        Some("true") => Ok(1),
        Some("false") => Ok(0),
        Some("undefined") => Ok(-1),
        _ => Err(badarg()),
    }
}

/// The process-global microsecond reading of the last `now/0` — the C
/// `now_prev` counter.
static NOW_PREVIOUS: AtomicI64 = AtomicI64::new(i64::MIN);

/// `system_time` in microseconds since the epoch (the C `now/0` clock).
///
/// # Errors
///
/// [`BeamError::SystemLimit`] for a clock before 1970 or a count past `i64`.
fn system_micros() -> Result<i64> {
    let since_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BeamError::SystemLimit)?;
    i64::try_from(since_epoch.as_micros()).map_err(|_| BeamError::SystemLimit)
}

/// The C `now/0` strictly-monotonic bump (`erl_time_sup.c:1863`): the returned
/// microsecond value is always **greater** than the previous call's.
fn bump_micros(micros: i64) -> i64 {
    let mut prev = NOW_PREVIOUS.load(Ordering::Relaxed);
    loop {
        let now = if micros <= prev {
            prev.saturating_add(1)
        } else {
            micros
        };
        match NOW_PREVIOUS.compare_exchange_weak(prev, now, Ordering::SeqCst, Ordering::Relaxed) {
            Ok(_) => return now,
            Err(actual) => prev = actual,
        }
    }
}

/// `erlang:now/0` — the deprecated `{MegaSecs, Secs, MicroSecs}` (microseconds
/// since the epoch, split as `timestamp/0` does, with the strict-monotonic bump).
fn now_0(ctx: &mut BifContext<'_>, _args: &[Eterm]) -> Result<Eterm> {
    let micros = bump_micros(system_micros()?);
    let mega_secs = micros.div_euclid(1_000_000_000_000);
    let total_secs = micros.div_euclid(1_000_000);
    let secs = total_secs - mega_secs * 1_000_000;
    let micro_secs = micros - total_secs * 1_000_000;

    let mega = ctx.int(mega_secs)?;
    let sec = ctx.int(secs)?;
    let micro = ctx.int(micro_secs)?;
    ctx.tuple(&[mega, sec, micro])
}

/// Registers the `erlang` calendar/clock BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("erlang", "date", 0, date_0);
    reg.register("erlang", "time", 0, time_0);
    reg.register("erlang", "now", 0, now_0);
    reg.register("erlang", "localtime", 0, localtime_0);
    reg.register("erlang", "universaltime", 0, universaltime_0);
    reg.register(
        "erlang",
        "localtime_to_universaltime",
        2,
        localtime_to_universaltime_2,
    );
    reg.register(
        "erlang",
        "universaltime_to_localtime",
        1,
        universaltime_to_localtime_1,
    );
    reg.register(
        "erlang",
        "universaltime_to_posixtime",
        1,
        universaltime_to_posixtime_1,
    );
    reg.register(
        "erlang",
        "posixtime_to_universaltime",
        1,
        posixtime_to_universaltime_1,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seconds_to_univ_matches_the_oracle_goldens() {
        // Goldens from `erl` (OTP 29.1.1).
        for (secs, expected) in [
            (0i64, Tm { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 }),
            (1, Tm { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 1 }),
            (-1, Tm { year: 1969, month: 12, day: 31, hour: 23, minute: 59, second: 59 }),
            (1000000000, Tm { year: 2001, month: 9, day: 9, hour: 1, minute: 46, second: 40 }),
            (951782400, Tm { year: 2000, month: 2, day: 29, hour: 0, minute: 0, second: 0 }),
            (4102444800, Tm { year: 2100, month: 1, day: 1, hour: 0, minute: 0, second: 0 }),
            (-86400, Tm { year: 1969, month: 12, day: 31, hour: 0, minute: 0, second: 0 }),
            (-62135596800, Tm { year: 1, month: 1, day: 1, hour: 0, minute: 0, second: 0 }),
            (253402300800, Tm { year: 10000, month: 1, day: 1, hour: 0, minute: 0, second: 0 }),
        ] {
            assert_eq!(seconds_to_univ(secs), expected, "posixtime {secs}");
        }
    }

    #[test]
    fn seconds_to_univ_accepts_the_i64_extremes() {
        assert_eq!(
            seconds_to_univ(i64::MAX),
            Tm { year: 292277026596, month: 12, day: 4, hour: 15, minute: 30, second: 7 }
        );
        assert_eq!(
            seconds_to_univ(i64::MIN),
            Tm { year: -292277022657, month: 1, day: 26, hour: 8, minute: 29, second: 52 }
        );
    }

    #[test]
    fn univ_to_seconds_matches_the_oracle_goldens() {
        for (t, expected) in [
            (Tm { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 }, 0i64),
            (Tm { year: 2000, month: 2, day: 29, hour: 0, minute: 0, second: 0 }, 951782400),
            (Tm { year: 2016, month: 2, day: 29, hour: 12, minute: 34, second: 56 }, 1456749296),
            (Tm { year: 1901, month: 12, day: 31, hour: 23, minute: 59, second: 59 }, -2145916801),
            (Tm { year: 1600, month: 1, day: 1, hour: 0, minute: 0, second: 0 }, -11676096000),
            (Tm { year: 2020, month: 1, day: 1, hour: 0, minute: 0, second: 0 }, 1577836800),
            (Tm { year: 2147483646, month: 1, day: 1, hour: 0, minute: 0, second: 0 }, 67767976170460800),
        ] {
            assert_eq!(univ_to_seconds(&t), Some(expected), "datetime {t:?}");
        }
    }

    #[test]
    fn univ_to_seconds_rejects_out_of_range_fields() {
        let valid = Tm { year: 2020, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
        assert!(univ_to_seconds(&valid).is_some());

        for t in [
            Tm { year: 1599, month: 12, day: 31, ..valid },
            Tm { year: 2147483647, month: 1, day: 1, ..valid },
            Tm { month: 13, ..valid },
            Tm { day: 0, ..valid },
            Tm { day: 32, ..valid },
            Tm { hour: 24, ..valid },
            Tm { minute: 60, ..valid },
            Tm { second: 60, ..valid },
        ] {
            assert_eq!(univ_to_seconds(&t), None, "should be rejected: {t:?}");
        }

        // The leap-day rule: 2000-02-29 is valid, 1900-02-29 and 2000-02-30 are not.
        assert!(univ_to_seconds(&Tm { year: 2000, month: 2, day: 29, ..valid }).is_some());
        assert_eq!(univ_to_seconds(&Tm { year: 1900, month: 2, day: 29, ..valid }), None);
        assert_eq!(univ_to_seconds(&Tm { year: 2000, month: 2, day: 30, ..valid }), None);
    }

    #[test]
    fn datetime_in_range_uses_the_local_base_year() {
        let t = |year| Tm { year, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
        assert!(datetime_in_range(&t(1902), BASE_YEAR));
        assert!(!datetime_in_range(&t(1901), BASE_YEAR));
        // The universal bound is lower.
        assert!(datetime_in_range(&t(1901), UNIVERSAL_MIN_YEAR));
        assert!(!datetime_in_range(&t(1599), UNIVERSAL_MIN_YEAR));
    }

    #[test]
    fn gregday_spans_the_epoch() {
        assert_eq!(gregday(1970, 1, 1), 0);
        assert_eq!(gregday(1970, 1, 2), 1);
        assert_eq!(gregday(1969, 12, 31), -1);
        assert_eq!(gregday(2000, 1, 1), 10957);
        assert_eq!(gregday(1600, 1, 1), -135140);
    }

    #[test]
    fn seconds_to_univ_and_univ_to_seconds_round_trip_over_a_sweep() {
        // Every day for ~30 years, plus a spread of extreme values.
        let mut secs = -1000i64;
        while secs < 946_684_800 {
            let t = seconds_to_univ(secs);
            assert_eq!(univ_to_seconds(&t), Some(secs), "round trip at {secs}");
            secs += 86_399;
        }
    }

    #[test]
    fn term_to_i64_reads_small_ints_and_fitting_bignums() {
        use beam_alloc::Heap;
        use beam_term::make_small;

        let mut heap = Heap::new(64).expect("test heap");
        assert_eq!(term_to_i64(make_small(0)).expect("small"), 0);
        assert_eq!(term_to_i64(make_small(-7)).expect("small"), -7);
        // `2^70` is a two-limb bignum and does not fit `i64`.
        let big = beam_codec::term::make_big(&mut heap, false, &[0, 64]).expect("bignum");
        assert!(matches!(term_to_i64(big), Err(BeamError::BadArg)));
        // A one-limb bignum at `i64::MAX` fits.
        let max = beam_codec::term::make_big(&mut heap, false, &[i64::MAX as u64]).expect("bignum");
        assert_eq!(term_to_i64(max).expect("fits"), i64::MAX);
        // A float is never an integer.
        let float = crate::access::make_float(&mut heap, 1.0).expect("float");
        assert!(matches!(term_to_i64(float), Err(BeamError::BadArg)));
    }

    #[test]
    fn now_bumps_strictly() {
        let a = bump_micros(1_000);
        let b = bump_micros(1_000);
        let c = bump_micros(1_000_000);
        assert!(b > a, "now/0 must increase: {a} then {b}");
        assert!(c > b, "now/0 must increase: {b} then {c}");
        // An explicit larger reading is taken as-is.
        assert_eq!(bump_micros(i64::MAX), i64::MAX);
    }

    #[test]
    fn register_installs_the_erlang_calendar_bifs() {
        let mut reg = BifRegistry::new();
        register(&mut reg);

        for (name, arity) in [
            ("date", 0),
            ("time", 0),
            ("now", 0),
            ("localtime", 0),
            ("universaltime", 0),
            ("localtime_to_universaltime", 2),
            ("universaltime_to_localtime", 1),
            ("universaltime_to_posixtime", 1),
            ("posixtime_to_universaltime", 1),
        ] {
            assert!(reg.contains("erlang", name, arity), "missing erlang:{name}/{arity}");
        }
        assert_eq!(reg.len(), 9);
    }
}
