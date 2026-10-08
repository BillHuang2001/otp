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

//! Integration tests for the *pure* `bif.tab` BIFs added to the registry:
//! `maps:take/2`, `maps:from_keys/2`, `unicode:bin_is_7bit/1`,
//! `io:printable_range/0`, `string:list_to_float/1` and the `math:*` sweep.
//!
//! Every expectation is pinned against the system oracle
//! (`erl -noshell -eval '…'`, OTP 29 / erts-17.1) and asserted **structurally**
//! with `beam_term::compare::eq`: a result term built on the test heap is never
//! word-identical to an independently built expected term, so raw `Eterm`
//! equality would be a false negative for lists, tuples and maps.
//!
//! The tests drive only the public API — [`BifRegistry`]/[`BifContext`] plus the
//! declared dependency crates (`beam_alloc` for the heap, `beam_codec::term` for
//! the binary/bitstring/float constructors and readers).

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{float_value, make_binary, make_bitstring};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{compare, AtomTable, Eterm, NIL};

/// The atoms every case below names. Only interned names resolve without an
/// `AtomInterner` seam, so the table is seeded up front.
const ATOMS: [&str; 10] = [
    "true",
    "false",
    "error",
    "no_float",
    "not_a_list",
    "latin1",
    "a",
    "b",
    "x",
    "v",
];

fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(ATOMS);
    atoms
}

/// Runs `f` with the real [`default_registry`] over a fresh heap and the seeded
/// atom table — no optional seams installed (these BIFs need none).
fn run<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>) -> R) -> R {
    let reg = default_registry();
    let mut heap = Heap::new(1 << 14).expect("test heap");
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx)
}

/// Calls `Module:Function(Args)` through the real registry and unwraps the result.
///
/// `Args` must be materialized by the caller: passing `ctx` as an explicit
/// argument while an `Args` element also borrows `ctx` would be a double mutable
/// borrow (the same trap the `#[cfg(test)]` suites avoid).
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    function: &str,
    args: &[Eterm],
) -> Eterm {
    reg.call(module, function, args.len() as u32, ctx, args)
        .unwrap_or_else(|e| panic!("{module}:{function}/{} failed: {e:?}", args.len()))
}

/// `maps:from_list(Pairs)` — maps are built through the registered BIF, because
/// [`BifContext`] exposes no map constructor.
fn map_of(reg: &BifRegistry, ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    let tuples: Vec<Eterm> = pairs
        .iter()
        .map(|&(k, v)| ctx.tuple(&[k, v]).expect("a 2-tuple fits"))
        .collect();
    let list = ctx.list(&tuples).expect("a short list fits");
    call(reg, ctx, "maps", "from_list", &[list])
}

/// A structural (`=:=`) comparison, the only sound term comparison across heaps.
fn same(ctx: &BifContext<'_>, a: Eterm, b: Eterm) -> bool {
    compare::eq(a, b, ctx.atoms())
}

/// The character list of `text` (each byte a small integer).
fn chars(ctx: &mut BifContext<'_>, text: &str) -> Eterm {
    let elems: Vec<Eterm> = text
        .bytes()
        .map(|b| ctx.int(i64::from(b)).expect("a byte fits"))
        .collect();
    ctx.list(&elems).expect("a short list fits")
}

/// The `f64` behind a float result (`math:*` always returns a float).
fn value(term: Eterm) -> f64 {
    float_value(term).expect("a float result")
}

// ---------------------------------------------------------------------------
// maps:take/2
// ---------------------------------------------------------------------------

#[test]
fn maps_take_returns_the_value_and_the_rest() {
    run(|reg, ctx| {
        // `maps:take(a, #{a => 1})` ⇒ `{1, #{}}`.
        let a = ctx.atom("a").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let map = map_of(reg, ctx, &[(a, one)]);
        let taken = call(reg, ctx, "maps", "take", &[a, map]);
        let empty = map_of(reg, ctx, &[]);
        let expected = ctx.tuple(&[one, empty]).expect("fits");
        assert!(same(ctx, taken, expected), "maps:take(a, #{{a => 1}})");
        // The returned map holds no pairs, so `maps:size/1` sees `0`.
        let size = call(reg, ctx, "maps", "size", &[empty]);
        assert_eq!(size, ctx.int(0).expect("fits"));
    });
}

#[test]
fn maps_take_missing_key_is_the_atom_error() {
    run(|reg, ctx| {
        // `maps:take(b, #{a => 1})` ⇒ `error`.
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let map = map_of(reg, ctx, &[(a, one)]);
        let missing = call(reg, ctx, "maps", "take", &[b, map]);
        let error = ctx.atom("error").expect("seeded");
        assert!(same(ctx, missing, error), "a missing key is `error`");
    });
}

#[test]
fn maps_take_uses_exact_key_identity() {
    run(|reg, ctx| {
        let x = ctx.atom("x").expect("seeded");
        let one = ctx.int(1).expect("fits");
        let one_f = ctx.make_float(1.0).expect("fits");
        let error = ctx.atom("error").expect("seeded");

        // `maps:take(1.0, #{1 => x})` ⇒ `error` — a float key is not `=:=` an int key.
        let int_keyed = map_of(reg, ctx, &[(one, x)]);
        let got = call(reg, ctx, "maps", "take", &[one_f, int_keyed]);
        assert!(same(ctx, got, error), "take(1.0, #{{1 => x}})");

        // …and the mirror image: `maps:take(1, #{1.0 => x})` ⇒ `error`.
        let float_keyed = map_of(reg, ctx, &[(one_f, x)]);
        let got = call(reg, ctx, "maps", "take", &[one, float_keyed]);
        assert!(same(ctx, got, error), "take(1, #{{1.0 => x}})");
    });
}

// ---------------------------------------------------------------------------
// maps:from_keys/2
// ---------------------------------------------------------------------------

#[test]
fn maps_from_keys_builds_a_map_with_one_value() {
    run(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let one = ctx.int(1).expect("fits");

        // `maps:from_keys([a, b], 1)` prints as `#{b => 1, a => 1}` in the oracle;
        // map equality is order-independent, so the structural check is enough.
        let keys = ctx.list(&[a, b]).expect("fits");
        let got = call(reg, ctx, "maps", "from_keys", &[keys, one]);
        let expected = map_of(reg, ctx, &[(a, one), (b, one)]);
        assert!(same(ctx, got, expected), "maps:from_keys([a, b], 1)");

        // `maps:from_keys([], 1)` ⇒ `#{}`.
        let empty = call(reg, ctx, "maps", "from_keys", &[NIL, one]);
        let expected_empty = map_of(reg, ctx, &[]);
        assert!(same(ctx, empty, expected_empty), "maps:from_keys([], 1)");
        let size = call(reg, ctx, "maps", "size", &[empty]);
        assert_eq!(size, ctx.int(0).expect("fits"));
    });
}

#[test]
fn maps_from_keys_collapses_duplicates_and_keeps_exact_keys() {
    run(|reg, ctx| {
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let v = ctx.atom("v").expect("seeded");
        let one = ctx.int(1).expect("fits");

        // A repeated key collapses: `maps:from_keys([a, b, a], 1)` ⇒ `#{a => 1, b => 1}`.
        let keys = ctx.list(&[a, b, a]).expect("fits");
        let got = call(reg, ctx, "maps", "from_keys", &[keys, one]);
        let expected = map_of(reg, ctx, &[(a, one), (b, one)]);
        assert!(same(ctx, got, expected), "duplicate keys collapse");
        let size = call(reg, ctx, "maps", "size", &[got]);
        assert_eq!(size, ctx.int(2).expect("fits"));

        // Two keys distinct under `=:=` stay separate even though `1 == 1.0`:
        // `maps:from_keys([1, 1.0], v)` ⇒ `#{1 => v, 1.0 => v}`.
        let one_f = ctx.make_float(1.0).expect("fits");
        let keys = ctx.list(&[one, one_f]).expect("fits");
        let got = call(reg, ctx, "maps", "from_keys", &[keys, v]);
        let expected = map_of(reg, ctx, &[(one, v), (one_f, v)]);
        assert!(same(ctx, got, expected), "exact key identity");
        let size = call(reg, ctx, "maps", "size", &[got]);
        assert_eq!(size, ctx.int(2).expect("fits"));
    });
}

#[test]
fn maps_from_keys_rejects_a_non_list() {
    run(|reg, ctx| {
        let v = ctx.atom("v").expect("seeded");
        // `maps:from_keys(5, v)` ⇒ `error:badarg`.
        let five = ctx.int(5).expect("fits");
        assert!(matches!(
            reg.call("maps", "from_keys", 2, ctx, &[five, v]),
            Err(BeamError::BadArg)
        ));
        // An improper list is `badarg` too (`maps:from_keys([a|b], v)`).
        let a = ctx.atom("a").expect("seeded");
        let b = ctx.atom("b").expect("seeded");
        let improper = ctx.cons(a, b).expect("fits");
        assert!(matches!(
            reg.call("maps", "from_keys", 2, ctx, &[improper, v]),
            Err(BeamError::BadArg)
        ));
    });
}

// ---------------------------------------------------------------------------
// unicode:bin_is_7bit/1
// ---------------------------------------------------------------------------

#[test]
fn unicode_bin_is_7bit_matches_the_oracle() {
    run(|reg, ctx| {
        let truthy = ctx.atom("true").expect("seeded");
        let falsy = ctx.atom("false").expect("seeded");

        // A byte-aligned binary of bytes < 0x80 is 7-bit: `<<1, 2, 127>>` ⇒ `true`.
        let bin = make_binary(ctx.heap(), &[1, 2, 127]).expect("the binary fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[bin]);
        assert!(same(ctx, got, truthy));

        // `<<1, 128>>` ⇒ `false` (128 is not 7-bit).
        let bin = make_binary(ctx.heap(), &[1, 128]).expect("the binary fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[bin]);
        assert!(same(ctx, got, falsy));

        // The empty binary is trivially 7-bit.
        let bin = make_binary(ctx.heap(), &[]).expect("the binary fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[bin]);
        assert!(same(ctx, got, truthy));

        // A non-byte-aligned bitstring is `false`: `<<1:1>>` and `<<0,1:1>>`.
        let bits = make_bitstring(ctx.heap(), &[0x80], 1).expect("the bitstring fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[bits]);
        assert!(same(ctx, got, falsy), "<<1:1>>");
        let bits = make_bitstring(ctx.heap(), &[0x00, 0x80], 9).expect("the bitstring fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[bits]);
        assert!(same(ctx, got, falsy), "<<0,1:1>> (9 bits)");

        // Every non-binary term is `false` — the argument is never validated, so
        // no case below may raise.
        let list = ctx
            .list(&[ctx.int(1).expect("fits"), ctx.int(2).expect("fits")])
            .expect("fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[list]);
        assert!(same(ctx, got, falsy), "[1, 2]");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[NIL]);
        assert!(same(ctx, got, falsy), "[]");
        let five = ctx.int(5).expect("fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[five]);
        assert!(same(ctx, got, falsy), "5");
        let float = ctx.make_float(1.0).expect("fits");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[float]);
        assert!(same(ctx, got, falsy), "1.0");
        let a = ctx.atom("a").expect("seeded");
        let got = call(reg, ctx, "unicode", "bin_is_7bit", &[a]);
        assert!(same(ctx, got, falsy), "a");
    });
}

// ---------------------------------------------------------------------------
// io:printable_range/0
// ---------------------------------------------------------------------------

#[test]
fn io_printable_range_is_latin1_without_the_pc_flag() {
    run(|reg, ctx| {
        // `erl -noshell -eval 'io:printable_range().'` ⇒ `latin1` (the default; only
        // `+pc unicode` upgrades it, and Milestone-1 has no flag plumbing).
        let got = call(reg, ctx, "io", "printable_range", &[]);
        let latin1 = ctx.atom("latin1").expect("seeded");
        assert!(same(ctx, got, latin1));
    });
}

// ---------------------------------------------------------------------------
// string:list_to_float/1
// ---------------------------------------------------------------------------

#[test]
fn string_list_to_float_scans_a_prefix_and_returns_the_rest() {
    run(|reg, ctx| {
        // `string:list_to_float("1.5")` ⇒ `{1.5, []}`.
        let arg = chars(ctx, "1.5");
        let got = call(reg, ctx, "string", "list_to_float", &[arg]);
        let one_point_five = ctx.make_float(1.5).expect("fits");
        let expected = ctx.tuple(&[one_point_five, NIL]).expect("fits");
        assert!(same(ctx, got, expected), "1.5");

        // `string:list_to_float("2.3e")` ⇒ `{2.3, "e"}` — an exponent that never
        // starts is given back (`bif.c`'s `SAVE_E`/`LOAD_E` restore).
        let arg = chars(ctx, "2.3e");
        let got = call(reg, ctx, "string", "list_to_float", &[arg]);
        let two_point_three = ctx.make_float(2.3).expect("fits");
        let rest = chars(ctx, "e");
        let expected = ctx.tuple(&[two_point_three, rest]).expect("fits");
        assert!(same(ctx, got, expected), "2.3e");
    });
}

#[test]
fn string_list_to_float_error_shapes_match_the_oracle() {
    run(|reg, ctx| {
        let error = ctx.atom("error").expect("seeded");
        let no_float = ctx.atom("no_float").expect("seeded");
        let not_a_list = ctx.atom("not_a_list").expect("seeded");

        // No complete `<int><radix><fraction>` prefix ⇒ `{error, no_float}`
        // (`string:list_to_float("1")` and `string:list_to_float("abc")`).
        for text in ["1", "abc"] {
            let arg = chars(ctx, text);
            let got = call(reg, ctx, "string", "list_to_float", &[arg]);
            let expected = ctx.tuple(&[error, no_float]).expect("fits");
            assert!(same(ctx, got, expected), "{text} ⇒ {{error, no_float}}");
        }

        // A non-list argument ⇒ `{error, not_a_list}` (`string:list_to_float(5)`).
        let five = ctx.int(5).expect("fits");
        let got = call(reg, ctx, "string", "list_to_float", &[five]);
        let expected = ctx.tuple(&[error, not_a_list]).expect("fits");
        assert!(same(ctx, got, expected), "5 ⇒ {{error, not_a_list}}");
    });
}

// ---------------------------------------------------------------------------
// the math:* sweep
// ---------------------------------------------------------------------------

#[test]
fn math_bifs_match_the_oracle() {
    run(|reg, ctx| {
        // `math:floor(1)` ⇒ `1.0` — a float, not an integer, for an int argument.
        let one = ctx.int(1).expect("fits");
        assert_eq!(value(call(reg, ctx, "math", "floor", &[one])), 1.0);

        // `math:ceil(1.2)` ⇒ `2.0`.
        let one_point_two = ctx.make_float(1.2).expect("fits");
        assert_eq!(value(call(reg, ctx, "math", "ceil", &[one_point_two])), 2.0);

        // `math:fmod(5.0, 2.0)` ⇒ `1.0`.
        let five = ctx.make_float(5.0).expect("fits");
        let two = ctx.make_float(2.0).expect("fits");
        assert_eq!(value(call(reg, ctx, "math", "fmod", &[five, two])), 1.0);

        // `math:sqrt(4)` ⇒ `2.0` (the integer coerces to `4.0`).
        let four = ctx.int(4).expect("fits");
        assert_eq!(value(call(reg, ctx, "math", "sqrt", &[four])), 2.0);

        // `math:pow(2, 10)` ⇒ `1024.0`.
        let two_int = ctx.int(2).expect("fits");
        let ten = ctx.int(10).expect("fits");
        assert_eq!(value(call(reg, ctx, "math", "pow", &[two_int, ten])), 1024.0);

        // A negative fraction floors away from zero: `math:floor(-0.5)` ⇒ `-1.0`.
        let neg_half = ctx.make_float(-0.5).expect("fits");
        assert_eq!(value(call(reg, ctx, "math", "floor", &[neg_half])), -1.0);
    });
}

#[test]
fn math_domain_and_overflow_errors_match_the_oracle() {
    run(|reg, ctx| {
        let zero = ctx.int(0).expect("fits");
        let neg_one = ctx.int(-1).expect("fits");
        let neg_one_f = ctx.make_float(-1.0).expect("fits");
        let half = ctx.make_float(0.5).expect("fits");
        let exp_710 = ctx.int(710).expect("fits");

        // A `NaN`/`±inf` result is `badarith`, never `badarg` and never a wrong float.
        assert!(matches!(
            reg.call("math", "log", 1, ctx, &[zero]),
            Err(BeamError::BadArith)
        ));
        assert!(matches!(
            reg.call("math", "sqrt", 1, ctx, &[neg_one]),
            Err(BeamError::BadArith)
        ));
        assert!(matches!(
            reg.call("math", "pow", 2, ctx, &[neg_one_f, half]),
            Err(BeamError::BadArith)
        ));
        assert!(matches!(
            reg.call("math", "exp", 1, ctx, &[exp_710]),
            Err(BeamError::BadArith)
        ));

        // A non-numeric argument is `badarg`, not `badarith` (`math:sqrt(a)`).
        let a = ctx.atom("a").expect("seeded");
        assert!(matches!(
            reg.call("math", "sqrt", 1, ctx, &[a]),
            Err(BeamError::BadArg)
        ));
        // …and so is a list.
        let list = ctx.list(&[a]).expect("fits");
        assert!(matches!(
            reg.call("math", "exp", 1, ctx, &[list]),
            Err(BeamError::BadArg)
        ));
    });
}
