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

//! The C-library local/universal-time bridge for [`super`]'s calendar BIFs.
//!
//! `date/0`, `time/0` and `localtime/0` report the **local** timezone, which only
//! the C library knows: erts' `get_localtime`/`get_date`/`get_time` call
//! `localtime_r` and `get_universaltime` calls `gmtime_r`
//! (`../../erts/emulator/beam/erl_time_sup.c:1515-1586`).
//! `universaltime_to_localtime/1` calls `localtime_r` on a computed clock and
//! `localtime_to_universaltime/2` calls `mktime` then `gmtime_r`
//! (`erl_time_sup.c:1734-1849`). This module wraps exactly those `libc` calls; it
//! is the crate's `#[allow(unsafe_code)]` module for the C time library (the
//! `access.rs` precedent), so `calendar.rs` stays safe Rust.
//!
//! Everything the C does **outside** the C library — the calendar arithmetic, the
//! range checks and the `mdays`/leap-year tables (`gregday`, `univ_to_seconds`,
//! `seconds_to_univ`) — lives in [`super`], which never uses `unsafe`.
//!
//! # Safety invariant
//!
//! Every `unsafe` block below passes a `libc` time function a fully initialised
//! local `struct tm` (or a `*mut` to a live `time_t`) and null-checks the returned
//! pointer before dereferencing it. The `daylight` external is a plain scalar read
//! taken under the crate's single-threaded BIF execution model; `tzset()` — which
//! only primes the C library's timezone tables — runs **once per process**
//! ([`TZ_INIT`], see [`sys_daylight`]).
#![allow(unsafe_code)]

use std::sync::OnceLock;

use beam_common::{BeamError, Result};

use crate::error::badarg;

extern "C" {
    /// The C-library DST flag from `<time.h>`: nonzero when the active timezone
    /// defines DST transitions for some period (`sys_time.c` reads the same
    /// `daylight` global into `sys_daylight`, `erl_time_sup.c:1764`).
    static mut daylight: libc::c_int;
    /// The C-library timezone-table initialiser from `<time.h>`; erts' `sys_tzset`
    /// is the same call. It populates [`daylight`].
    fn tzset();
}

/// A broken-down calendar time: the fields of a C `struct tm` in Erlang's shape
/// (`{Year, Month, Day}`, `{Hour, Minute, Second}`), with `Year` the full year
/// (the C `tm_year + 1900`) and `Month` 1-based (the C `tm_mon + 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tm {
    /// The full year (`tm_year + 1900`).
    pub(crate) year: i64,
    /// The month, `1..=12` (`tm_mon + 1`).
    pub(crate) month: i64,
    /// The day of month, `1..=31`.
    pub(crate) day: i64,
    /// The hour, `0..=23`.
    pub(crate) hour: i64,
    /// The minute, `0..=59`.
    pub(crate) minute: i64,
    /// The second, `0..=59`.
    pub(crate) second: i64,
}

/// The `Tm` view of a `struct tm` (`erl_time_sup.c`'s `get_localtime` shape).
fn tm_view(tm: &libc::tm) -> Tm {
    Tm {
        year: i64::from(tm.tm_year) + 1900,
        month: i64::from(tm.tm_mon) + 1,
        day: i64::from(tm.tm_mday),
        hour: i64::from(tm.tm_hour),
        minute: i64::from(tm.tm_min),
        second: i64::from(tm.tm_sec),
    }
}

/// The current wall-clock time as a `time_t` (C `time((time_t *)0)`).
fn now_clock() -> libc::time_t {
    // SAFETY: `time` only reads the system clock; a null pointer asks for the
    // return value alone.
    unsafe { libc::time(core::ptr::null_mut()) }
}

/// `sys_localtime_r(clock)` — the local broken-down time, or `None` when the C
/// library cannot convert `clock` (the C would dereference a null pointer; here it
/// is reported).
fn localtime_r_at(clock: libc::time_t) -> Option<Tm> {
    // SAFETY: `tm` is zero-initialised and `localtime_r` writes a fully populated
    // `struct tm` into it (or returns null, checked below).
    let mut tm: libc::tm = unsafe { core::mem::zeroed() };
    // SAFETY: `clock` is a live local and `tm` a live local `struct tm`.
    let res = unsafe { libc::localtime_r(&clock, &mut tm) };
    if res.is_null() {
        None
    } else {
        Some(tm_view(&tm))
    }
}

/// `gmtime_r(clock)` — the UTC broken-down time, or `None` on failure.
fn gmtime_r_at(clock: libc::time_t) -> Option<Tm> {
    // SAFETY: as `localtime_r_at`.
    let mut tm: libc::tm = unsafe { core::mem::zeroed() };
    // SAFETY: `clock` is a live local and `tm` a live local `struct tm`.
    let res = unsafe { libc::gmtime_r(&clock, &mut tm) };
    if res.is_null() {
        None
    } else {
        Some(tm_view(&tm))
    }
}

/// Guards the **one** `tzset()` a process needs — see [`sys_daylight`].
static TZ_INIT: OnceLock<()> = OnceLock::new();

/// The C-library `daylight` flag, after ensuring the timezone tables are loaded
/// (`sys_daylight` / `sys_tzset`, `erts/emulator/sys/unix/sys_time.c`).
///
/// The C runs `tzset()` on every read; here it runs **at most once per process**
/// (`TZ_INIT`), because the timezone tables only need priming once and this crate
/// never changes `TZ`. The `daylight` scalar itself is then **re-read on every
/// call**, and must be: glibc's `daylight` is not a pure function of the zone — a
/// `mktime(3)`/`localtime_r(3)` conversion updates it, and a later `tzset()` does
/// not recompute it while the timezone file is unchanged. `local_to_utc` branches on
/// this flag (`if !sys_daylight() { tm.tm_isdst = 0; }`), so freezing the *value* in
/// a `OnceLock` would change `localtime_to_universaltime/2` results; only the
/// setup call is cached.
fn sys_daylight() -> bool {
    TZ_INIT.get_or_init(|| {
        // SAFETY: `tzset` has no preconditions and only primes the C library's
        // process-global timezone tables.
        unsafe {
            tzset();
        }
    });
    // SAFETY: reading a scalar `c_int` takes no reference and is a plain copy.
    let flag = unsafe { daylight };
    flag != 0
}

/// `time(NULL)` then `sys_localtime_r` — the C `get_localtime`/`get_date`/
/// `get_time` clock read.
///
/// # Errors
///
/// [`BeamError::Io`] when the C library cannot convert the system clock (the C
/// ignores the null return and would fault; the failure is surfaced here).
pub(crate) fn now_local() -> Result<Tm> {
    localtime_r_at(now_clock()).ok_or_else(|| BeamError::Io(std::io::Error::last_os_error()))
}

/// `time(NULL)` then `gmtime_r` — the C `get_universaltime` clock read.
///
/// # Errors
///
/// [`BeamError::Io`] when the C library cannot convert the system clock.
pub(crate) fn now_utc() -> Result<Tm> {
    gmtime_r_at(now_clock()).ok_or_else(|| BeamError::Io(std::io::Error::last_os_error()))
}

/// `localtime_r(clock)` for an explicit POSIX clock — the tail of the C
/// `univ_to_local` (`erl_time_sup.c:1837`).
///
/// # Errors
///
/// [`BeamError::BadArg`] when the C library cannot convert `clock` (the C returns
/// `0`, which the BIF reports as `badarg`).
pub(crate) fn localtime_of(clock: i64) -> Result<Tm> {
    localtime_r_at(clock as libc::time_t).ok_or_else(badarg)
}

/// The C `erl_mktime` (`erl_time_sup.c:1607`): `mktime`, retrying one second
/// earlier when it answers `-1` (a legitimate near-epoch value).
fn erl_mktime(tm: &mut libc::tm) -> Option<i64> {
    // SAFETY: `tm` is a live local `struct tm` and `mktime` normalises it in place.
    let clock = unsafe { libc::mktime(tm) };
    if clock != -1 {
        return Some(i64::from(clock));
    }
    tm.tm_sec -= 1;
    // SAFETY: as above.
    let retry = unsafe { libc::mktime(tm) };
    tm.tm_sec += 1;
    if retry == -2 {
        Some(-1)
    } else {
        None
    }
}

/// The C `local_to_univ` (`erl_time_sup.c:1734`): interpret a local broken-down
/// time as UTC-broken-down, honouring the `isdst` hint (`1` true, `0` false, `-1`
/// unknown).
///
/// # Errors
///
/// [`BeamError::BadArg`] when a field does not fit the C `struct tm`, `mktime`
/// reports failure, or `gmtime_r` cannot convert the resulting clock — every case
/// the C reports as `0`/`badarg`.
pub(crate) fn local_to_utc(t: Tm, isdst: libc::c_int) -> Result<Tm> {
    // SAFETY: a zero-initialised `struct tm`; every field the C sets is written
    // below before the call, and the remaining (`tm_wday`/`tm_yday`/`tm_gmtoff`/
    // `tm_zone`) are ignored by `mktime`.
    let mut tm: libc::tm = unsafe { core::mem::zeroed() };
    tm.tm_year = i32::try_from(t.year - 1900).map_err(|_| badarg())?;
    tm.tm_mon = i32::try_from(t.month - 1).map_err(|_| badarg())?;
    tm.tm_mday = i32::try_from(t.day).map_err(|_| badarg())?;
    tm.tm_hour = i32::try_from(t.hour).map_err(|_| badarg())?;
    tm.tm_min = i32::try_from(t.minute).map_err(|_| badarg())?;
    tm.tm_sec = i32::try_from(t.second).map_err(|_| badarg())?;
    tm.tm_isdst = isdst;

    // A timezone without DST ignores the flag, as the C simulates
    // (`erl_time_sup.c:1764`).
    if !sys_daylight() {
        tm.tm_isdst = 0;
    }

    let clock = erl_mktime(&mut tm).ok_or_else(badarg)?;
    gmtime_r_at(clock as libc::time_t).ok_or_else(badarg)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `sys_daylight` primes the C timezone tables with `tzset()` **once**
    /// ([`TZ_INIT`]) and then re-reads the `daylight` scalar on every call — the flag
    /// is observed exactly as an uncached `tzset()` + read would, but without
    /// repeating the setup.
    #[test]
    fn tzset_runs_at_most_once_and_the_flag_is_re_read() {
        let first = sys_daylight();
        assert!(
            TZ_INIT.get().is_some(),
            "the first call must run the one-time tzset"
        );
        assert_eq!(sys_daylight(), first, "the flag reads are consistent");
        // The `daylight` scalar is shared process state, not a frozen snapshot: a
        // direct read agrees with the accessor (the cache holds only the setup call).
        // SAFETY: reading a scalar `c_int` takes no reference and is a plain copy.
        let fresh = unsafe { daylight != 0 };
        assert_eq!(first, fresh);
    }

    /// `local_to_utc` must keep seeing the **current** `daylight` flag: the cache
    /// covers `tzset()`, not the value, so a `mktime(3)`-driven change to the scalar
    /// is still observable (this is why caching the `bool` would be wrong).
    #[test]
    fn the_flag_is_not_frozen_behind_the_cache() {
        let _ = sys_daylight();
        // A conversion like `erl_mktime`'s updates the C library's process-global
        // `daylight`; whatever it leaves behind, a later `sys_daylight()` reports it.
        // SAFETY: a zero-initialised `struct tm`; only the fields `mktime` reads are
        // written, and `mktime` normalises it in place.
        let mut tm: libc::tm = unsafe { core::mem::zeroed() };
        tm.tm_year = 2020 - 1900;
        tm.tm_mon = 5;
        tm.tm_mday = 1;
        tm.tm_hour = 12;
        tm.tm_isdst = -1;
        // SAFETY: `tm` is a live local `struct tm`.
        let _ = unsafe { libc::mktime(&mut tm) };
        // SAFETY: a scalar `c_int` read.
        let after = unsafe { daylight != 0 };
        assert_eq!(sys_daylight(), after);
    }
}
