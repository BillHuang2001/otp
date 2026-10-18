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

//! Native-JIT **tier equivalence** over the broad loader fixture
//! `beam_loader/tests/fixtures/coverage.beam`.
//!
//! Where `tests/jit.rs` proves the vertical slice's `add`/`fib`/`pick` run on the
//! native baseline tier, this test drives an *idiomatic* module through the
//! interpreter and **every native tier this build has** ([`NATIVE_TIERS`]: the
//! baseline tier always, plus the Cranelift hot tier with the `jit-cranelift`
//! feature) and requires them to agree. It is the direct evidence for the native
//! mechanism's most important claim — the per-instruction deopt — because
//! `coverage.beam` reaches opcodes whose helper result is a `Step::Goto` to a
//! **different address** (a lowered `jump`, a cross-function call, and the
//! `badarith` raise landing in a live catch frame) and a `Step::Done` (the
//! `return`), not just `Step::Next`.
//!
//! # Why this is a separate file (and reuses `MinimalContext`)
//!
//! `tests/coverage.rs` already asserts the interpreter's exact oracle values over
//! the same fixture with its own `CoverageCtx` (whose BIF hook can build boxed
//! floats). This file does **not** re-implement that context: the tier-equivalence
//! claim only needs a hook that returns small integers (plus an echo for
//! `get_module_info`), and boxed float *arguments* are built through the public
//! `terms` builders on `MinimalContext`'s arena. Every expected value asserted here
//! is the one `tests/coverage.rs` establishes from the real system ERTS.
//!
//! # What each export exercises through the native path
//!
//! | export | inlined prefix → helper | control-flow result |
//! |---|---|---|
//! | `tuple_ops/1` | `select_tuple_arity`/`put_tuple2`/`element` via helper | helper `Goto` (labels) |
//! | `list_ops/1` | recursive `get_list`/`put_list`/call via helper | helper `Goto`, cross-function |
//! | `make_adder/1`, `apply_adder/2` | `make_fun3`/`call_fun2` via helper | helper `Goto` |
//! | `classify/1` | `is_eq_exact`/`is_integer`/`is_ge` via helper | helper `Goto` (case labels) |
//! | `safe_div/2` | `try`/`fconv`/`fdiv`/`try_case` via helper | helper `Goto` to the **catch** handler |
//! | `float_ops/2` | `gc_bif '+'\''-'\''*'` via helper | `Next` |
//! | `map_ops/2` | `put_map_*`/`get_map_elements` via helper | helper `Goto` |
//! | `module_info/0,1` | `call_ext_only` via helper | `Done` |
//! | `bits/1` | match side + `bs_create_bin` append via helper | helper `Goto`/`Next` |

use beam_common::{BeamError, Result};
use beam_engine::jit::baseline::BaselineJit;
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, terms, Machine, MinimalContext, Tier};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm};

/// The sibling `beam_loader` oracle fixture (reused by relative path — single
/// source of truth for the `.beam` image; never copied).
const COVERAGE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/coverage.beam"
);

/// The number of x-registers the baseline block emitter caches in machine
/// registers (`CACHE_REGISTERS`), i.e. the budget past which a register is
/// addressed directly in `machine.x`.
const CACHE_BUDGET: u32 = 5;

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// The BIF floor `coverage.beam` needs on this suite's inputs.
///
/// Small-integer `erlang:'+'/'-'/'*'`, 1-based `erlang:element/2`, and the
/// `erlang:get_module_info/1,2` echo the `module_info/0,1` forwarders reach.
/// Everything else (the float family, `try`/`catch`, maps, funs) is executed by
/// the engine itself.
fn bif_hook(module: &str, function: &str, arity: u32, args: &[Eterm]) -> Result<Eterm> {
    match (module, function, arity) {
        ("erlang", "+", 2) => small_arith(args, '+'),
        ("erlang", "-", 2) => small_arith(args, '-'),
        ("erlang", "*", 2) => small_arith(args, '*'),
        ("erlang", "element", 2) => element(args),
        // `module_info/0` forwards the module name, `module_info/1` the key; echo it
        // so the tests can assert the `call_ext_only` argument convention.
        ("erlang", "get_module_info", 1) => args.first().copied().ok_or(BeamError::BadArg),
        ("erlang", "get_module_info", 2) => args.get(1).copied().ok_or(BeamError::BadArg),
        _ => Err(BeamError::BadArg),
    }
}

/// `+`/`-`/`*` over two small integers (the only cases this suite feeds them).
fn small_arith(args: &[Eterm], op: char) -> Result<Eterm> {
    if args.len() != 2 || !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    Ok(make_small(match op {
        '+' => a + b,
        '-' => a - b,
        '*' => a * b,
        _ => return Err(BeamError::BadArg),
    }))
}

/// `erlang:element/2` — the 1-based element of a tuple.
fn element(args: &[Eterm]) -> Result<Eterm> {
    if args.len() != 2 || !is_small(args[0]) {
        return Err(BeamError::BadArg);
    }
    let index = signed_value(args[0]);
    if index < 1 {
        return Err(BeamError::BadArg);
    }
    // A non-tuple or an out-of-range index is already a typed `BadArg`.
    terms::tuple_element(args[1], (index - 1) as usize)
}

/// The apply hook: `coverage.beam`'s sole lambda, `'-make_adder/1-fun-0-'/2`
/// (fun-table index 0), is `fun(X) -> X + N end` with `N` captured as its free
/// variable.
fn apply_hook(fun: Eterm, args: &[Eterm]) -> Result<Eterm> {
    if !terms::is_fun(fun) || terms::fun_index(fun)? != 0 {
        return Err(BeamError::BadArg);
    }
    let free = terms::fun_free(fun)?;
    if args.len() != 1 || free.len() != 1 {
        return Err(BeamError::BadArity);
    }
    small_arith(&[args[0], free[0]], '+')
}

/// The native tiers this build has to check the interpreter against.
///
/// The baseline tier is in every build; the Cranelift hot tier joins the roster
/// with the `jit-cranelift` Cargo feature. Both run the *same* shared per-instruction
/// helper through [`run_native`](beam_engine::jit), so the equivalence claim is
/// asserted for whichever tiers are compiled in.
#[cfg(feature = "jit-cranelift")]
const NATIVE_TIERS: [Tier; 2] = [Tier::BaselineJit, Tier::Cranelift];

/// The native tiers this build has to check the interpreter against.
#[cfg(not(feature = "jit-cranelift"))]
const NATIVE_TIERS: [Tier; 1] = [Tier::BaselineJit];

/// A context with the fixture's BIF floor and apply hook installed.
fn context() -> MinimalContext {
    let mut ctx = MinimalContext::with_bif_hook(Box::new(bif_hook));
    ctx.set_apply_hook(Box::new(apply_hook));
    ctx
}

fn load() -> Module {
    load_file(COVERAGE_BEAM).expect("load coverage.beam")
}

/// Run `name/args.len()` on a **fresh** machine through `tier`, over `ctx`.
fn run_on_tier(
    ctx: &mut MinimalContext,
    module: &Module,
    tier: Tier,
    name: &str,
    args: &[Eterm],
) -> Result<Eterm> {
    let mut machine = Machine::new();
    machine.run_with_tier(
        module,
        atom(module, name),
        args.len() as u32,
        args,
        ctx,
        tier,
    )
}

/// Assert every native tier returns a result structurally equal to the
/// interpreter's, and return the shared value.
///
/// `beam_term::cmp` is the module's exact `=:=` ordering, so boxed tuples/floats
/// (whose raw `Eterm` words differ by allocation address) compare by value.
fn agree(ctx: &mut MinimalContext, module: &Module, name: &str, args: &[Eterm]) -> Eterm {
    let interpreted = run_on_tier(ctx, module, Tier::Interpreter, name, args)
        .unwrap_or_else(|err| panic!("interpreter {name}/{} failed: {err:?}", args.len()));
    for tier in NATIVE_TIERS {
        let got = run_on_tier(ctx, module, tier, name, args)
            .unwrap_or_else(|err| panic!("{} {name}/{} failed: {err:?}", tier.name(), args.len()));
        assert!(
            beam_term::cmp(got, interpreted, &module.atom_table).is_eq(),
            "{name}/{} differs on {}: {got:?} vs {interpreted:?}",
            args.len(),
            tier.name()
        );
    }
    interpreted
}

/// Assert `term` is a tuple of exactly `expected` (compared structurally).
fn assert_tuple(module: &Module, term: Eterm, expected: &[Eterm]) {
    assert_eq!(
        terms::tuple_arity(term).expect("a tuple"),
        expected.len(),
        "tuple arity"
    );
    for (i, want) in expected.iter().enumerate() {
        let got = terms::tuple_element(term, i).expect("in-range element");
        assert!(
            beam_term::cmp(got, *want, &module.atom_table).is_eq(),
            "tuple element {i}: {got:?} != {want:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// The tier roster
// ---------------------------------------------------------------------------

#[test]
fn the_native_tiers_are_in_the_roster() {
    assert!(available_tiers().contains(&"baseline-jit"));
    assert!(Tier::BaselineJit.is_available());
    // The Cranelift hot tier joins the roster exactly with the `jit-cranelift`
    // feature, which is what `NATIVE_TIERS` tracks.
    assert_eq!(
        available_tiers().contains(&"jit-cranelift"),
        cfg!(feature = "jit-cranelift")
    );
    // `NATIVE_TIERS` is the set `agree` and the register-file tests below iterate, so
    // every member must be a tier the crate actually reports as available. (Asserting
    // the shape of `NATIVE_TIERS` *itself* — e.g. that it holds `Cranelift` exactly
    // when `cfg!(feature = "jit-cranelift")` — could never fail: the constant is
    // defined that way two screens up. Cross-checking it against the crate's roster is
    // what makes the claim load-bearing.)
    for tier in NATIVE_TIERS {
        assert!(
            tier.is_available(),
            "{tier:?} is driven by NATIVE_TIERS but is not in the build roster {:?}",
            available_tiers()
        );
    }
}

// ---------------------------------------------------------------------------
// tuples / lists — helper `Goto` across function-local labels
// ---------------------------------------------------------------------------

#[test]
fn tuple_ops_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();

    let two = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2)]).expect("tuple");
    let result = agree(&mut ctx, &module, "tuple_ops", &[two]);
    assert_tuple(&module, result, &[make_small(2), make_small(1), make_small(3)]);

    let three =
        terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).expect("tuple");
    let result = agree(&mut ctx, &module, "tuple_ops", &[three]);
    assert_tuple(
        &module,
        result,
        &[make_small(3), make_small(2), make_small(1), make_small(2)],
    );

    // The arity-4 clause reaches `erlang:element/2` through the helper.
    let four = terms::make_tuple(
        &mut ctx,
        &[make_small(5), make_small(6), make_small(7), make_small(8)],
    )
    .expect("tuple");
    assert_eq!(agree(&mut ctx, &module, "tuple_ops", &[four]), make_small(5));
}

#[test]
fn list_ops_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();

    let list = terms::make_list(&mut ctx, &[make_small(1), make_small(2), make_small(3)])
        .expect("list");
    let result = agree(&mut ctx, &module, "list_ops", &[list]);
    assert_eq!(
        terms::list_elements(result).expect("a proper list"),
        vec![make_small(2), make_small(4), make_small(6)]
    );

    let empty = terms::make_list(&mut ctx, &[]).expect("empty list");
    assert_eq!(agree(&mut ctx, &module, "list_ops", &[empty]), beam_term::NIL);
}

// ---------------------------------------------------------------------------
// funs — the closure path (`make_fun3` + `call_fun2`)
// ---------------------------------------------------------------------------

#[test]
fn the_closure_path_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();

    // `beam_term::cmp` orders funs by their raw word (a documented fallback), so
    // the closure is checked structurally on each tier rather than cross-compared.
    for tier in std::iter::once(Tier::Interpreter).chain(NATIVE_TIERS) {
        let fun = run_on_tier(&mut ctx, &module, tier, "make_adder", &[make_small(10)])
            .unwrap_or_else(|err| panic!("{tier:?} make_adder/1 failed: {err:?}"));
        assert!(terms::is_fun(fun), "{tier:?}: make_adder/1 returns a fun");
        assert_eq!(terms::fun_index(fun).unwrap(), 0);
        assert_eq!(terms::fun_free(fun).unwrap(), &[make_small(10)]);
    }

    assert_eq!(
        agree(&mut ctx, &module, "apply_adder", &[make_small(10), make_small(5)]),
        make_small(15)
    );
}

// ---------------------------------------------------------------------------
// case / guards
// ---------------------------------------------------------------------------

#[test]
fn classify_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();

    for (arg, expected) in [
        (make_small(0), "zero"),
        (make_small(5), "positive"),
        (make_small(-3), "negative"),
    ] {
        let got = agree(&mut ctx, &module, "classify", &[arg]);
        assert_eq!(got, make_atom(atom(&module, expected)));
    }

    // A non-integer falls through every integer guard to `other`.
    let non_integer = terms::make_float(&mut ctx, 1.5).expect("float");
    assert_eq!(
        agree(&mut ctx, &module, "classify", &[non_integer]),
        make_atom(atom(&module, "other"))
    );
}

// ---------------------------------------------------------------------------
// floats + exceptions — the `badarith` helper `Goto` into a live catch frame
// ---------------------------------------------------------------------------

#[test]
fn float_arithmetic_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();
    // `float_ops(2, 3) = (2+3) + (2-3) + (2*3) = 10` (integer tower; the same
    // `gc_bif` path the helper executes).
    assert_eq!(
        agree(&mut ctx, &module, "float_ops", &[make_small(2), make_small(3)]),
        make_small(10)
    );
}

#[test]
fn safe_div_success_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();

    // The `try` body's `fconv`/`fdiv` run through the helper; the result is `{ok, 3.5}`.
    let result = agree(&mut ctx, &module, "safe_div", &[make_small(7), make_small(2)]);
    assert_tuple(
        &module,
        result,
        &[
            make_atom(atom(&module, "ok")),
            terms::make_float(&mut ctx, 3.5).expect("float"),
        ],
    );
}

#[test]
fn safe_div_raises_land_in_the_catch_frame_on_both_tiers() {
    let module = load();
    let mut ctx = context();

    // `safe_div(1, 0)` takes the first clause's static literal — a helper `Goto`
    // through the `is_eq_exact` guard.
    let result = agree(&mut ctx, &module, "safe_div", &[make_small(1), make_small(0)]);
    assert_tuple(
        &module,
        result,
        &[
            make_atom(atom(&module, "error")),
            make_atom(atom(&module, "division_by_zero")),
        ],
    );

    // `safe_div([], 2)`: `fconv` on a non-number raises `badarith` with no fail
    // label; the helper must hand back a `Goto` to the *catch* handler, which yields
    // `{error, badarith}`.
    let result = agree(&mut ctx, &module, "safe_div", &[beam_term::NIL, make_small(2)]);
    assert_tuple(
        &module,
        result,
        &[
            make_atom(atom(&module, "error")),
            make_atom(atom(&module, "badarith")),
        ],
    );

    // `safe_div(1, 0.0)`: the float `fdiv` overflows and the same catch path runs.
    let one = terms::make_float(&mut ctx, 1.0).expect("float");
    let zero = terms::make_float(&mut ctx, 0.0).expect("float");
    let result = agree(&mut ctx, &module, "safe_div", &[one, zero]);
    assert_tuple(
        &module,
        result,
        &[
            make_atom(atom(&module, "error")),
            make_atom(atom(&module, "badarith")),
        ],
    );
}

// ---------------------------------------------------------------------------
// maps
// ---------------------------------------------------------------------------

#[test]
fn map_ops_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();

    for (key, expected) in [
        (make_atom(atom(&module, "a")), 52),
        (make_atom(atom(&module, "b")), 2),
        (make_atom(atom(&module, "c")), 42),
        (make_small(99), 0),
    ] {
        let got = agree(&mut ctx, &module, "map_ops", &[key, make_small(42)]);
        assert_eq!(got, make_small(expected), "map_ops(key, 42)");
    }
}

// ---------------------------------------------------------------------------
// external calls
// ---------------------------------------------------------------------------

#[test]
fn module_info_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();

    assert_eq!(
        agree(&mut ctx, &module, "module_info", &[]),
        make_atom(atom(&module, "coverage"))
    );
    let key = make_atom(atom(&module, "module_info"));
    assert_eq!(agree(&mut ctx, &module, "module_info", &[key]), key);
}

// ---------------------------------------------------------------------------
// The supported match path and the supported construction path
// ---------------------------------------------------------------------------

#[test]
fn the_bitstring_match_path_agrees_across_tiers() {
    let module = load();
    let mut ctx = context();
    // `bits(<<>>)` takes the no-match clause (`bs_set_position`/`bs_get_tail`).
    let empty = terms::make_heap_binary(&mut ctx, &[]).expect("empty binary");
    let result = agree(&mut ctx, &module, "bits", &[empty]);
    assert_eq!(terms::binary_bytes(result).expect("a binary"), &[]);
}

#[test]
fn the_append_segment_agrees_on_both_tiers() {
    let module = load();
    let mut ctx = context();
    // The clause body builds `<<Rest/binary, H:8>>` via an `append` segment the
    // bitstring family supports: a first-position `private_append` over a writable
    // payload writes **in place**, and grows the payload when the bytes would not fit
    // (the copy-rebuild form is only the fallback when growth is refused or the source
    // aliases the payload). Both tiers must produce the rotated bytes `<<2,3,1>>` and
    // agree with the interpreter.
    let binary = terms::make_heap_binary(&mut ctx, &[1, 2, 3]).expect("binary");
    let result = agree(&mut ctx, &module, "bits", &[binary]);
    assert_eq!(
        terms::binary_bytes(result).expect("a binary"),
        &[2, 3, 1],
        "bits/1 rotates the leading byte to the end"
    );
}

// ---------------------------------------------------------------------------
// Registers beyond the cache budget (direct `machine.x` addressing)
// ---------------------------------------------------------------------------

#[test]
fn the_fixture_stays_within_the_cache_budget() {
    // The baseline emitter holds up to `CACHE_BUDGET` x-registers in machine
    // registers, chosen by a **live-interval** allocator: the registers whose
    // `[first touch, last touch]` spans the block most widely (ties by ascending
    // index) become resident and everything else is addressed directly in
    // `machine.x`. The artifact records both the resident set and the highest index
    // the driver's `native_ok` guard has to cover.
    //
    // This fixture — idiomatic output of the system `erlc` — turns out to name only
    // x0..x3 in its inlined instructions, so `highest_native_register == 3` and the
    // residents are exactly {x0, x1, x2, x3}: the fixture stays well inside the
    // five-register budget, and `native == cached` (no register is addressed natively
    // that the cache did not take). That makes this a real regression guard (a change
    // to the inline set that starts addressing a register beyond x3, or an allocator
    // change, shows up here), and it is why the wide-block path itself is covered by
    // `tests/jit.rs` (`the_widened_pool_caches_a_register_the_old_budget_could_not`),
    // which builds the IR explicitly.
    let module = load();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile coverage.beam");
    let cached = compiled
        .highest_cached_register()
        .expect("the fixture must cache some registers");
    let native = compiled
        .highest_native_register()
        .expect("the fixture must touch some registers natively");
    assert_eq!(
        compiled.cached_registers(),
        &[0u32, 1, 2, 3],
        "the fixture's residents are exactly x0..x3"
    );
    assert_eq!(cached, 3, "the allocator's residents are x0..x3");
    assert!(
        compiled.cached_registers().len() as u32 <= CACHE_BUDGET,
        "the allocator never caches more registers than the budget"
    );
    assert_eq!(
        native, cached,
        "coverage.beam is expected to stay inside the cache budget"
    );
    assert!(
        native < beam_engine::MAX_REGISTER,
        "the bound must be addressable in `machine.x`"
    );
}

#[test]
fn the_fixture_agrees_on_a_register_file_covering_exactly_the_native_bound() {
    // A machine whose register file is exactly as long as the highest natively
    // touched register is the tightest native entry the driver's guard allows; a
    // shorter one (see `tests/jit.rs`) must stay on the interpreter step. Both must
    // agree with the interpreter, result or typed error.
    let module = load();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile coverage.beam");
    let native = compiled
        .highest_native_register()
        .expect("the fixture must touch some registers natively");
    let max_register = native + 1;

    let mut ctx = context();
    for (name, args) in [
        ("tuple_ops", vec![terms::make_tuple(&mut ctx, &[make_small(1), make_small(2)]).unwrap()]),
        ("classify", vec![make_small(7)]),
        ("float_ops", vec![make_small(3), make_small(4)]),
        ("map_ops", vec![make_atom(atom(&module, "a")), make_small(42)]),
        ("bits", vec![terms::make_heap_binary(&mut ctx, &[1, 2, 3]).unwrap()]),
    ] {
        let mut interpreted = Machine::with_max_register(max_register);
        let expected = interpreted.run_export(
            &module,
            atom(&module, name),
            args.len() as u32,
            &args,
            &mut ctx,
        );
        for tier in NATIVE_TIERS {
            let mut jitted = Machine::with_max_register(max_register);
            let got = jitted.run_with_tier(
                &module,
                atom(&module, name),
                args.len() as u32,
                &args,
                &mut ctx,
                tier,
            );
            match (got, &expected) {
                (Ok(got), Ok(expected)) => assert!(
                    beam_term::cmp(got, *expected, &module.atom_table).is_eq(),
                    "{name}/{} differs on {} with max_register={max_register}: {got:?} vs {expected:?}",
                    args.len(),
                    tier.name()
                ),
                (Err(got), Err(expected)) => assert_eq!(
                    format!("{got:?}"),
                    format!("{expected:?}"),
                    "{name}/{} disagrees on the typed error with max_register={max_register}",
                    args.len()
                ),
                (got, expected) => panic!(
                    "{name}/{} disagrees on {} with max_register={max_register}: {got:?} vs {expected:?}",
                    args.len(),
                    tier.name()
                ),
            }
        }
    }
}
