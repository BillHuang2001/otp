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
//! Integration tests for the **bignum time unit** of the `erlang` time BIFs
//! (`monotonic_time/1`, `system_time/1`, `time_offset/1`), driven through the
//! **public** API only (`default_registry()` + `BifRegistry::call`).
//!
//! A positive bignum unit is a legal `erlang:convert_time_unit/3` ratio, so the
//! `/1` BIFs convert it in arbitrary precision (the C `time_unit_conversion`
//! `default` arm traps to `erlang:convert_time_unit/3`, `erl_time_sup.c:2338-2352`),
//! while a negative one is `badarg` (the C's `big_sign`). The exact conversion
//! goldens are pinned in the `src/time.rs` unit tests; this file covers the public
//! seam, the rejection, and one **live** cross-check in which the oracle converts
//! the very native readings the runtime bracketed its own conversion with (it
//! skips loudly when no `erl` is on `PATH`).
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
            "millisecond",
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

/// Compares two decimal integers of arbitrary size and either sign.
fn cmp_decimal(a: &str, b: &str) -> Ordering {
    // Split the sign off; a non-negative value sorts after a negative one, and
    // within a sign the magnitude comparison is the digit-count-then-lexicographic
    // rule (with the order reversed for two negatives).
    let (a_neg, a_mag) = match a.strip_prefix('-') {
        Some(mag) => (true, mag),
        None => (false, a),
    };
    let (b_neg, b_mag) = match b.strip_prefix('-') {
        Some(mag) => (true, mag),
        None => (false, b),
    };
    if a_neg != b_neg {
        return if a_neg { Ordering::Less } else { Ordering::Greater };
    }
    let magnitude = a_mag
        .len()
        .cmp(&b_mag.len())
        .then_with(|| a_mag.cmp(b_mag));
    if a_neg {
        magnitude.reverse()
    } else {
        magnitude
    }
}

/// Asserts that `function/1` with `unit` — `monotonic_time(unit)`,
/// `system_time(unit)`, … — lies between the oracle's conversions of the two
/// `function/0` readings that bracket it. `false` when no oracle is available.
fn assert_oracle_bracket(
    fx: &mut Fixture,
    function: &str,
    unit: Eterm,
    unit_text: &str,
) -> bool {
    let before = fx.erlang(function, &[]).expect("the /0 form");
    let converted = fx.erlang(function, &[unit]).expect("a positive bignum unit");
    let after = fx.erlang(function, &[]).expect("the /0 form");

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
        "`{function}({unit_text})` = {converted} is outside the oracle's [{lo}, {hi}] \
         for the readings {before} .. {after}"
    );
    true
}

/// The oracle's `erlang:convert_time_unit(Native, nanosecond, Unit)` for the
/// decimal `native`, or `None` when no oracle is available.
fn oracle_convert(native: &str, unit_text: &str) -> Option<String> {
    oracle_eval(&format!(
        r#"io:format("~s", [integer_to_list(erlang:convert_time_unit({native}, nanosecond, {unit_text}))])."#
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

/// A positive bignum unit is a legal unit for all three `/1` BIFs — the runtime
/// returns the exact bignum the oracle returns, never the typed capability gap.
#[test]
fn a_positive_bignum_unit_is_accepted_by_every_unit_bif() {
    let mut fx = Fixture::new();
    let unit = unit_1_bsl_200(&mut fx);

    // `monotonic_time(1 bsl 200)` scales the **ERTS-epoch** native reading — a large
    // negative integer — by `2^200`, so it is a *negative* bignum (ORACLE:
    // `-926336713075587891010068494398143295895281295409423312654023500425432`,
    // and `abs(erlang:monotonic_time(1 bsl 200)) < 2^60` is `false`).
    let mono = fx.erlang("monotonic_time", &[unit]).expect("monotonic_time/1");
    let mono_bignum = is_bignum(mono);
    let mono = fx.decimal(mono);
    assert!(
        mono_bignum,
        "monotonic_time(1 bsl 200) must be a bignum: {mono}"
    );
    assert!(
        mono.starts_with('-'),
        "monotonic_time(1 bsl 200) must be negative on the ERTS epoch: {mono}"
    );
    assert!(
        mono.len() > 60,
        "monotonic_time(1 bsl 200) = {mono} is not the 2^200-scaled reading"
    );

    // The epoch reading and the offset scale the ~1.7e18-ns native value by
    // `2^200`, so both are bignums.
    let sys = fx.erlang("system_time", &[unit]).expect("system_time/1");
    assert!(
        is_bignum(sys),
        "system_time(1 bsl 200) must be the oracle's bignum"
    );
    let (negative, _) = parts(sys);
    assert!(!negative, "system_time(1 bsl 200) must not be negative");

    let offset = fx.erlang("time_offset", &[unit]).expect("time_offset/1");
    assert!(
        is_bignum(offset),
        "time_offset(1 bsl 200) must be the oracle's bignum"
    );
    let (negative, _) = parts(offset);
    assert!(!negative, "time_offset(1 bsl 200) must not be negative");
}

/// A negative bignum unit is `badarg` (the C `time_unit_conversion`'s
/// `big_sign` arm), for all three `/1` BIFs.
#[test]
fn a_negative_bignum_unit_is_badarg_for_every_unit_bif() {
    let mut fx = Fixture::new();
    let unit = fx.bignum(true, &[0, 0, 0, 1 << 8]);
    for function in ["monotonic_time", "system_time", "time_offset"] {
        assert!(
            matches!(fx.erlang(function, &[unit]), Err(BeamError::BadArg)),
            "{function}/1 must reject a negative bignum unit"
        );
    }
}

/// The grid of the *small* and *atom* units keeps its ratio while a bignum unit is
/// live: `second` and `1` agree, `1 bsl 58` promotes, and `1 bsl 200` scales the
/// same reading by `2^200` rather than by a truncated one.
#[test]
fn the_small_and_atom_units_are_unchanged_by_the_bignum_form() {
    let mut fx = Fixture::new();
    let second = fx.atoms.lookup("second").expect("seeded");
    let second = beam_term::make_atom(second);
    let unit = unit_1_bsl_200(&mut fx);

    let by_second = fx.erlang("system_time", &[second]).expect("second");
    let by_second = fx.decimal(by_second);

    assert!(
        by_second.parse::<u64>().expect("whole epoch seconds") > 1_577_836_800,
        "system_time(second) = {by_second} is not a plausible epoch value"
    );

    // The bignum unit's result is the same reading scaled by `2^200` (an exact
    // multiple of it, so its leading digits agree).
    let big = fx.erlang("system_time", &[unit]).expect("1 bsl 200");
    let big = fx.decimal(big);
    assert!(
        big.len() > by_second.len() + 50,
        "system_time(1 bsl 200) = {big} is not the {by_second}-second reading scaled by 2^200"
    );
}

/// The live oracle converts the very native readings the runtime bracketed its own
/// bignum-unit conversion with, so `monotonic_time(1 bsl 200)` and
/// `system_time(1 bsl 200)` are pinned end to end (a small and a 2-limb native
/// operand). Skips loudly when no oracle is available; the exact goldens live in
/// the `src/time.rs` unit tests either way.
#[test]
fn the_live_oracle_brackets_the_bignum_unit_conversion() {
    let mut fx = Fixture::new();
    let unit = unit_1_bsl_200(&mut fx);

    let checked = [
        assert_oracle_bracket(&mut fx, "monotonic_time", unit, "1 bsl 200"),
        assert_oracle_bracket(&mut fx, "system_time", unit, "1 bsl 200"),
    ];
    if !checked.iter().any(|ok| *ok) {
        eprintln!(
            "SKIP: no `erl` oracle — the bignum-unit conversion is pinned by the \
             `src/time.rs` goldens instead"
        );
    }
}
