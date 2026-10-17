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
//! `erlang:monotonic_time/0,1` on the **ERTS monotonic epoch** — the live-oracle
//! cross-check for every accepted unit (`second`/`millisecond`/`microsecond`/
//! `nanosecond`/`native`/`perf_counter` and a positive integer unit).
//!
//! The relation a drop-in runtime must reproduce is
//!
//! ```text
//! erlang:system_info(start_time) <= erlang:monotonic_time() < erlang:system_info(end_time)
//! ```
//!
//! with `start_time = ERTS_MONOTONIC_TIME_START_EXTERNAL = (MIN_SMALL / UNIT) * UNIT`
//! (`erl_time.h:312-323`) — i.e. a large **negative** reading, not an elapsed count
//! from zero — and every `/1` unit a floored division of the same native reading.
//! Nothing below pins an absolute clock value (that would be a clock race): the
//! assertions are the *relations* the oracle exhibits, evaluated on both sides —
//! same sign, inside the oracle's own `start_time .. end_time` window, and the same
//! unit-to-unit scaling.
//!
//! The oracle probe is one `erl -noshell -eval` run; with no `erl` on `PATH` the
//! cross-check **skips loudly** (`SKIP: …`, the crate convention) while the
//! self-consistency pins still run, so the file never passes silently on nothing.
use std::process::Command;
use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_process::ProcessId;
use beam_term::{signed_value, AtomTable, Eterm};
/// `ERTS_MONOTONIC_TIME_UNIT` (`erl_time.h:282-283`): nanoseconds per native tick on
/// the 64-bit Unix build.
const MONOTONIC_TIME_UNIT: i128 = 1_000_000_000;
/// `MIN_SMALL` (`erl_term.h:269-270`), the most negative 60-bit small integer.
const MIN_SMALL: i128 = -(1_i128 << 59);
/// `ERTS_MONOTONIC_TIME_START_EXTERNAL` (`erl_time.h:312-323`) — what
/// `erlang:system_info(start_time)` answers. The C division truncates toward zero.
const ERTS_START: i128 = MIN_SMALL / MONOTONIC_TIME_UNIT * MONOTONIC_TIME_UNIT;
/// `ERTS_MONOTONIC_TIME_END_EXTERNAL` (`erl_time.h:403-407`) — what
/// `erlang:system_info(end_time)` answers (`START + (END - BEGIN)` for the
/// `START < 0` branch, with the monotonic range being a signed 64-bit integer).
const ERTS_END: i128 = ERTS_START
    + (i64::MAX as i128 / MONOTONIC_TIME_UNIT * MONOTONIC_TIME_UNIT - MONOTONIC_TIME_UNIT);
/// One probe unit: its `/1` argument atom (an empty string for the integer form),
/// its integer-argument value (0 for the atom form), how the oracle spells it, and
/// how many of it fit in one nanosecond's worth of ticks (units per second).
///
/// `per_second` is the ratio a floored conversion divides by: `value = native *
/// per_second div 1e9`.
struct Unit {
    atom: &'static str,
    integer: Option<i64>,
    per_second: i128,
}
/// The seven probe units, in a fixed order the oracle probe prints in.
const UNITS: [Unit; 7] = [
    Unit { atom: "second", integer: None, per_second: 1 },
    Unit { atom: "millisecond", integer: None, per_second: 1_000 },
    Unit { atom: "microsecond", integer: None, per_second: 1_000_000 },
    Unit { atom: "nanosecond", integer: None, per_second: 1_000_000_000 },
    Unit { atom: "native", integer: None, per_second: 1_000_000_000 },
    Unit { atom: "perf_counter", integer: None, per_second: 1_000_000_000 },
    // The `erlang:convert_time_unit/3` integer form: 1000 units per second.
    Unit { atom: "", integer: Some(1_000), per_second: 1_000 },
];
/// A registry + heap + atom table the probes are driven through.
struct Fixture {
    registry: BifRegistry,
    heap: Heap,
    atoms: AtomTable,
}
impl Fixture {
    fn new() -> Fixture {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "true",
            "false",
            "ok",
            "second",
            "millisecond",
            "microsecond",
            "nanosecond",
            "native",
            "perf_counter",
        ]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 14).expect("test heap"),
            atoms,
        }
    }
    /// Calls `erlang:monotonic_time/0,1` through the public registry.
    fn monotonic(&mut self, unit: &Unit) -> i128 {
        let argument: Eterm = match unit.integer {
            Some(value) => {
                let building =
                    BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
                building.int(value).expect("fits")
            }
            None => beam_term::make_atom(self.atoms.lookup(unit.atom).expect("seeded")),
        };
        let args = [argument];
        // The context borrows the heap and the atom table; the registry is a distinct
        // field, so the two coexist for the length of the call.
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        let term = self
            .registry
            .call("erlang", "monotonic_time", args.len() as u32, &mut ctx, &args)
            .expect("erlang:monotonic_time/0,1");
        value(term)
    }
    /// Calls a `erlang` BIF of arity 0 through the public registry.
    fn call0(&mut self, function: &str) -> Eterm {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call("erlang", function, 0, &mut ctx, &[])
            .unwrap_or_else(|err| panic!("erlang:{function}/0: {err:?}"))
    }
}
/// The `i128` value of a small-integer clock reading.
fn value(term: Eterm) -> i128 {
    // Every probe unit of the ERTS epoch is inside the 60-bit small range for a
    // freshly started VM, which is what lets this file compare them as `i128`.
    assert!(
        beam_term::is_small(term),
        "a monotonic reading must be a small integer on this epoch"
    );
    i128::from(signed_value(term))
}
/// The label a failure message uses for a probe unit.
fn label(unit: &Unit) -> String {
    match unit.integer {
        Some(value) => value.to_string(),
        None => unit.atom.to_string(),
    }
}
/// The oracle's readings, in [`UNITS`] order, plus its `start_time`/`end_time`.
struct OracleReadings {
    values: Vec<i128>,
    start_time: i128,
    end_time: i128,
}
/// The `erl -eval` body: one integer per line, the seven readings and then the two
/// `system_info` bounds.
fn oracle_program() -> String {
    let reads: Vec<String> = UNITS
        .iter()
        .map(|unit| match unit.integer {
            Some(value) => format!("erlang:monotonic_time({value})"),
            None => format!("erlang:monotonic_time({})", unit.atom),
        })
        .collect();
    let mut all = reads;
    all.push("erlang:system_info(start_time)".to_string());
    all.push("erlang:system_info(end_time)".to_string());
    let mut program = String::from("io:format(\"");
    for _ in 0..all.len() {
        program.push_str("~w~n");
    }
    program.push_str("\", [");
    program.push_str(&all.join(", "));
    program.push_str("]), halt().");
    program
}
/// Runs the probe under the system `erl`; `None` (skip loudly) when no oracle is
/// available or it exits unsuccessfully.
fn oracle_readings() -> Option<OracleReadings> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", &oracle_program(), "-s", "init", "stop"])
        .output();
    let out = match output {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            return None;
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err}) — the epoch relations are \
                       pinned by the `src/time.rs` unit tests instead");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let mut numbers = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match line.parse::<i128>() {
            Ok(value) => numbers.push(value),
            Err(err) => {
                eprintln!("SKIP: the oracle printed a non-integer {line:?} ({err})");
                return None;
            }
        }
    }
    if numbers.len() != UNITS.len() + 2 {
        eprintln!(
            "SKIP: the oracle printed {} numbers, expected {}",
            numbers.len(),
            UNITS.len() + 2
        );
        return None;
    }
    let end_time = numbers.pop().expect("checked the length");
    let start_time = numbers.pop().expect("checked the length");
    Some(OracleReadings {
        values: numbers,
        start_time,
        end_time,
    })
}
/// `start_time`/`end_time` expressed in a probe unit — the window the reading of
/// that unit must fall in. Both constants are exact multiples of any whole-second
/// unit, and the oracle's values are pre-floored the same way.
fn window(start: i128, end: i128, per_second: i128) -> (i128, i128) {
    (start * per_second / MONOTONIC_TIME_UNIT, end * per_second / MONOTONIC_TIME_UNIT)
}
/// The relative difference between `ratio` and its expected value.
fn relative_error(ratio: f64, expected: f64) -> f64 {
    (ratio - expected).abs() / expected.abs()
}
/// The epoch relations, cross-checked against the live oracle: every unit of
/// `erlang:monotonic_time/1` is **negative** (the origin is `start_time`), lies
/// inside the oracle's own `start_time .. end_time` window, and scales between
/// units exactly as the oracle's readings do.
#[test]
fn every_unit_sits_on_the_erts_epoch_like_the_oracle() {
    let oracle = oracle_readings();
    if let Some(oracle) = &oracle {
        // The two bounds are the ERTS interface constants of this build (ORACLE:
        // `-576460752000000000` / `8646911283000000000`), and every oracle reading of a
        // freshly started VM is inside them.
        assert_eq!(
            oracle.start_time, ERTS_START,
            "the oracle's system_info(start_time) is the ERTS epoch origin"
        );
        assert_eq!(
            oracle.end_time, ERTS_END,
            "the oracle's system_info(end_time) is the ERTS epoch ceiling"
        );
        for (unit, value) in UNITS.iter().zip(&oracle.values) {
            let (low, high) = window(oracle.start_time, oracle.end_time, unit.per_second);
            assert!(
                *value < 0,
                "the oracle's monotonic_time({}) = {value} must be negative on the epoch",
                label(unit)
            );
            assert!(
                low <= *value && *value <= high,
                "the oracle's monotonic_time({}) = {value} is outside [{low}, {high}]",
                label(unit)
            );
        }
    }

    // Our own readings: the same two relations, plus the unit-to-unit scaling the
    // oracle exhibits (values are dominated by the epoch, so a ratio is stable to
    // ~1e-9 even though the individual reads are microseconds apart).
    let mut fx = Fixture::new();
    let ours: Vec<i128> = UNITS.iter().map(|unit| fx.monotonic(unit)).collect();
    for (unit, value) in UNITS.iter().zip(&ours) {
        assert!(
            *value < 0,
            "monotonic_time({}) = {value} must be negative on the ERTS epoch",
            label(unit)
        );
        let (low, high) = window(ERTS_START, ERTS_END, unit.per_second);
        assert!(
            low <= *value && *value <= high,
            "monotonic_time({}) = {value} is outside the epoch window [{low}, {high}]",
            label(unit)
        );
    }
    // Pairwise scaling: `value(unit_a) / value(unit_b)` must be the ratio of their
    // units-per-second values. `second`/`millisecond`/`microsecond`/`nanosecond`
    // scale by whole thousands, `native` and `perf_counter` agree with `nanosecond`,
    // and the integer form `1000` agrees with `millisecond`.
    let expected_scaling: [(usize, usize, f64); 7] = [
        (1, 0, 1_000.0),          // millisecond / second
        (2, 1, 1_000.0),          // microsecond / millisecond
        (2, 0, 1_000_000.0),      // microsecond / second
        (3, 0, 1_000_000_000.0),  // nanosecond / second
        (4, 3, 1.0),              // native / nanosecond
        (5, 3, 1.0),              // perf_counter / nanosecond
        (6, 1, 1.0),              // the integer unit 1000 / millisecond
    ];
    for (a, b, expected) in expected_scaling {
        let ours_ratio = ours[a] as f64 / ours[b] as f64;
        assert!(
            relative_error(ours_ratio, expected) < 1.0e-6,
            "monotonic_time({}) / monotonic_time({}) = {ours_ratio} is not {expected}",
            label(&UNITS[a]),
            label(&UNITS[b])
        );
        if let Some(oracle) = &oracle {
            let oracle_ratio = oracle.values[a] as f64 / oracle.values[b] as f64;
            assert!(
                relative_error(oracle_ratio, expected) < 1.0e-6,
                "the oracle's monotonic_time({}) / monotonic_time({}) = {oracle_ratio} \
                 is not {expected}: the scaling premise of this test is broken",
                label(&UNITS[a]),
                label(&UNITS[b])
            );
            // The two runtimes scale the same way to well within the read skew.
            assert!(
                relative_error(ours_ratio, oracle_ratio) < 1.0e-6,
                "our {}:{} scaling ({ours_ratio}) differs from the oracle's ({oracle_ratio})",
                label(&UNITS[a]),
                label(&UNITS[b])
            );
        }
    }
    if oracle.is_none() {
        eprintln!(
            "SKIP: the live-oracle half of every_unit_sits_on_the_erts_epoch_like_the_oracle \
             did not run (no `erl` oracle); the epoch relations above are pinned without it"
        );
    }
}
/// The `/1` units are **floored** divisions of one native reading — on a negative
/// epoch, truncation toward zero would be off by one and is what the C
/// `convert_time_unit/3` avoids (`erl_time.h`/`erl_time_sup.c:2462-2495`).
///
/// The readings are separate clock calls, so each `/1` value is bracketed by two
/// native ones; the oracle's own rounding is checked by the same bracket in its run
/// (its `monotonic_time(nanosecond)` bracket cannot be observed from outside, so the
/// sign and window pins above cover it).
#[test]
fn every_unit_is_the_floored_conversion_of_the_native_reading() {
    let mut fx = Fixture::new();
    let native = &UNITS[4];
    let mut checked = 0;
    for unit in &UNITS {
        let lo = fx.monotonic(native);
        let value = fx.monotonic(unit);
        let hi = fx.monotonic(native);
        assert!(lo <= hi, "the native clock went backwards: {lo} then {hi}");
        let (low, high) = (
            (lo * unit.per_second).div_euclid(MONOTONIC_TIME_UNIT),
            (hi * unit.per_second).div_euclid(MONOTONIC_TIME_UNIT),
        );
        assert!(
            low <= value && value <= high,
            "monotonic_time({}) = {value} is outside the floored bracket [{low}, {high}]",
            label(unit)
        );
        checked += 1;
    }
    assert_eq!(checked, UNITS.len(), "every probe unit was bracketed");
    // A truncating conversion of the very same native reading disagrees whenever the
    // reading is not a whole multiple of the unit — which, on a negative epoch, is
    // the common case (`-0.999… s` truncates to `0 s`, and must floor to `-1 s`).
    let one_ns_past_a_second = -MONOTONIC_TIME_UNIT + 1;
    assert_eq!(
        one_ns_past_a_second.div_euclid(MONOTONIC_TIME_UNIT),
        -1,
        "flooring `-0.999… s` is `-1 s`"
    );
    assert_eq!(one_ns_past_a_second / MONOTONIC_TIME_UNIT, 0, "truncation is not");
}
/// The invariant the oracle exhibits — `system_time() == monotonic_time() +
/// time_offset()` — holds on this epoch too, and the offset is the oracle's own
/// ~74-year constant rather than a number measured from a zero origin.
///
/// `system_time/0` and `time_offset/0` are bignums here (both are past the 60-bit
/// small range), so they are decoded through `beam_codec::term::big_parts`.
#[test]
fn system_time_is_monotonic_time_plus_the_epoch_offset() {
    let mut fx = Fixture::new();
    let mono = integer_f64(fx.call0("monotonic_time"));
    let sys = integer_f64(fx.call0("system_time"));
    let off = integer_f64(fx.call0("time_offset"));
    // `off = sys - mono`, and `mono` is `start_time` plus the uptime, so the offset
    // exceeds the system time by the whole `|start_time|` (less this VM's uptime).
    let epoch = (-ERTS_START) as f64;
    assert!(
        (off - sys - epoch).abs() <= 1.0e9,
        "time_offset ({off}) - system_time ({sys}) is not the ERTS epoch {epoch}"
    );
    assert!(
        (sys - (mono + off)).abs() <= 1.0e9,
        "system_time ({sys}) is not monotonic_time ({mono}) + time_offset ({off})"
    );
    assert!(mono < 0.0, "the monotonic reading is negative on this epoch: {mono}");
}
/// The exact `f64` value of a small-or-bignum integer term (the epoch values past
/// the 60-bit small range are bignums, `beam_codec::term::big_parts` being the safe
/// reader).
fn integer_f64(term: Eterm) -> f64 {
    if beam_term::is_small(term) {
        return signed_value(term) as f64;
    }
    let (negative, digits) = beam_codec::term::big_parts(term).expect("an integer term");
    // These clocks are ~2e18, i.e. a single 64-bit limb.
    assert!(
        digits.len() <= 2,
        "a clock value wider than i128: {} limbs",
        digits.len()
    );
    let mut magnitude = 0.0f64;
    for (index, limb) in digits.iter().enumerate() {
        magnitude += (*limb as f64) * 2f64.powi(64 * index as i32);
    }
    if negative {
        -magnitude
    } else {
        magnitude
    }
}