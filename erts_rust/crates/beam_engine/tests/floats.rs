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

//! Floating-point instruction-family integration test.
//!
//! Loads the committed oracle fixture `tests/fixtures/floats.beam` (produced by the
//! system `erlc`, OTP 29 / `erts-17.1` — the differential oracle) and runs its
//! functions through [`Machine::run_export`]. The fixture is *real compiler output*:
//! `beam_disasm` over it shows `fmove`/`fconv`/`fadd`/`fsub`/`fmul`/`fdiv`/`fnegate`
//! plus `fmove {float,K}` literals and four `fr` temporaries in `temps/4`.
//!
//! Every expected value below was read off the oracle
//! (`floats:add(1.5, 2.25) =:= 3.75`, `floats:divide(1.0, 0.0)` ⇒ `error:badarith`,
//! `floats:conv(a)` ⇒ `error:badarith`), so the test asserts the same observable
//! behavior the C runtime produces.
//!
//! The fixture needs **no BIF hook**: float arithmetic lowers to the dedicated
//! opcodes, never to a `gc_bif`.

use beam_common::BeamError;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_small, Atom, Eterm, NIL};

/// The committed fixture (produced by `erlc floats.erl`; see `fixtures/README.md`).
const FLOATS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/floats.beam");

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// The fixture, loaded once per call (a `Module` owns its literal heap).
fn floats_module() -> Module {
    load_file(FLOATS_BEAM).expect("load floats.beam")
}

/// A float argument term, owned by `ctx`'s arena.
fn float(ctx: &mut MinimalContext, value: f64) -> Eterm {
    terms::make_float(ctx, value).expect("the context arena allocates a float")
}

/// Run `name/arity` on the fixture and return the raw result term.
fn run(
    name: &str,
    arity: u32,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let module = floats_module();
    let mut machine = Machine::new();
    machine.run_export(&module, atom(&module, name), arity, args, ctx)
}

/// Run `name/2` over two float arguments and return the `f64` result.
fn run_float(name: &str, args: &[f64]) -> f64 {
    let mut ctx = MinimalContext::new();
    let terms: Vec<Eterm> = args.iter().map(|v| float(&mut ctx, *v)).collect();
    let result = run(name, terms.len() as u32, &terms, &mut ctx)
        .unwrap_or_else(|e| panic!("{name} failed: {e:?}"));
    terms::float_value(result).expect("the result is a float")
}

#[test]
fn module_exports_every_float_function() {
    let module = floats_module();
    assert_eq!(module.atom_name(module.name), Some("floats"));
    for (name, arity) in [
        ("add", 2),
        ("sub", 2),
        ("mul", 2),
        ("divide", 2),
        ("negate", 1),
        ("mixed", 1),
        ("temps", 4),
        ("conv", 1),
        ("scale", 2),
    ] {
        assert!(
            module.export(atom(&module, name), arity).is_some(),
            "{name}/{arity} must be exported"
        );
    }
}

#[test]
fn binary_opcodes_match_the_oracle() {
    // `fadd` / `fsub` / `fmul` / `fdiv` over the two `fmove`d X registers.
    assert_eq!(run_float("add", &[1.5, 2.25]), 3.75);
    assert_eq!(run_float("sub", &[1.5, 2.25]), -0.75);
    assert_eq!(run_float("mul", &[1.5, 2.0]), 3.0);
    assert_eq!(run_float("divide", &[3.0, 2.0]), 1.5);
}

#[test]
fn fnegate_matches_the_oracle() {
    assert_eq!(run_float("negate", &[2.5]), -2.5);
    assert_eq!(run_float("negate", &[-1.25]), 1.25);
}

#[test]
fn float_literals_flow_through_fmove_into_the_fr_file() {
    // `mixed(A) -> A * 2.0 + 1.0.` — the two `fmove {float,K}` literals.
    assert_eq!(run_float("mixed", &[3.0]), 7.0);
    assert_eq!(run_float("mixed", &[0.0]), 1.0);
    // `conv(I) -> I * 1.0.` also carries a float literal.
    assert_eq!(run_float("conv", &[4.0]), 4.0);
}

#[test]
fn four_float_registers_are_live_in_temps_4() {
    // `temps(A,B,C,D) -> A * B + C * D - A.` uses FR0..FR3.
    assert_eq!(run_float("temps", &[2.0, 3.0, 4.0, 5.0]), 24.0);
    assert_eq!(run_float("temps", &[1.0, 1.0, 1.0, 1.0]), 1.0);
}

#[test]
fn fconv_converts_small_integers_and_passes_floats_through() {
    let mut ctx = MinimalContext::new();
    // `conv(I) -> I * 1.0.` — `fconv` turns the small int into a float.
    let three = make_small(3);
    let result = run("conv", 1, &[three], &mut ctx).expect("conv(3)");
    assert_eq!(terms::float_value(result).unwrap(), 3.0);

    // A float argument is passed through `fconv` unchanged.
    let mut ctx = MinimalContext::new();
    let arg = float(&mut ctx, 3.5);
    let result = run("conv", 1, &[arg], &mut ctx).expect("conv(3.5)");
    assert_eq!(terms::float_value(result).unwrap(), 3.5);
}

#[test]
fn scale_converts_the_integer_operand_beside_a_float() {
    // `scale(I, F) -> I * F.` — a `fconv` next to an `fmove`.
    let mut ctx = MinimalContext::new();
    let i = make_small(3);
    let f = float(&mut ctx, 1.5);
    let result = run("scale", 2, &[i, f], &mut ctx).expect("scale(3, 1.5)");
    assert_eq!(terms::float_value(result).unwrap(), 4.5);
}

/// A bignum term with least-significant-first `u64` digits, owned by `ctx`'s arena.
///
/// Mirrors how the engine's own operand reader materialises a wide integer literal
/// (the frozen `[header, d0..dn]` layout, written by `beam_term::write_big`).
fn bignum(ctx: &mut MinimalContext, negative: bool, digits: &[u64]) -> Eterm {
    let hp = ctx
        .alloc_words(beam_term::big_size_words(digits.len()))
        .expect("arena words for the bignum");
    // SAFETY: `alloc_words` returned exactly `big_size_words(len)` writable words —
    // the block `write_big` requires and fully writes — and `digits` is non-empty
    // and normalised. The arena keeps them live for the whole run.
    unsafe { beam_term::write_big(hp, negative, digits) }
}

#[test]
fn fconv_of_a_representable_bignum_matches_the_oracle() {
    // A bignum source (outside the 60-bit immediate range) converts through the
    // faithful `big_to_double` port. Every expected value is the oracle's:
    // `erl -noshell -pa "$TMPDIR/fixgen" -eval
    // 'io:format("~.17e~n",[floats:conv(1 bsl 60)])'` ⇒ `1.1529215046068470e+18`,
    // `floats:conv(1000000000000000000)` ⇒ `1.0000000000000000e+18`.
    let cases: [(i64, f64); 3] = [
        // 2^60 — exact.
        (1i64 << 60, 2.0f64.powi(60)),
        // 10^18 = 2^18 * 5^18 — exact.
        (1_000_000_000_000_000_000, 1.0e18),
        // 2^60 + 1 rounds down to the nearest double, 2^60.
        ((1i64 << 60) + 1, 2.0f64.powi(60)),
    ];
    for (source_value, expected) in cases {
        let mut ctx = MinimalContext::new();
        let arg = terms::make_integer(&mut ctx, source_value).expect("the arena builds a bignum");
        assert!(!is_small(arg), "{source_value} must be a bignum");
        let result = run("conv", 1, &[arg], &mut ctx)
            .unwrap_or_else(|e| panic!("conv({source_value}) failed: {e:?}"));
        assert_eq!(
            terms::float_value(result).expect("the result is a float"),
            expected,
            "conv({source_value})"
        );
    }
}

#[test]
fn fconv_of_a_bignum_beside_a_float_matches_the_oracle() {
    // `scale(I, F) when is_integer(I), is_float(F) -> I * F.` — the `fconv` on the
    // integer argument runs with a *bignum* value (the guard `is_integer/1` accepts
    // a bignum), so `scale/2` exercises the same conversion through real compiled
    // code. `floats:scale(1 bsl 60, 1.0)` ⇒ `1.1529215046068470e+18`.
    let mut ctx = MinimalContext::new();
    let big = terms::make_integer(&mut ctx, 1i64 << 60).expect("the arena builds a bignum");
    let one = float(&mut ctx, 1.0);
    let result = run("scale", 2, &[big, one], &mut ctx).expect("scale(2^60, 1.0)");
    assert_eq!(terms::float_value(result).unwrap(), 2.0f64.powi(60));
}

#[test]
fn fconv_of_an_overwide_bignum_raises_badarith() {
    // 2^1024 (17 limbs, bitlen 1025) is above the largest finite double, so
    // `big_to_double` fails and `fconv` raises — the oracle's
    // `floats:conv(1 bsl 1100)` ⇒ `error:badarith`.
    let mut limbs = vec![0u64; 17];
    limbs[16] = 1; // 2^1024
    let mut ctx = MinimalContext::new();
    let arg = bignum(&mut ctx, false, &limbs);
    let err = run("conv", 1, &[arg], &mut ctx).expect_err("conv(2^1024)");
    assert!(matches!(err, BeamError::BadArith), "got {err:?}");

    // The guarded `scale/2` refuses it the same way (no fail label on `fconv`).
    let mut ctx = MinimalContext::new();
    let arg = bignum(&mut ctx, false, &limbs);
    let one = float(&mut ctx, 1.0);
    let err = run("scale", 2, &[arg, one], &mut ctx).expect_err("scale(2^1024, 1.0)");
    assert!(matches!(err, BeamError::BadArith), "got {err:?}");
}

#[test]
fn division_by_zero_raises_badarith() {
    // `fdiv` carries no fail label: `1.0 / 0.0` is `+inf`, a non-finite result, so
    // the family raises the typed `badarith` (the oracle raises `error:badarith`).
    let mut ctx = MinimalContext::new();
    let one = float(&mut ctx, 1.0);
    let zero = float(&mut ctx, 0.0);
    let err = run("divide", 2, &[one, zero], &mut ctx).expect_err("1.0 / 0.0");
    assert!(matches!(err, BeamError::BadArith), "got {err:?}");

    // `0.0 / 0.0` is a NaN result: also `badarith`.
    let mut ctx = MinimalContext::new();
    let zero = float(&mut ctx, 0.0);
    let err = run("divide", 2, &[zero, zero], &mut ctx).expect_err("0.0 / 0.0");
    assert!(matches!(err, BeamError::BadArith), "got {err:?}");
}

#[test]
fn fconv_of_a_non_number_raises_badarith() {
    // `floats:conv([])` ⇒ `error:badarith` in the oracle.
    let module = floats_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let err = machine
        .run_export(&module, atom(&module, "conv"), 1, &[NIL], &mut ctx)
        .expect_err("conv([])");
    assert!(matches!(err, BeamError::BadArith), "got {err:?}");
}
