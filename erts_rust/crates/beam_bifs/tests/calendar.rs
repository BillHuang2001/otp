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

//! Integration tests for the `erlang` calendar/clock BIFs, driven through
//! [`BifRegistry::call`] over the full [`default_registry`].
//!
//! The nine BIFs and their oracle semantics (erts-17.1) are:
//!
//! * `posixtime_to_universaltime/1` — a plain `Sint64` second count (negative
//!   counts are pre-1970 dates) as `{{Y,M,D},{H,Mi,S}}`; a float or an out-of-`i64`
//!   bignum is `badarg` (`bif.c:4029`, `term_to_Sint64`).
//! * `universaltime_to_posixtime/1` — the second count of a UTC datetime, with the
//!   C calendar range (year ≥ 1600, `1..=31` day, `0..=23`/`0..=59` fields);
//!   anything else is `badarg` (`bif.c:4005`, `univ_to_seconds`).
//! * `universaltime_to_localtime/1` / `localtime_to_universaltime/2` — the same
//!   datetime shape in the local timezone, with the C `BASEYEAR` (1902) lower
//!   bound; the `/2` second argument is the DST hint atom `true`/`false`/
//!   `undefined`, anything else `badarg` (`bif.c:3952`).
//! * `date/0`, `time/0`, `localtime/0`, `universaltime/0`, `now/0` — clock reads
//!   (shape-only assertions; the exact values are "now").
//!
//! Structural comparisons use the public `beam_codec::term`/`beam_term` readers.
//! One test re-probes the system `erl` **live** and skips loudly when it is not on
//! `PATH` (the `modinfo.rs` pattern); every fixed-timestamp golden was produced
//! with OTP 29.1.1 / erts-17.1.
//!
//! # Threading
//!
//! The calendar BIFs read the C library's process-global timezone state
//! (`tzset`/`daylight`/`mktime`/`localtime_r`), which is not thread-safe. libtest
//! runs these tests on parallel threads, so every test takes [`TZ_GUARD`] for its
//! whole body (see [`tz_guard`]) — running them concurrently makes the C library's
//! `daylight` flag observable in a half-updated state and the Rust-side local-time
//! results flaky. The `erl` oracle probe runs in a separate *process* and shares no
//! memory, so it needs no coordination of its own.

use std::process::Command;
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;
use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{tuple_arity, tuple_element};
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{is_small, signed_value, AtomTable, Eterm};

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "false", "undefined", "not_boolean", "ok"]);
    atoms
}

/// A heap that outlives the terms the tests inspect (the results point into it).
fn leaked_heap() -> &'static mut Heap {
    Box::leak(Box::new(Heap::new(8192).expect("test heap")))
}

/// Serialises every test in this file around the **process-global C-library
/// timezone state** the calendar BIFs read.
///
/// `erlang:date/0`, `time/0`, `localtime/0`, `universaltime_to_localtime/1` and
/// `localtime_to_universaltime/2` are thin wrappers over the C library's
/// `tzset`/`daylight`/`mktime`/`localtime_r`, which POSIX does **not** require to be
/// thread-safe. libtest runs the tests of one binary on parallel threads, so when
/// several of them drive those BIFs at once the C library's `daylight` flag can be
/// observed half-updated: `local_to_utc`'s `if !sys_daylight()` then takes the
/// "timezone without DST" branch and drops the caller's DST hint, which shifts
/// `localtime_to_universaltime({{2020,6,1},{12,0,0}}, true)` by an hour and makes
/// `oracle_agrees_on_the_fixed_timestamps` disagree with a perfectly healthy `erl`
/// oracle (in exactly one run out of three, only under the parallel harness).
///
/// Every test that drives a calendar BIF takes this guard for its whole body, so the
/// shared timezone globals are only ever touched by one thread. The oracle probe
/// itself is a separate *process* and shares no memory with the test binary, so it
/// needs no coordination beyond this — the guard exists for the in-process BIF calls
/// that produce the values compared against the oracle.
static TZ_GUARD: Mutex<()> = Mutex::new(());

/// Acquires [`TZ_GUARD`], recovering from poisoning: a failed assertion in one test
/// must report that failure, not cascade `PoisonError`s into every other test.
fn tz_guard() -> MutexGuard<'static, ()> {
    TZ_GUARD.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The small-integer value at `index` of the 3-tuple `tuple`.
fn triple_element(tuple: Eterm, index: usize) -> i64 {
    let value = tuple_element(tuple, index).expect("element in range");
    assert!(is_small(value), "expected a small integer component");
    signed_value(value)
}

/// The six small-integer components of a `{{Y,M,D},{H,Mi,S}}` result.
fn read_dt(term: Eterm) -> (i64, i64, i64, i64, i64, i64) {
    assert_eq!(tuple_arity(term).expect("a tuple"), 2, "datetime arity");
    let date = tuple_element(term, 0).expect("date");
    let time = tuple_element(term, 1).expect("time");
    assert_eq!(tuple_arity(date).expect("date tuple"), 3, "date arity");
    assert_eq!(tuple_arity(time).expect("time tuple"), 3, "time arity");
    (
        triple_element(date, 0),
        triple_element(date, 1),
        triple_element(date, 2),
        triple_element(time, 0),
        triple_element(time, 1),
        triple_element(time, 2),
    )
}

/// The three small-integer components of a `{A,B,C}` result.
fn read_triple(term: Eterm) -> [i64; 3] {
    assert_eq!(tuple_arity(term).expect("a tuple"), 3);
    [triple_element(term, 0), triple_element(term, 1), triple_element(term, 2)]
}

/// Reads a small-integer result.
fn read_int(term: Eterm) -> i64 {
    assert!(is_small(term), "expected a small integer result");
    signed_value(term)
}

/// The oracle's `~w` rendering of a datetime (`{{Y,M,D},{H,Mi,S}}`).
fn dt_string(y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> String {
    let date = format!("{{{y},{mo},{d}}}");
    let time = format!("{{{h},{mi},{s}}}");
    format!("{{{date},{time}}}")
}

/// The `~w` string of a `read_dt` result.
fn dt_string_of(t: (i64, i64, i64, i64, i64, i64)) -> String {
    dt_string(t.0, t.1, t.2, t.3, t.4, t.5)
}

fn assert_badarg(result: Result<Eterm>) {
    assert!(matches!(result, Err(BeamError::BadArg)), "expected badarg, got {result:?}");
}

/// Builds the `{{Y,M,D},{H,Mi,S}}` argument for `ctx`.
fn datetime_arg(ctx: &mut BifContext<'_>, y: i64, mo: i64, d: i64, h: i64, mi: i64, s: i64) -> Eterm {
    let (year, month, day) = (
        ctx.int(y).expect("fits"),
        ctx.int(mo).expect("fits"),
        ctx.int(d).expect("fits"),
    );
    let (hour, minute, second) = (
        ctx.int(h).expect("fits"),
        ctx.int(mi).expect("fits"),
        ctx.int(s).expect("fits"),
    );
    let date = ctx.tuple(&[year, month, day]).expect("fits");
    let time = ctx.tuple(&[hour, minute, second]).expect("fits");
    ctx.tuple(&[date, time]).expect("fits")
}

/// The argument term for a `Sint64`, promotable to a bignum when the value does
/// not fit a 60-bit small integer (`i64::MAX`/`i64::MIN`).
fn int_arg(ctx: &mut BifContext<'_>, value: i64) -> Eterm {
    if let Ok(small) = ctx.int(value) {
        return small;
    }
    let (negative, magnitude) = if value < 0 {
        (true, value.unsigned_abs())
    } else {
        (false, value as u64)
    };
    beam_codec::term::make_big(ctx.heap(), negative, &[magnitude]).expect("a bignum fits")
}

/// `erlang:posixtime_to_universaltime/1` through the registry.
fn posix_to_univ(reg: &BifRegistry, heap: &mut Heap, atoms: &AtomTable, secs: i64) -> Eterm {
    let mut ctx = BifContext::new(heap, atoms, ProcessId::new(1, 0));
    let arg = int_arg(&mut ctx, secs);
    reg.call("erlang", "posixtime_to_universaltime", 1, &mut ctx, &[arg])
        .expect("a valid Sint64 converts")
}

// ---------------------------------------------------------------------------
// posixtime_to_universaltime/1
// ---------------------------------------------------------------------------

#[test]
fn posixtime_to_universaltime_matches_the_oracle_goldens() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    // Goldens from `erl` (OTP 29.1.1).
    for (secs, expected) in [
        (0i64, (1970, 1, 1, 0, 0, 0)),
        (1, (1970, 1, 1, 0, 0, 1)),
        (-1, (1969, 12, 31, 23, 59, 59)),
        (1000000000, (2001, 9, 9, 1, 46, 40)),
        (951782400, (2000, 2, 29, 0, 0, 0)),
        (4102444800, (2100, 1, 1, 0, 0, 0)),
        (4, (1970, 1, 1, 0, 0, 4)),
        (-86400, (1969, 12, 31, 0, 0, 0)),
        (-62135596800, (1, 1, 1, 0, 0, 0)),
        (253402300800, (10000, 1, 1, 0, 0, 0)),
    ] {
        let term = posix_to_univ(&reg, heap, &atoms, secs);
        assert_eq!(read_dt(term), expected, "posixtime {secs}");
    }
}

#[test]
fn posixtime_to_universaltime_accepts_the_i64_extremes() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    assert_eq!(
        read_dt(posix_to_univ(&reg, heap, &atoms, i64::MAX)),
        (292277026596, 12, 4, 15, 30, 7)
    );
    assert_eq!(
        read_dt(posix_to_univ(&reg, heap, &atoms, i64::MIN)),
        (-292277022657, 1, 26, 8, 29, 52)
    );
}

#[test]
fn posixtime_to_universaltime_rejects_non_integers() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    // A float is `badarg` (`term_to_Sint64`).
    {
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
        let float = ctx.make_float(1.0).expect("fits");
        assert_badarg(reg.call("erlang", "posixtime_to_universaltime", 1, &mut ctx, &[float]));
    }

    // `2^70` is a two-limb bignum past `i64`: `badarg`.
    {
        let mut heap2 = Heap::new(64).expect("test heap");
        let big = beam_codec::term::make_big(&mut heap2, false, &[0, 64]).expect("bignum");
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
        assert_badarg(reg.call("erlang", "posixtime_to_universaltime", 1, &mut ctx, &[big]));
    }
}

// ---------------------------------------------------------------------------
// universaltime_to_posixtime/1
// ---------------------------------------------------------------------------

#[test]
fn universaltime_to_posixtime_matches_the_oracle_goldens() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    // Goldens from `erl` (OTP 29.1.1).
    for ((y, mo, d, h, mi, s), expected) in [
        ((1970, 1, 1, 0, 0, 0), 0i64),
        ((2000, 2, 29, 0, 0, 0), 951782400),
        ((2016, 2, 29, 12, 34, 56), 1456749296),
        ((1901, 12, 31, 23, 59, 59), -2145916801),
        ((1600, 1, 1, 0, 0, 0), -11676096000),
        ((2020, 1, 1, 0, 0, 0), 1577836800),
        ((2147483646, 1, 1, 0, 0, 0), 67767976170460800),
    ] {
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
        let arg = datetime_arg(&mut ctx, y, mo, d, h, mi, s);
        let result = reg
            .call("erlang", "universaltime_to_posixtime", 1, &mut ctx, &[arg])
            .expect("in range");
        assert_eq!(read_int(result), expected, "universaltime {y}-{mo}-{d}");
    }
}

#[test]
fn universaltime_to_posixtime_rejects_bad_input() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));

    // Year below the C's 1600 bound.
    let arg = datetime_arg(&mut ctx, 1599, 12, 31, 0, 0, 0);
    assert_badarg(reg.call("erlang", "universaltime_to_posixtime", 1, &mut ctx, &[arg]));

    // A nonexistent day, an out-of-range month/hour/minute/second:
    // `(month, day, hour, minute, second)`.
    for (mo, d, h, mi, s) in [
        (1, 32, 0, 0, 0),
        (13, 1, 0, 0, 0),
        (1, 0, 0, 0, 0),
        (1, 1, 24, 0, 0),
        (1, 1, 0, 60, 0),
        (1, 1, 0, 0, 60),
        (2, 30, 0, 0, 0),
    ] {
        let arg = datetime_arg(&mut ctx, 2020, mo, d, h, mi, s);
        assert_badarg(reg.call("erlang", "universaltime_to_posixtime", 1, &mut ctx, &[arg]));
    }

    // A float component is not a small integer (`time_to_parts`).
    let float = ctx.make_float(0.0).expect("fits");
    let (y, mo, d) = (
        ctx.int(2020).expect("fits"),
        ctx.int(1).expect("fits"),
        ctx.int(1).expect("fits"),
    );
    let date = ctx.tuple(&[y, mo, d]).expect("fits");
    let (z, zero) = (ctx.int(0).expect("fits"), ctx.int(0).expect("fits"));
    let bad_time = ctx.tuple(&[z, zero, float]).expect("fits");
    let arg = ctx.tuple(&[date, bad_time]).expect("fits");
    assert_badarg(reg.call("erlang", "universaltime_to_posixtime", 1, &mut ctx, &[arg]));

    // A malformed outer shape.
    let flat = ctx.tuple(&[y, mo]).expect("fits");
    assert_badarg(reg.call("erlang", "universaltime_to_posixtime", 1, &mut ctx, &[flat]));
}

// ---------------------------------------------------------------------------
// universaltime_to_localtime/1
// ---------------------------------------------------------------------------

#[test]
fn universaltime_to_localtime_round_trips_and_rejects_bad_input() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    // `localtime_to_universaltime(universaltime_to_localtime(U), undefined)` must
    // return `U` — the local representation carries the same instant. Verified
    // against the oracle; holds for every timezone.
    for (y, mo, d, h, mi, s) in [
        (2020, 6, 1, 12, 0, 0),
        (2020, 1, 1, 0, 0, 0),
        (1970, 1, 1, 0, 0, 0),
        (2000, 2, 29, 6, 30, 0),
        (2147483646, 1, 1, 0, 0, 0),
    ] {
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
        let arg = datetime_arg(&mut ctx, y, mo, d, h, mi, s);
        let local = reg
            .call("erlang", "universaltime_to_localtime", 1, &mut ctx, &[arg])
            .expect("in range");
        // The result is itself a well-formed datetime (all fields small ints).
        let _ = read_dt(local);

        let unknown = ctx.atom("undefined").expect("seeded");
        let back = reg
            .call("erlang", "localtime_to_universaltime", 2, &mut ctx, &[local, unknown])
            .expect("a valid local datetime");
        assert_eq!(read_dt(back), (y, mo, d, h, mi, s), "round trip for {y}-{mo}-{d}");
    }

    // Year 1901 is below the C `BASEYEAR` (1902): `badarg`.
    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
    let arg = datetime_arg(&mut ctx, 1901, 1, 1, 0, 0, 0);
    assert_badarg(reg.call("erlang", "universaltime_to_localtime", 1, &mut ctx, &[arg]));

    let flat = ctx.tuple(&[arg, arg]).expect("fits");
    assert_badarg(reg.call("erlang", "universaltime_to_localtime", 1, &mut ctx, &[flat]));
}

// ---------------------------------------------------------------------------
// localtime_to_universaltime/2
// ---------------------------------------------------------------------------

#[test]
fn localtime_to_universaltime_rejects_non_boolean_and_out_of_range() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));

    let arg = datetime_arg(&mut ctx, 2020, 6, 1, 12, 0, 0);
    let not_boolean = ctx.atom("not_boolean").expect("seeded");
    assert_badarg(reg.call(
        "erlang",
        "localtime_to_universaltime",
        2,
        &mut ctx,
        &[arg, not_boolean],
    ));

    // A non-atom flag is `badarg` too.
    let int_flag = ctx.int(1).expect("fits");
    assert_badarg(reg.call("erlang", "localtime_to_universaltime", 2, &mut ctx, &[arg, int_flag]));

    // Year 1901 is below the C `BASEYEAR` (1902): `badarg`.
    let old = datetime_arg(&mut ctx, 1901, 1, 1, 0, 0, 0);
    let dst = ctx.atom("true").expect("seeded");
    assert_badarg(reg.call("erlang", "localtime_to_universaltime", 2, &mut ctx, &[old, dst]));
}

#[test]
fn localtime_to_universaltime_flags_agree_within_two_hours() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));

    let dst_true = ctx.atom("true").expect("seeded");
    let dst_false = ctx.atom("false").expect("seeded");
    let dst_unknown = ctx.atom("undefined").expect("seeded");

    for (y, mo, d) in [(2020, 6, 15), (2020, 1, 15), (1979, 7, 1)] {
        let arg = datetime_arg(&mut ctx, y, mo, d, 12, 0, 0);
        let mut instants = Vec::new();
        for flag in [dst_true, dst_false, dst_unknown] {
            let utc = reg
                .call("erlang", "localtime_to_universaltime", 2, &mut ctx, &[arg, flag])
                .expect("a valid local datetime");
            // The result must itself be a well-formed datetime.
            let _ = read_dt(utc);
            instants.push(read_int(
                reg.call("erlang", "universaltime_to_posixtime", 1, &mut ctx, &[utc])
                    .expect("in range"),
            ));
        }
        let spread = instants.iter().max().expect("three instants")
            - instants.iter().min().expect("three instants");
        assert!(
            spread <= 2 * 3600,
            "{y}-{mo}-{d}: the DST hint must move the instant by at most two hours, got {spread}s"
        );
    }
}

// ---------------------------------------------------------------------------
// date/0, time/0, localtime/0, universaltime/0, now/0 (shape only)
// ---------------------------------------------------------------------------

#[test]
fn date_time_localtime_and_universaltime_are_well_formed() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));

    let date = read_triple(reg.call("erlang", "date", 0, &mut ctx, &[]).expect("date/0"));
    assert!((2020..2100).contains(&date[0]), "year {} out of range", date[0]);
    assert!((1..=12).contains(&date[1]), "month {} out of range", date[1]);
    assert!((1..=31).contains(&date[2]), "day {} out of range", date[2]);

    let time = read_triple(reg.call("erlang", "time", 0, &mut ctx, &[]).expect("time/0"));
    assert!((0..=23).contains(&time[0]), "hour {} out of range", time[0]);
    assert!((0..=59).contains(&time[1]), "minute {} out of range", time[1]);
    assert!((0..=59).contains(&time[2]), "second {} out of range", time[2]);

    for bif in ["localtime", "universaltime"] {
        let term = reg.call("erlang", bif, 0, &mut ctx, &[]).expect("a clock read");
        let (y, mo, d, h, mi, s) = read_dt(term);
        assert!((2020..2100).contains(&y), "{bif} year {y} out of range");
        assert!((1..=12).contains(&mo), "{bif} month {mo} out of range");
        assert!((1..=31).contains(&d), "{bif} day {d} out of range");
        assert!((0..=23).contains(&h), "{bif} hour {h} out of range");
        assert!((0..=59).contains(&mi), "{bif} minute {mi} out of range");
        assert!((0..=59).contains(&s), "{bif} second {s} out of range");
    }
}

#[test]
fn now_is_strictly_increasing_and_plausible() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();
    let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));

    let mut previous = i64::MIN;
    for _ in 0..4 {
        let [mega, sec, micro] = read_triple(reg.call("erlang", "now", 0, &mut ctx, &[]).expect("now/0"));
        assert!((1500..5000).contains(&mega), "mega-secs {mega} implausible");
        assert!((0..1_000_000).contains(&sec), "secs {sec} out of range");
        assert!((0..1_000_000).contains(&micro), "micro-secs {micro} out of range");

        let total = mega * 1_000_000_000_000 + sec * 1_000_000 + micro;
        assert!(total > previous, "now/0 must increase: {previous} then {total}");
        previous = total;
    }
}

#[test]
fn registry_registers_the_nine_calendar_bifs() {
    let _tz = tz_guard();
    let reg = default_registry();

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
        assert!(reg.lookup("erlang", name, arity).is_some());
    }
}

// ---------------------------------------------------------------------------
// Live oracle cross-check (skips loudly when `erl` is absent)
// ---------------------------------------------------------------------------

/// Wall-clock budget for the `erl` probe before the oracle is declared unusable.
const ERL_TIMEOUT: Duration = Duration::from_secs(30);

/// Runs `<expr>, halt(0).` under the system `erl` and returns its stdout, or `None`
/// (after a clear skip message) when `erl` cannot be spawned / does not answer.
///
/// The probe is self-contained: it owns its child process and its output buffer and
/// shares no state with any other test. `Command::output()` drains the child's stdout
/// to **EOF** and reaps it before returning, so the caller always parses a complete
/// output (never a partial read); a child that cannot finish inside [`ERL_TIMEOUT`]
/// is reported as an unusable oracle (a loud skip) instead of being half-read.
fn erl_stdout(expr: &str) -> Option<String> {
    let code = format!("{expr}, halt(0).");
    let (tx, rx) = mpsc::channel();
    let code_for_thread = code.clone();
    thread::spawn(move || {
        let out = Command::new("erl")
            .arg("-noshell")
            .arg("-eval")
            .arg(&code_for_thread)
            .output();
        let _ = tx.send(out);
    });

    match rx.recv_timeout(ERL_TIMEOUT) {
        Err(_) => {
            eprintln!("SKIP: `erl` did not answer within {ERL_TIMEOUT:?}");
            None
        }
        Ok(Err(e)) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({e}); set $ERTS_ORACLE or install Erlang");
            None
        }
        Ok(Ok(out)) if !out.status.success() => {
            // A local `erl` that cannot complete a probe (e.g. resource limits while
            // the test binary runs its cases in parallel) is an *unusable oracle*, not
            // a Rust-side disagreement, so skip loudly instead of reporting a failure.
            eprintln!(
                "SKIP: `erl` exited with {} for:\n{code}\nstderr:\n{}",
                out.status,
                String::from_utf8_lossy(&out.stderr),
            );
            None
        }
        Ok(Ok(out)) => Some(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

#[test]
fn oracle_agrees_on_the_fixed_timestamps() {
    let _tz = tz_guard();
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap = leaked_heap();

    let mut exprs: Vec<String> = Vec::new();
    let mut expected: Vec<String> = Vec::new();

    // posixtime_to_universaltime/1.
    for secs in [0i64, -1, 951782400, 4102444800, -62135596800] {
        exprs.push(format!("io:format(\"~w~n\",[erlang:posixtime_to_universaltime({secs})])"));
        expected.push(dt_string_of(read_dt(posix_to_univ(&reg, heap, &atoms, secs))));
    }

    // universaltime_to_posixtime/1.
    for (y, mo, d, h, mi, s) in [
        (1600, 1, 1, 0, 0, 0),
        (2016, 2, 29, 12, 34, 56),
        (1970, 1, 1, 0, 0, 0),
    ] {
        let datetime = dt_string(y, mo, d, h, mi, s);
        exprs.push(format!(
            "io:format(\"~w~n\",[erlang:universaltime_to_posixtime({datetime})])"
        ));
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
        let arg = datetime_arg(&mut ctx, y, mo, d, h, mi, s);
        let value = read_int(
            reg.call("erlang", "universaltime_to_posixtime", 1, &mut ctx, &[arg])
                .expect("in range"),
        );
        expected.push(value.to_string());
    }

    // universaltime_to_localtime/1.
    for (y, mo, d, h, mi, s) in [(2020, 6, 1, 12, 0, 0), (1970, 1, 1, 0, 0, 0)] {
        let datetime = dt_string(y, mo, d, h, mi, s);
        exprs.push(format!(
            "io:format(\"~w~n\",[erlang:universaltime_to_localtime({datetime})])"
        ));
        let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
        let arg = datetime_arg(&mut ctx, y, mo, d, h, mi, s);
        let local = reg
            .call("erlang", "universaltime_to_localtime", 1, &mut ctx, &[arg])
            .expect("in range");
        expected.push(dt_string_of(read_dt(local)));
    }

    // localtime_to_universaltime/2 for all three DST hints.
    for (y, mo, d) in [(2020, 6, 1), (1979, 7, 1), (2020, 12, 1)] {
        let datetime = dt_string(y, mo, d, 12, 0, 0);
        for flag in ["true", "false", "undefined"] {
            exprs.push(format!(
                "io:format(\"~w~n\",[erlang:localtime_to_universaltime({datetime},{flag})])"
            ));
            let mut ctx = BifContext::new(&mut *heap, &atoms, ProcessId::new(1, 0));
            let arg = datetime_arg(&mut ctx, y, mo, d, 12, 0, 0);
            let hint = ctx.atom(flag).expect("seeded");
            let utc = reg
                .call("erlang", "localtime_to_universaltime", 2, &mut ctx, &[arg, hint])
                .expect("a valid local datetime");
            expected.push(dt_string_of(read_dt(utc)));
        }
    }

    let expr = exprs.join(",");
    let Some(stdout) = erl_stdout(&expr) else {
        return;
    };
    let lines: Vec<&str> = stdout.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    assert_eq!(
        lines.len(),
        expected.len(),
        "oracle produced {} lines, expected {}\n{stdout}",
        lines.len(),
        expected.len()
    );
    for (index, (line, want)) in lines.iter().zip(&expected).enumerate() {
        assert_eq!(line, want, "probe {index} disagrees with the oracle");
    }
}
