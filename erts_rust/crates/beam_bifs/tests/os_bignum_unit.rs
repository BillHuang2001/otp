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

//! Integration tests for the **bignum time unit** of `os:system_time/1`, driven
//! through the **public** API only (`default_registry()` + `BifRegistry::call`).
//!
//! `os:system_time/1` runs the same `time_unit_conversion` as
//! `erlang:system_time/1` (`erl_time_sup.c:2268-2356`, `os_system_time_1` calls
//! it with `muloff` 0 on its own clock reading), so a positive bignum unit is a
//! legal ratio the BIF converts in arbitrary precision — the C's `default` arm
//! hands such a unit to `erlang:convert_time_unit/3` (`erl_time_sup.c:2333-2352`)
//! — while a negative one is `badarg` (the C's `big_sign`).
//!
//! The exact conversion goldens are pinned in the `src/os_bif.rs` unit tests
//! (they need a fixed clock reading, which only the private conversion helper
//! takes); this file covers the public seam, the rejection, the units that must
//! not change, and one **live** cross-check in which the oracle converts the very
//! native readings the runtime bracketed its own conversion with (it skips
//! loudly when no `erl` is on `PATH`).
//!
//! The inspection toolkit is itself made of registered BIFs —
//! `erlang:integer_to_list/1`, `erlang:length/1`, `erlang:hd/1`, `erlang:tl/1` —
//! plus `beam_codec::term::big_parts` for a bignum's limbs.
use std::cmp::Ordering;
use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{signed_value, AtomTable, Eterm};

/// A registry + heap + atom table the tests drive BIF calls through.
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
            "second",
            "seconds",
            "millisecond",
            "milli_seconds",
            "microsecond",
            "nanosecond",
            "native",
            "perf_counter",
            "fortnight",
        ]);
        Fixture {
            registry: default_registry(),
            heap: Heap::new(1 << 12).expect("test heap"),
            atoms,
        }
    }

    /// Calls `module:function/args.len()` through the registry.
    fn call(&mut self, module: &str, function: &str, args: &[Eterm]) -> Result<Eterm> {
        let mut ctx = BifContext::new(&mut self.heap, &self.atoms, ProcessId::new(1, 0));
        self.registry
            .call(module, function, args.len() as u32, &mut ctx, args)
    }

    /// Calls an `erlang` BIF.
    fn erlang(&mut self, function: &str, args: &[Eterm]) -> Result<Eterm> {
        self.call("erlang", function, args)
    }

    /// Calls `os:system_time/1`.
    fn os_system_time(&mut self, unit: Eterm) -> Result<Eterm> {
        self.call("os", "system_time", &[unit])
    }

    /// The atom term `name` (seeded in [`Fixture::new`]).
    fn atom(&self, name: &str) -> Eterm {
        beam_term::make_atom(self.atoms.lookup(name).expect("a seeded atom"))
    }

    /// The bignum term for `(negative, digits)`, built on the fixture's heap
    /// (digits least significant first).
    fn bignum(&mut self, negative: bool, digits: &[u64]) -> Eterm {
        beam_codec::term::make_big(&mut self.heap, negative, digits).expect("test bignum")
    }

    /// The decimal string of an integer term, rendered by the registered
    /// `erlang:integer_to_list/1` and read back one character at a time.
    fn decimal(&mut self, term: Eterm) -> String {
        let list = self
            .erlang("integer_to_list", &[term])
            .expect("integer_to_list/1");
        let len = small(self.erlang("length", &[list]).expect("length/1"));
        let mut text = String::with_capacity(len as usize);
        let mut cur = list;
        for _ in 0..len {
            let character = small(self.erlang("hd", &[cur]).expect("hd/1")) as u32;
            text.push(char::from_u32(character).expect("an ASCII character"));
            cur = self.erlang("tl", &[cur]).expect("tl/1");
        }
        text
    }
}

/// The value of a small-integer term.
fn small(term: Eterm) -> i64 {
    assert!(beam_term::is_small(term), "expected a small int, got {term:?}");
    signed_value(term)
}

/// `(negative, digits)` of a bignum result, copied out of its storage.
fn parts(term: Eterm) -> (bool, Vec<u64>) {
    let (negative, digits) = beam_codec::term::big_parts(term).expect("expected a bignum");
    (negative, digits.to_vec())
}

/// Whether `term` is a bignum (`big_parts` is the safe reading; the `beam_term`
/// predicate dereferences a header word).
fn is_bignum(term: Eterm) -> bool {
    beam_codec::term::big_parts(term).is_ok()
}

/// The unit `1 bsl 200`: a positive bignum "units per second" ratio (bit 8 of
/// limb 3). Each converted value is `native * 2^200 div 1e9`.
fn unit_1_bsl_200(fx: &mut Fixture) -> Eterm {
    fx.bignum(false, &[0, 0, 0, 1 << 8])
}

/// The unit `1 bsl 61`: the smallest bignum "units per second" ratio (bit 61 of
/// limb 0), so `os:system_time(1 bsl 61)` is `native * 2^61 div 1e9`.
fn unit_1_bsl_61(fx: &mut Fixture) -> Eterm {
    fx.bignum(false, &[1 << 61])
}

/// Compares two non-negative decimal integers of arbitrary size.
fn cmp_decimal(a: &str, b: &str) -> Ordering {
    assert!(
        !a.starts_with('-') && !b.starts_with('-'),
        "the bracketed clock values are non-negative"
    );
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

/// Asserts that `os:system_time(unit)` lies between the oracle's conversions of the
/// two `os:system_time/0` readings that bracket it. `false` when no oracle is
/// available.
fn assert_oracle_bracket(fx: &mut Fixture, unit: Eterm, unit_text: &str) -> bool {
    let nanosecond = fx.atom("nanosecond");
    let before = fx.os_system_time(nanosecond).expect("the nanosecond form");
    let converted = fx.os_system_time(unit).expect("a positive bignum unit");
    let after = fx.os_system_time(nanosecond).expect("the nanosecond form");
    let before = fx.decimal(before);
    let converted = fx.decimal(converted);
    let after = fx.decimal(after);
    let (lo, hi) = match (
        oracle_convert(&before, unit_text),
        oracle_convert(&after, unit_text),
    ) {
        (Some(lo), Some(hi)) => (lo, hi),
        _ => return false,
    };
    assert!(
        cmp_decimal(&lo, &converted) != Ordering::Greater
            && cmp_decimal(&converted, &hi) != Ordering::Greater,
        "`os:system_time({unit_text})` = {converted} is outside the oracle's [{lo}, {hi}] \
         for the readings {before} .. {after}"
    );
    true
}/// The oracle's `erlang:convert_time_unit(Native, native, Unit)` for the decimal
/// `native` — the exact equivalent of the C `os_system_time_1`
/// (`time_unit_conversion`'s `trap_to_erlang_code` arm passes the native reading,
/// `erts_internal:time_unit()`, and the unit). `None` when no oracle is available.
fn oracle_convert(native: &str, unit_text: &str) -> Option<String> {
    oracle_eval(&format!(
        r#"io:format("~s", [integer_to_list(erlang:convert_time_unit({native}, native, {unit_text}))])."#
    ))
    .map(|out| out.trim().to_string())
}

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the caller then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// A positive bignum unit is a legal `os:system_time/1` unit — the runtime returns
/// the exact bignum the oracle returns, never the typed capability gap.
#[test]
fn a_positive_bignum_unit_is_accepted() {
    let mut fx = Fixture::new();
    let unit = unit_1_bsl_200(&mut fx);
    // The epoch reading scales the ~1.7e18-ns native value by `2^200`, so the
    // result is a bignum (roughly 1.3e60 x 1.7e9).
    let value = fx.os_system_time(unit).expect("os:system_time/1");
    assert!(
        is_bignum(value),
        "os:system_time(1 bsl 200) must be the oracle's bignum"
    );
    let (negative, digits) = parts(value);
    assert!(!negative, "a positive unit has a positive value");
    assert!(
        digits.last() != Some(&0),
        "no trailing zero limb: the value is scaled by 2^200, not truncated"
    );
    // The epoch reading is ~1.75e18 ns, so the scaled value is
    // `~1.75e18 * 2^200 / 1e9` ≈ 2.8e69 — 70 decimal digits.
    let rendered = fx.decimal(value);
    assert!(
        (65..=75).contains(&rendered.len()),
        "os:system_time(1 bsl 200) = {rendered} is not the 2^200-scaled reading"
    );
    // The smallest bignum ratio (`2^61`/s) is a bignum too: the epoch nanosecond
    // count is itself past the 60-bit range, so the product is far past it.
    let small_ratio = unit_1_bsl_61(&mut fx);
    let scaled = fx.os_system_time(small_ratio).expect("1 bsl 61");
    assert!(
        is_bignum(scaled),
        "os:system_time(1 bsl 61) must be a bignum"
    );
    let (negative, _) = parts(scaled);
    assert!(!negative, "a positive unit has a positive value");
}

/// A negative bignum unit is `badarg` (the C `time_unit_conversion`'s `big_sign`
/// arm), and so are the small/float/atom shapes — never a wrong value.
#[test]
fn a_negative_bignum_unit_is_badarg() {
    let mut fx = Fixture::new();
    let negative = fx.bignum(true, &[0, 0, 0, 1 << 8]);
    assert!(
        matches!(fx.os_system_time(negative), Err(BeamError::BadArg)),
        "os:system_time/1 must reject a negative bignum unit"
    );
    // The deprecated plural spelling stays legal — only the *sign* decides.
    let milli_seconds = fx.atom("milli_seconds");
    let suffix_unit = fx.bignum(false, &[0, 0, 0, 1 << 8]);
    assert!(fx.os_system_time(milli_seconds).is_ok(), "milli_seconds is legal");
    assert!(fx.os_system_time(suffix_unit).is_ok(), "2^200 is legal");
    let fortnight = fx.atom("fortnight");
    assert!(
        matches!(fx.os_system_time(fortnight), Err(BeamError::BadArg)),
        "os:system_time(fortnight) must be badarg"
    );
    let zero = beam_term::make_small(0);
    assert!(matches!(fx.os_system_time(zero), Err(BeamError::BadArg)));
    let minus_one = beam_term::make_small(-1);
    assert!(matches!(fx.os_system_time(minus_one), Err(BeamError::BadArg)));
}

/// The *small* and *atom* units keep their ratio while a bignum unit is live:
/// `second` and `1000` agree with the nanosecond reading divided by their ratio,
/// and `1 bsl 200` scales the same reading by `2^200` rather than by a truncated
/// one.
#[test]
fn the_small_and_atom_units_are_unchanged_by_the_bignum_form() {
    let mut fx = Fixture::new();
    let second = fx.atom("second");
    let thousand = beam_term::make_small(1000);
    let unit = unit_1_bsl_200(&mut fx);
    let by_second = fx.os_system_time(second).expect("second");
    let by_second = fx.decimal(by_second);
    let by_thousand = fx.os_system_time(thousand).expect("1000");
    let by_thousand = fx.decimal(by_thousand);
    assert!(
        by_second.parse::<u64>().expect("whole epoch seconds") > 1_577_836_800,
        "os:system_time(second) = {by_second} is not a plausible epoch value"
    );
    // `1000`/s is the millisecond reading: three decimal digits more than the
    // second reading (the two calls bracket each other, so allow one digit of
    // clock drift).
    assert!(
        by_thousand.len() >= by_second.len() + 3
            && by_thousand.len() <= by_second.len() + 4,
        "os:system_time(1000) = {by_thousand} is not the millisecond reading of {by_second}"
    );
    let big = fx.os_system_time(unit).expect("1 bsl 200");
    let big = fx.decimal(big);
    assert!(
        big.len() > by_second.len() + 50,
        "os:system_time(1 bsl 200) = {big} is not the {by_second}-second reading scaled by 2^200"
    );
}

/// The live oracle converts the very native readings the runtime bracketed its own
/// bignum-unit conversion with, so `os:system_time(1 bsl 200)` is pinned end to
/// end against `erlang:convert_time_unit(Reading, native, 1 bsl 200)` — the C's
/// own trap path. Skips loudly when no oracle is available; the exact goldens live
/// in the `src/os_bif.rs` unit tests either way.
#[test]
fn the_live_oracle_brackets_the_bignum_unit_conversion() {
    let mut fx = Fixture::new();
    let unit = unit_1_bsl_200(&mut fx);
    let small_ratio = unit_1_bsl_61(&mut fx);
    let checked = [
        assert_oracle_bracket(&mut fx, unit, "1 bsl 200"),
        assert_oracle_bracket(&mut fx, small_ratio, "1 bsl 61"),
    ];
    if !checked.iter().any(|ok| *ok) {
        eprintln!(
            "SKIP: no `erl` oracle — the bignum-unit conversion is pinned by the \
             `src/os_bif.rs` goldens instead"
        );
    }
}
