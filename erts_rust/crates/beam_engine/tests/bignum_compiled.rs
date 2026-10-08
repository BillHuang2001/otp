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

//! Compiled-bignum oracle-equivalence test.
//!
//! Proves that **compiled** arithmetic on integers outside the 60-bit immediate
//! range matches the real system Erlang exactly, on every execution tier.
//!
//! The `bignum.beam` fixture (produced by the system `erlc`, the differential
//! oracle — see `tests/fixtures/README.md`) covers both shapes:
//!
//! * functions that return a big integer **literal** emitted inline in the
//!   instruction stream (`move {integer, V} {x,0}`, `V` in `[2^59, 2^63)`), which
//!   the engine materialises as an exact bignum via `terms::make_integer`;
//! * functions that **overflow at runtime** from arguments, which route through
//!   `ExecutionContext::call_bif` (`gc_bif '+'`/`'-'`/`'*'`), plus big literal
//!   *operands* fed into that BIF.
//!
//! Every expected value below was derived from the real `erl` (`OTP 29`,
//! `erts-17.1`) with e.g.
//! `erl -noshell -eval 'io:format("~p~n",[bignum:mul(123456789012345678,123456789012345678)]),halt().'`
//! and is asserted **bit-exactly** against the term the engine produces (the
//! representation — a small int inside `MIN_SMALL..=MAX_SMALL`, an exact bignum
//! outside it — is asserted too, not just the value).
//!
//! The test also pins the tier seam: the interpreter, the native baseline tier
//! and (when the `jit-cranelift` feature is on) the Cranelift hot tier must all
//! return the identical term.

use std::path::Path;

use beam_common::BeamError;
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, Tier};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm, MAX_SMALL, MIN_SMALL};

/// The committed fixture, built by the system `erlc` (the oracle toolchain).
const BIGNUM_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/bignum.beam"
);

/// Skips the calling test (loudly) when the committed fixture is absent.
///
/// The fixture is committed, so this never fires in a normal checkout; it exists
/// so a stripped tree degrades to a loud skip rather than a confusing failure.
fn skip_if_missing() -> bool {
    if Path::new(BIGNUM_BEAM).exists() {
        return false;
    }
    eprintln!(
        "SKIP: {BIGNUM_BEAM} is missing — regenerate it with the system `erlc` \
         (see tests/fixtures/README.md)"
    );
    true
}

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in bignum.beam"))
}

fn module() -> Module {
    load_file(BIGNUM_BEAM).expect("load bignum.beam")
}

// ---------------------------------------------------------------------------
// The exact-integer tower the BIF hook serves
// ---------------------------------------------------------------------------

/// An integer as a term denotes it: an immediate small int, or an exact bignum.
///
/// Deriving this from a term lets a test assert the *representation* the engine
/// chose (small vs bignum) as well as the value — the oracle's canonical choice.
#[derive(Debug, PartialEq, Eq)]
enum Exact {
    Small(i64),
    Big { negative: bool, digits: Vec<u64> },
}

/// The canonical `Exact` for `value`, matching what ERTS/`make_integer` produce:
/// a small int inside `MIN_SMALL..=MAX_SMALL`, else a bignum with least
/// significant `u64` digits first and no leading zero digit.
fn exact_of(value: i128) -> Exact {
    if (MIN_SMALL as i128..=MAX_SMALL as i128).contains(&value) {
        return Exact::Small(value as i64);
    }
    let mut magnitude = value.unsigned_abs();
    let mut digits = Vec::new();
    while magnitude != 0 {
        digits.push(magnitude as u64);
        magnitude >>= 64;
    }
    Exact::Big {
        negative: value < 0,
        digits,
    }
}

/// Read a live integer term (small or bignum) as an [`Exact`].
///
/// Panics if `term` is not an integer — a wrong term type here is a test bug, not
/// a recoverable runtime condition.
fn exact(term: Eterm) -> Exact {
    if is_small(term) {
        return Exact::Small(signed_value(term));
    }
    // SAFETY: `term` is live (it came from the engine/context arena, which outlives
    // this call); `is_big` only dereferences a *boxed* term's header word.
    assert!(
        unsafe { beam_term::is_big(term) },
        "expected an integer term, got {term:?}"
    );
    // SAFETY: `is_big` established `term` is a live bignum written in the frozen
    // `[header, d0..dn]` layout, so `big_parts`' precondition holds; the digits are
    // copied out, so the borrow ends here.
    let (negative, digits) = unsafe { beam_term::big_parts(term) };
    Exact::Big {
        negative,
        digits: digits.to_vec(),
    }
}

// ---------------------------------------------------------------------------
// The BIF hook: exact integer `+`, `-`, `*`
// ---------------------------------------------------------------------------

/// A minimal exact-integer number tower for the `erlang` arithmetic BIFs.
///
/// It keeps its own term arena: a BIF hook has no access to the context's
/// `alloc_words`, so a bignum *result* is written into a `Box<[Eterm]>` the hook
/// owns. The box is moved into `arena` (the buffer address is stable across the
/// move), and the arena lives exactly as long as the hook — i.e. as long as the
/// context — so every returned term stays valid for the run.
struct BigArith {
    arena: Vec<Box<[Eterm]>>,
}

impl BigArith {
    fn new() -> BigArith {
        BigArith { arena: Vec::new() }
    }

    /// Dispatch one `erlang` arithmetic BIF; anything else is a typed `BadArg`
    /// (the fixture only calls these).
    fn call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm, BeamError> {
        if module != "erlang" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let a = self.read(args[0])?;
        let b = self.read(args[1])?;
        let value = match function {
            "+" => a.checked_add(b),
            "-" => a.checked_sub(b),
            "*" => a.checked_mul(b),
            _ => return Err(BeamError::BadArg),
        };
        // A result wider than `i128` is a `system_limit` in real ERTS; the fixture
        // never reaches it.
        let value = value.ok_or(BeamError::SystemLimit)?;
        Ok(self.write(value))
    }

    /// Read a small int or a bignum term as an `i128` (the fixture's values all fit).
    fn read(&self, term: Eterm) -> Result<i128, BeamError> {
        if is_small(term) {
            return Ok(signed_value(term) as i128);
        }
        // SAFETY: `term` is a live argument from the engine; if it is boxed, its
        // header word is a live aligned term word.
        if !unsafe { beam_term::is_big(term) } {
            return Err(BeamError::BadArith);
        }
        // SAFETY: `is_big` established a live bignum in the frozen layout.
        let (negative, digits) = unsafe { beam_term::big_parts(term) };
        // The fixture's operands are at most two 64-bit digits wide.
        if digits.len() > 2 {
            return Err(BeamError::SystemLimit);
        }
        let mut magnitude = 0u128;
        for (index, &digit) in digits.iter().enumerate() {
            magnitude |= (digit as u128) << (64 * index);
        }
        // `magnitude == 2^127` would overflow the negated `i128`; reject it rather
        // than wrapping.
        if magnitude > i128::MAX as u128 + 1 {
            return Err(BeamError::SystemLimit);
        }
        let value = magnitude as i128;
        Ok(if negative { -value } else { value })
    }

    /// Build the term for `value`: a small int when it fits, else an exact bignum.
    fn write(&mut self, value: i128) -> Eterm {
        if (MIN_SMALL as i128..=MAX_SMALL as i128).contains(&value) {
            return make_small(value as i64);
        }
        let negative = value < 0;
        let mut magnitude = value.unsigned_abs();
        let mut digits = Vec::new();
        while magnitude != 0 {
            digits.push(magnitude as u64);
            magnitude >>= 64;
        }
        let mut block: Box<[Eterm]> = vec![Eterm::from_raw(0); digits.len() + 1].into_boxed_slice();
        // SAFETY: the box owns `big_size_words(digits.len())` writable words — the
        // block `write_big` requires — and `digits` is non-empty and normalised.
        // The address survives the move of the `Box` into the arena below, and the
        // arena lives for the whole run.
        let term = unsafe { beam_term::write_big(block.as_mut_ptr(), negative, &digits) };
        self.arena.push(block);
        term
    }
}

/// The fixture's execution context: an unbounded arena plus the exact-integer
/// arithmetic hook.
fn context() -> MinimalContext {
    let mut hook = BigArith::new();
    MinimalContext::with_bif_hook(Box::new(
        move |module: &str, function: &str, arity: u32, args: &[Eterm]| -> Result<Eterm, BeamError> {
            hook.call(module, function, arity, args)
        },
    ))
}

// ---------------------------------------------------------------------------
// Case tables (every value oracle-derived)
// ---------------------------------------------------------------------------

/// An oracle case: an export, its small-int arguments, and the exact value
/// `erl` returns.
struct Case {
    export: &'static str,
    args: &'static [i64],
    value: i128,
}

/// The literal-returning exports and their oracle values. Every value above
/// `MAX_SMALL` (or below `MIN_SMALL`) must come back as an exact bignum.
const LITERAL_CASES: &[Case] = &[
    // 2^59 — the first value above MAX_SMALL (2^59 - 1).
    Case {
        export: "lit_2pow59",
        args: &[],
        value: 576_460_752_303_423_488,
    },
    Case {
        export: "lit_2pow59_plus1",
        args: &[],
        value: 576_460_752_303_423_489,
    },
    // Just below 2^60.
    Case {
        export: "lit_nines",
        args: &[],
        value: 999_999_999_999_999_999,
    },
    // 2^61.
    Case {
        export: "lit_2pow61",
        args: &[],
        value: 2_305_843_009_213_693_952,
    },
    // MIN_SMALL — a small int, the negative literal boundary.
    Case {
        export: "lit_neg_2pow59",
        args: &[],
        value: -576_460_752_303_423_488,
    },
    Case {
        export: "lit_neg_nines",
        args: &[],
        value: -999_999_999_999_999_999,
    },
    // MIN_SMALL - 1 — the first value below MIN_SMALL.
    Case {
        export: "lit_neg_2pow59_minus1",
        args: &[],
        value: -576_460_752_303_423_489,
    },
    // MAX_SMALL (2^59 - 1) — still an immediate.
    Case {
        export: "max_small",
        args: &[],
        value: 576_460_752_303_423_487,
    },
    // MIN_SMALL (-2^59) — still an immediate.
    Case {
        export: "min_small",
        args: &[],
        value: -576_460_752_303_423_488,
    },
];

/// Runtime-overflow exports — the arithmetic happens inside the BIF, so the
/// compiler cannot fold it.
const ARITH_CASES: &[Case] = &[
    // MAX_SMALL + 1 crosses into the bignum range.
    Case {
        export: "add",
        args: &[576_460_752_303_423_487, 1],
        value: 576_460_752_303_423_488,
    },
    // 2^61 + 2^61 = 2^62.
    Case {
        export: "add",
        args: &[2_305_843_009_213_693_952, 2_305_843_009_213_693_952],
        value: 4_611_686_018_427_387_904,
    },
    // 1 - MIN_SMALL = -MAX_SMALL.
    Case {
        export: "sub",
        args: &[1, 576_460_752_303_423_488],
        value: -576_460_752_303_423_487,
    },
    // A zero result stays a (small) integer.
    Case {
        export: "sub",
        args: &[576_460_752_303_423_487, 576_460_752_303_423_487],
        value: 0,
    },
    // 123456789012345678^2 — a two-digit bignum.
    Case {
        export: "mul",
        args: &[123_456_789_012_345_678, 123_456_789_012_345_678],
        value: 15_241_578_753_238_836_527_968_299_765_279_684,
    },
    // 999999999999999999^2 — a two-digit bignum.
    Case {
        export: "mul",
        args: &[999_999_999_999_999_999, 999_999_999_999_999_999],
        value: 999_999_999_999_999_998_000_000_000_000_000_001,
    },
    // A negative two-digit bignum result.
    Case {
        export: "mul",
        args: &[-123_456_789_012_345_678, 123_456_789_012_345_678],
        value: -15_241_578_753_238_836_527_968_299_765_279_684,
    },
];

/// Big **literal operands** feeding a runtime BIF: the argument is dynamic, so
/// the expression cannot be folded — the engine must materialise the bignum
/// operand from the code stream before `call_bif` sees it.
const BIG_OPERAND_CASES: &[Case] = &[
    // 2^59 + 1.
    Case {
        export: "add_to_big",
        args: &[1],
        value: 576_460_752_303_423_489,
    },
    // 2^59 + MAX_SMALL = 2^60 - 1.
    Case {
        export: "add_to_big",
        args: &[576_460_752_303_423_487],
        value: 1_152_921_504_606_846_975,
    },
    // 999999999999999999 - 1.
    Case {
        export: "big_minus",
        args: &[1],
        value: 999_999_999_999_999_998,
    },
    // A big operand that cancels back into the small range.
    Case {
        export: "big_minus",
        args: &[999_999_999_999_999_999],
        value: 0,
    },
    // 2^61 * 1 and 2^61 * 4 = 2^63.
    Case {
        export: "scale_big",
        args: &[1],
        value: 2_305_843_009_213_693_952,
    },
    Case {
        export: "scale_big",
        args: &[4],
        value: 9_223_372_036_854_775_808,
    },
];

/// Every execution tier this build can run, interpreter first.
fn tiers() -> Vec<Tier> {
    let mut tiers = vec![Tier::Interpreter, Tier::BaselineJit];
    tiers.extend(cranelift_tier());
    tiers
}

/// The Cranelift tier, only when the `jit-cranelift` feature compiled it in.
#[cfg(feature = "jit-cranelift")]
fn cranelift_tier() -> Vec<Tier> {
    vec![Tier::Cranelift]
}

#[cfg(not(feature = "jit-cranelift"))]
fn cranelift_tier() -> Vec<Tier> {
    Vec::new()
}

/// Run `case` on `tier` and return the term it produced.
///
/// The caller must read the term before the returned context is dropped — the
/// helper hands the machine and context back for exactly that reason.
fn run(module: &Module, tier: Tier, case: &Case) -> (Eterm, Machine, MinimalContext) {
    let mut machine = Machine::new();
    let mut ctx = context();
    // Arguments may themselves be outside the immediate range (e.g. `add(2^61,
    // 2^61)`), so build each one with the same shared constructor the engine's
    // operand reader uses.
    let mut args = Vec::with_capacity(case.args.len());
    for &arg in case.args {
        args.push(terms::make_integer(&mut ctx, arg).expect("build an argument term"));
    }
    let got = machine
        .run_with_tier(
            module,
            atom(module, case.export),
            case.args.len() as u32,
            &args,
            &mut ctx,
            tier,
        )
        .unwrap_or_else(|err| {
            panic!(
                "{}({}) on the {} tier failed: {err:?}",
                case.export,
                case.args
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", "),
                tier.name()
            )
        });
    (got, machine, ctx)
}

/// Assert `case` runs on `tier` and yields exactly the oracle value.
fn assert_case(module: &Module, tier: Tier, case: &Case) {
    let (got, _machine, _ctx) = run(module, tier, case);
    assert_eq!(
        exact(got),
        exact_of(case.value),
        "{} on the {} tier (args {:?})",
        case.export,
        tier.name(),
        case.args
    );
}

// ---------------------------------------------------------------------------
// The tests
// ---------------------------------------------------------------------------

#[test]
fn the_fixture_exports_every_function_the_cases_use() {
    if skip_if_missing() {
        return;
    }
    let module = module();
    assert_eq!(module.atom_name(module.name), Some("bignum"));
    for case in LITERAL_CASES
        .iter()
        .chain(ARITH_CASES)
        .chain(BIG_OPERAND_CASES)
    {
        let arity = case.args.len() as u32;
        assert!(
            module.export(atom(&module, case.export), arity).is_some(),
            "{}/{} must be exported",
            case.export,
            arity
        );
    }
}

#[test]
fn big_integer_literals_are_materialised_exactly() {
    if skip_if_missing() {
        return;
    }
    let module = module();
    for case in LITERAL_CASES {
        assert_case(&module, Tier::Interpreter, case);
    }
}

#[test]
fn runtime_overflow_matches_the_oracle() {
    if skip_if_missing() {
        return;
    }
    let module = module();
    for case in ARITH_CASES {
        assert_case(&module, Tier::Interpreter, case);
    }
}

#[test]
fn big_literal_operands_reach_the_bif() {
    if skip_if_missing() {
        return;
    }
    let module = module();
    for case in BIG_OPERAND_CASES {
        assert_case(&module, Tier::Interpreter, case);
    }
    // The engine must have materialised the operand as a *bignum* (not a small
    // int) for every case whose literal operand is out of the immediate range.
    let (got, _machine, _ctx) = run(&module, Tier::Interpreter, &BIG_OPERAND_CASES[0]);
    assert!(
        matches!(exact(got), Exact::Big { .. }),
        "2^59 + 1 must be an exact bignum, got {got:?}"
    );
}

#[test]
fn a_bignum_argument_reaches_the_bif() {
    if skip_if_missing() {
        return;
    }
    let module = module();
    // Build the bignum argument (2^59) through the context's own arena, so it is
    // the *test* — not the engine's operand reader — that produces it; then run
    // `sub(1, 2^59)`, whose oracle value is `-576460752303423487` (an immediate).
    let mut machine = Machine::new();
    let mut ctx = context();
    let hp = ctx.alloc_words(2).expect("arena words for the bignum argument");
    // SAFETY: `alloc_words(2)` returned exactly `big_size_words(1) == 2` writable
    // words and `digits` is non-empty; the arena keeps them live for the run.
    let big = unsafe { beam_term::write_big(hp, false, &[576_460_752_303_423_488]) };

    let got = machine
        .run_with_tier(
            &module,
            atom(&module, "sub"),
            2,
            &[make_small(1), big],
            &mut ctx,
            Tier::Interpreter,
        )
        .expect("sub(1, 2^59)");
    assert_eq!(exact(got), exact_of(-576_460_752_303_423_487));
    // The oracle returns a small int here, and so must the engine.
    assert_eq!(exact(got), Exact::Small(-576_460_752_303_423_487));
}

#[test]
fn every_tier_matches_the_oracle() {
    if skip_if_missing() {
        return;
    }
    let module = module();
    let cases = LITERAL_CASES
        .iter()
        .chain(ARITH_CASES)
        .chain(BIG_OPERAND_CASES);
    for tier in tiers() {
        for case in cases.clone() {
            assert_case(&module, tier, case);
        }
    }
}

#[test]
fn every_tier_agrees_on_the_same_term() {
    if skip_if_missing() {
        return;
    }
    let module = module();
    let cases: Vec<&Case> = LITERAL_CASES
        .iter()
        .chain(ARITH_CASES)
        .chain(BIG_OPERAND_CASES)
        .collect();
    for tier in tiers() {
        for case in &cases {
            let (interpreted, _m, _c) = run(&module, Tier::Interpreter, case);
            let (got, _m, _c) = run(&module, tier, case);
            assert_eq!(
                exact(got),
                exact(interpreted),
                "{} on the {} tier",
                case.export,
                tier.name()
            );
        }
    }
}
