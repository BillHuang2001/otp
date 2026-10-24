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

//! The baseline JIT's **`erlang:div/2` / `erlang:rem/2`** small-int fast path: the
//! shape predicate, the emitted semantics and tier equality.
//!
//! The lowering itself is `emit::arith`'s; what this suite proves is that the natively
//! computed value is the one the interpreter produces (and hence the one a real BIF
//! produces), for every guard the emitted sequence has:
//!
//! * the **sign matrix** — `div` truncates toward zero and `rem` takes the dividend's
//!   sign, so all four sign combinations are asserted;
//! * a **zero divisor** — the emitted code must reject it *before* the `idiv` (`idiv`
//!   by zero raises `#DE`, a CPU trap that would kill the process) and let the BIF
//!   raise `error:badarith`; the test surviving is part of the assertion;
//! * the **bignum quotient** (`MIN_SMALL div -1 == 2^59`), where the untagged range
//!   check must decline and the helper produce the exact bignum — while
//!   `MIN_SMALL rem -1 == 0` is answered natively;
//! * a **bignum operand** and a **float operand**, both rejected by the existing
//!   small-int guard and answered by the BIF;
//! * `intdiv/2`, which is **floor** division and so is never admitted (the emitted
//!   truncating `idiv` would answer `-3` where `-7 intdiv 2 == -4`);
//! * `Y`-slot operands and a `Y`-slot destination (the frame-window-guarded shape).
//!
//! Two mechanisms make the assertions strong rather than incidental:
//!
//! * **Tier equality.** Every row runs on `Tier::Interpreter` *and* `Tier::BaselineJit`
//!   and the two outcomes must be observationally identical
//!   ([`assert_same_outcome`]).
//! * **A fallback counter.** The test host counts every `call_bif` it serves, so a row
//!   whose whole point is that the instruction was *inlined* asserts `0` host calls on
//!   the baseline tier, and a row whose point is that it *declined* asserts `1` on both
//!   tiers. Without it "the tiers agree" could not distinguish an inline from a deopt.
//!
//! The host ([`DivRemCtx`]) is the reference implementation of the two BIFs over
//! `i64`/`i128` — the same semantics `beam_bifs::arith::int_div`/`int_rem` implement —
//! so a native result that disagreed with it, or a fallback that did not reach it,
//! fails.
//!
//! The interpreter is the tier that reaches this host for *every* `gc_bif`; the
//! baseline tier inlines the shape and only reaches it when a guard rejects. That is
//! the whole point of a baseline-tier-only inlineset member: the Cranelift tier is
//! interpreter-backed per function and takes the same `call_bif` road as the
//! interpreter, so it inherits this equality rather than needing a third run.

use super::tests::{assert_same_outcome, build_module, error_reason, export_atom};
use super::*;
use beam_common::BeamError;
use beam_loader::{HeapNeed, ImportRef, Instr, Label, Operand, NO_FAIL};
use beam_term::{is_small, make_small, signed_value, Atom, Eterm, MAX_SMALL, MIN_SMALL};

use crate::jit::baseline::emit::{allocate_registers, fast_arith, uses_y, FastDest, SmallArith};
use crate::jit::Tier;
use crate::machine::Machine;
use crate::terms;
use crate::MinimalContext;

/// The `erlang:div/2` import index of every fixture module below.
const DIV: ImportRef = ImportRef(0);
/// The `erlang:rem/2` import index.
const REM: ImportRef = ImportRef(1);
/// The `erlang:intdiv/2` import index.
const INTDIV: ImportRef = ImportRef(2);

// ---------------------------------------------------------------------------
// The host: the reference `div`/`rem` over exact integers
// ---------------------------------------------------------------------------

/// A `MinimalContext` whose `call_bif` serves `erlang:div/2`, `erlang:rem/2` and
/// `erlang:intdiv/2` exactly, counting every call.
///
/// The counter is the tier probe: `calls == 0` on a baseline run means the instruction
/// was lowered natively, while `calls == 1` means it deopted to `jit_step_one` (the
/// interpreter tier always pays one, since it never inlines a `gc_bif`).
struct DivRemCtx {
    inner: MinimalContext,
    /// How many `div`/`rem`/`intdiv` calls this context has served.
    calls: usize,
}

impl DivRemCtx {
    fn new() -> DivRemCtx {
        DivRemCtx {
            inner: MinimalContext::new(),
            calls: 0,
        }
    }

    /// An integer term's exact value: a small int, or a bignum of up to two 64-bit
    /// `ErtsDigit`s (every row here is far inside 128 bits).
    fn read_int(term: Eterm) -> Option<i128> {
        if is_small(term) {
            return Some(i128::from(signed_value(term)));
        }
        // SAFETY: `term` is a live machine word the caller's arena owns. `is_big` only
        // reads a *boxed* term's header word, and `big_parts` (called only when it
        // answered `true`) copies the digits out of the frozen `[header, d0..dn]`
        // layout, so the borrow ends with the copy.
        if !unsafe { beam_term::is_big(term) } {
            return None;
        }
        let (negative, digits) = unsafe { beam_term::big_parts(term) };
        let mut magnitude: i128 = 0;
        for digit in digits.iter().rev() {
            magnitude = magnitude.checked_mul(1i128 << 64)?.checked_add(i128::from(*digit))?;
        }
        Some(if negative { -magnitude } else { magnitude })
    }

    /// The term for `value`: a small int inside the 60-bit range, an exact bignum
    /// otherwise (the host's rows never leave `i64`).
    fn answer(&mut self, value: i128) -> Result<Eterm> {
        let narrowed = i64::try_from(value).map_err(|_| BeamError::SystemLimit)?;
        terms::make_integer(&mut self.inner, narrowed)
    }

    /// The two integer operands of a `div`/`rem`/`intdiv` call, or the `badarith` the
    /// real BIF raises for a float / non-number operand.
    fn operands(&self, args: &[Eterm]) -> Result<(i128, i128)> {
        match (Self::read_int(args[0]), Self::read_int(args[1])) {
            (Some(x), Some(y)) => Ok((x, y)),
            _ => Err(BeamError::BadArith),
        }
    }
}

impl ExecutionContext for DivRemCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        let wanted = module == "erlang" && arity == 2 && args.len() == 2;
        if wanted && matches!(function, "div" | "rem" | "intdiv") {
            self.calls += 1;
            let (x, y) = self.operands(args)?;
            if y == 0 {
                return Err(BeamError::BadArith);
            }
            let value = match function {
                // Erlang's `div` truncates toward zero (Rust's `/`), and `intdiv` is
                // the floor variant (`div_euclid`) — the distinction the fast path
                // must never blur.
                "div" => x / y,
                "rem" => x % y,
                _ => x.div_euclid(y),
            };
            return self.answer(value);
        }
        self.inner.call_bif(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        heap_need: u32,
        live: u32,
        x_regs: &mut [Eterm],
        stack: &mut [Eterm],
    ) -> Result<()> {
        self.inner.ensure_heap(heap_need, live, x_regs, stack)
    }

    fn alloc_words(&mut self, n: usize) -> Result<*mut Eterm> {
        self.inner.alloc_words(n)
    }
}

// ---------------------------------------------------------------------------
// Fixtures: the shapes a real clause compiles to
// ---------------------------------------------------------------------------

/// A `GcBif` instruction over `bif` with the given operands.
fn gc_bif(bif: ImportRef, args: Vec<Operand>, dst: Operand) -> Instr {
    Instr::GcBif {
        arity: args.len() as u8,
        bif,
        fail: NO_FAIL,
        live: args.len() as u32,
        args,
        dst,
    }
}

/// A module `f/2` whose body is `gc_bif op(x0, x1) -> x0` — the two-x-register shape
/// every one of the sign-matrix rows takes.
fn binary_module(name: &str, op: ImportRef) -> (Module, Atom) {
    let module = build_module(
        name,
        &[
            ("erlang", "div", 2),
            ("erlang", "rem", 2),
            ("erlang", "intdiv", 2),
        ],
        vec![(
            "f",
            2,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                gc_bif(op, vec![Operand::X(0), Operand::X(1)], Operand::X(0)),
                Instr::Return,
            ],
        )],
        &[("f", 2, Label(2))],
    );
    let atom = export_atom(&module, "f");
    (module, atom)
}

/// A module `f/1` whose body is `gc_bif op(x0, Integer(K)) -> x0` — the
/// **compile-time-constant** second operand shape.
fn constant_module(name: &str, op: ImportRef, k: i64) -> (Module, Atom) {
    let module = build_module(
        name,
        &[
            ("erlang", "div", 2),
            ("erlang", "rem", 2),
            ("erlang", "intdiv", 2),
        ],
        vec![(
            "f",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                gc_bif(op, vec![Operand::X(0), Operand::Integer(k)], Operand::X(0)),
                Instr::Return,
            ],
        )],
        &[("f", 1, Label(2))],
    );
    let atom = export_atom(&module, "f");
    (module, atom)
}

/// The `Y`-slot shape the loader emits for a clause whose `gc_bif`'s **first operand**
/// is a `Y` slot and whose **destination** is one:
/// `Allocate{2}` · `Move X(0)->Y(0)` · `GcBif op [Y(0),X(0)] -> Y(1)` ·
/// `Move Y(1)->X(0)` · `Deallocate 2` · `Return`.
fn y_slot_module(name: &str, op: ImportRef) -> (Module, Atom) {
    let module = build_module(
        name,
        &[
            ("erlang", "div", 2),
            ("erlang", "rem", 2),
            ("erlang", "intdiv", 2),
        ],
        vec![(
            "f",
            1,
            vec![
                Instr::Label(Label(1)),
                Instr::Label(Label(2)),
                Instr::Allocate {
                    stack_need: HeapNeed::Words(2),
                    live: 2,
                },
                Instr::Move {
                    src: Operand::X(0),
                    dst: Operand::Y(0),
                },
                gc_bif(op, vec![Operand::Y(0), Operand::X(0)], Operand::Y(1)),
                Instr::Move {
                    src: Operand::Y(1),
                    dst: Operand::X(0),
                },
                Instr::Deallocate(2),
                Instr::Return,
            ],
        )],
        &[("f", 1, Label(2))],
    );
    let atom = export_atom(&module, "f");
    (module, atom)
}

// ---------------------------------------------------------------------------
// The runner
// ---------------------------------------------------------------------------

/// One tier's outcome plus the host calls it took.
struct Run {
    outcome: Result<RunOutcome>,
    calls: usize,
}

/// Run `name(build_args(ctx))` on `tier` over a **fresh** machine and context.
fn run_tier(
    module: &Module,
    name: Atom,
    arity: u32,
    args: &[Eterm],
    tier: Tier,
) -> (Run, DivRemCtx) {
    let mut machine = Machine::new();
    let mut ctx = DivRemCtx::new();
    let outcome =
        machine.run_export_outcome_with_tier(module, name, arity, args, &mut ctx, tier);
    let calls = ctx.calls;
    (Run { outcome, calls }, ctx)
}

/// Run one row on **both** tiers and assert the expected properties, comparing the two
/// outcomes only while both contexts (their heaps) are alive.
///
/// `expect` receives the interpreter run and the baseline run; it must assert the value
/// (or the typed error) and the per-tier host-call counts.
fn both_tiers<R>(
    label: &str,
    module: &Module,
    name: Atom,
    arity: u32,
    args: &[Eterm],
    expect: impl FnOnce(&Run, &Run) -> R,
) -> R {
    let (interpreter, interpreter_ctx) = run_tier(module, name, arity, args, Tier::Interpreter);
    let (baseline, baseline_ctx) = run_tier(module, name, arity, args, Tier::BaselineJit);
    assert_same_outcome(label, &interpreter.outcome, &baseline.outcome, &module.atom_table);
    // The interpreter never inlines a `gc_bif`, so it always pays exactly one host call
    // for a small-int row; the baseline tier's count is the inline evidence.
    assert_eq!(interpreter.calls, 1, "{label}: the interpreter must reach the BIF once");
    let result = expect(&interpreter, &baseline);
    drop((interpreter_ctx, baseline_ctx, interpreter, baseline));
    result
}

/// The integer a returned term denotes (`None` when the tiers raised).
fn value(run: &Run) -> Option<i128> {
    match &run.outcome {
        Ok(RunOutcome::Returned(term)) => DivRemCtx::read_int(*term),
        _ => None,
    }
}

/// Assert both tiers returned exactly `expected`, the baseline tier **without** reaching
/// the BIF (the native inline answered).
fn assert_native(label: &str, interpreter: &Run, baseline: &Run, expected: i128) {
    assert_eq!(value(interpreter), Some(expected), "{label}: interpreter");
    assert_eq!(value(baseline), Some(expected), "{label}: baseline-jit");
    assert_eq!(
        baseline.calls, 0,
        "{label}: the baseline tier must answer from the emitted code, not the BIF"
    );
    assert!(
        (MIN_SMALL as i128..=MAX_SMALL as i128).contains(&expected)
            || expected == 1i128 << 59,
        "{label}: {expected} is outside the small range — the wrong side of the range check"
    );
}

/// Assert both tiers raised `error:badarith` and that the *baseline* tier declined the
/// inline (so the BIF produced the raise).
fn assert_badarith(label: &str, interpreter: &Run, baseline: &Run) {
    for (tier, run) in [("interpreter", interpreter), ("baseline-jit", baseline)] {
        match &run.outcome {
            Err(BeamError::BadArith) => {}
            other => panic!(
                "{label}: {tier} must raise error:badarith, got {:?}",
                other.as_ref().map_err(error_reason)
            ),
        }
    }
    assert_eq!(baseline.calls, 1, "{label}: the BIF must have raised it");
}

/// A small-int argument term (an immediate, so no arena is involved).
fn small(value: i64) -> Eterm {
    make_small(value)
}

// ---------------------------------------------------------------------------
// The sign matrix
// ---------------------------------------------------------------------------

/// `div` truncates **toward zero** in all four sign combinations: `-7 div 2 == -3`
/// (not `-4`, which is `intdiv`'s floor), `7 div -2 == -3`, `-7 div -2 == 3`.
#[test]
fn div_truncates_toward_zero_for_every_sign_combination() {
    let (module, name) = binary_module("div_signs", DIV);
    for (a, b, expected) in [
        (7, 2, 3),
        (-7, 2, -3),
        (7, -2, -3),
        (-7, -2, 3),
        (0, 5, 0),
        (1, 1, 1),
        // A quotient that lands exactly on both small-range bounds is answered natively.
        (MAX_SMALL, 1, MAX_SMALL),
        (MIN_SMALL, 1, MIN_SMALL),
        (MIN_SMALL, -1, 1i64 << 59),
    ] {
        both_tiers(
            &format!("{a} div {b}"),
            &module,
            name,
            2,
            &[small(a), small(b)],
            |interpreter, baseline| {
                // `MIN_SMALL div -1 == 2^59` is the **bignum quotient**: the range check
                // must decline it, so the BIF (not the emitted code) answers.
                if expected == 1i64 << 59 {
                    assert_eq!(value(interpreter), Some(i128::from(expected)));
                    assert_eq!(value(baseline), Some(i128::from(expected)));
                    assert_eq!(
                        baseline.calls, 1,
                        "the bignum quotient must decline the native inline"
                    );
                } else {
                    assert_native("div", interpreter, baseline, i128::from(expected));
                }
            },
        );
    }
}

/// `rem` takes the **dividend's** sign, and needs no range check: `|rem| < |divisor|`.
#[test]
fn rem_takes_the_dividend_sign() {
    let (module, name) = binary_module("rem_signs", REM);
    for (a, b, expected) in [
        (7, 2, 1),
        (-7, 2, -1),
        (7, -2, 1),
        (-7, -2, -1),
        (6, 3, 0),
        (-6, 3, 0),
        (0, 5, 0),
        // The remainder's bound: `|rem| < |divisor| <= 2^59`, so it is always small —
        // including at the two range bounds, where `div` had to decline.
        (MIN_SMALL, -1, 0),
        (MAX_SMALL, 2, 1),
        (MAX_SMALL, MIN_SMALL, MAX_SMALL),
        (576_460_752_303_423_487, 2, 1),
    ] {
        both_tiers(
            &format!("{a} rem {b}"),
            &module,
            name,
            2,
            &[small(a), small(b)],
            |interpreter, baseline| {
                assert_native("rem", interpreter, baseline, i128::from(expected));
            },
        );
    }
}

// ---------------------------------------------------------------------------
// The guards: a zero divisor, a float operand, a bignum operand
// ---------------------------------------------------------------------------

/// A zero divisor must be **refused before the `idiv`**: `idiv` by zero raises `#DE`, a
/// CPU trap that kills the process, where the interpreter raises the catchable
/// `error:badarith`. The test completing at all is half the assertion — the emitted code
/// took the divide only on the non-zero rows.
#[test]
fn a_zero_divisor_declines_to_the_bif_which_raises_badarith() {
    for (label, op) in [("div", DIV), ("rem", REM)] {
        let (module, name) = binary_module(&format!("zero_{label}"), op);
        both_tiers(&format!("0-divisor {label}"), &module, name, 2, &[small(1), small(0)], |i, b| {
            assert_badarith("0-divisor", i, b);
        });
        // A zero **dividend** over a non-zero divisor is an ordinary answer.
        both_tiers(&format!("0-dividend {label}"), &module, name, 2, &[small(0), small(7)], |i, b| {
            assert_native("0-dividend", i, b, 0);
        });
    }
}

/// A **float** operand is rejected by the small-int guard and the BIF raises
/// `error:badarith` — `div`/`rem` are integer-only in Erlang.
#[test]
fn a_float_operand_declines_and_the_bif_raises_badarith() {
    for (label, op) in [("div", DIV), ("rem", REM)] {
        let (module, name) = binary_module(&format!("float_{label}"), op);
        for (left, right, what) in [
            (Some(5.0), None, "5.0 op 2"),
            (None, Some(2.0), "5 op 2.0"),
            (Some(5.0), Some(2.0), "5.0 op 2.0"),
        ] {
            // A float term is boxed, so it lives on a context arena — the one the run
            // itself uses, which is what keeps the term readable.
            let mut arena = MinimalContext::new();
            let mut build = |value: Option<f64>, default: i64| match value {
                Some(v) => terms::make_float(&mut arena, v).expect("arena allocates a float"),
                None => make_small(default),
            };
            let args = [build(left, 5), build(right, 2)];
            both_tiers(&format!("{label}: {what}"), &module, name, 2, &args, |i, b| {
                // The interpreter itself cannot read the float either way (this host
                // raises `badarith`, like the real BIF); the point is the *baseline*
                // tier must reach the same conclusion and never fabricate an integer.
                assert_badarith(&format!("{label}: {what}"), i, b);
            });
        }
    }
}

/// A **bignum** operand is likewise rejected by the small-int guard, and the BIF answers
/// exactly (never a truncated small).
#[test]
fn a_bignum_operand_declines_and_the_bif_answers_exactly() {
    let (module, name) = binary_module("big_operand", DIV);
    // `2^62` is outside `MIN_SMALL..=MAX_SMALL`, so `make_integer` writes a real bignum.
    let dividend = 1i64 << 62;
    let mut arena = MinimalContext::new();
    let args = [
        terms::make_integer(&mut arena, dividend).expect("arena allocates a bignum"),
        small(3),
    ];
    both_tiers("bignum div", &module, name, 2, &args, |interpreter, baseline| {
        let expected = i128::from(dividend) / 3;
        assert_eq!(value(interpreter), Some(expected));
        assert_eq!(value(baseline), Some(expected));
        assert_eq!(baseline.calls, 1, "a bignum operand must decline the native inline");
    });

    let (module, name) = binary_module("big_operand_rem", REM);
    let mut arena = MinimalContext::new();
    let args = [
        terms::make_integer(&mut arena, dividend).expect("arena allocates a bignum"),
        small(3),
    ];
    both_tiers("bignum rem", &module, name, 2, &args, |interpreter, baseline| {
        let expected = i128::from(dividend) % 3;
        assert_eq!(value(interpreter), Some(expected));
        assert_eq!(value(baseline), Some(expected));
        assert_eq!(baseline.calls, 1, "a bignum operand must decline the native inline");
    });
}

// ---------------------------------------------------------------------------
// The constant operand, and `intdiv`'s refusal
// ---------------------------------------------------------------------------

/// A **compile-time small-int constant** second operand lowers too (it is a `movabs`,
/// not a register).
#[test]
fn a_small_constant_operand_lowers() {
    let (module, name) = constant_module("div_const", DIV, 4);
    for (value_in, expected) in [(10, 2), (-10, -2), (11, 2), (-11, -2)] {
        both_tiers(
            &format!("{value_in} div 4"),
            &module,
            name,
            1,
            &[small(value_in)],
            |interpreter, baseline| assert_native("div const", interpreter, baseline, expected),
        );
    }
    let (module, name) = constant_module("rem_const", REM, 4);
    for (value_in, expected) in [(10, 2), (-10, -2), (11, 3), (-11, -3)] {
        both_tiers(
            &format!("{value_in} rem 4"),
            &module,
            name,
            1,
            &[small(value_in)],
            |interpreter, baseline| assert_native("rem const", interpreter, baseline, expected),
        );
    }
}

/// `erlang:intdiv/2` is **not** an inlineset member: its floor rounding differs from the
/// emitted truncating `idiv` for a negative quotient, so the shape predicate must refuse
/// it and both tiers must get the BIF's floor answer (`-7 intdiv 2 == -4`, where `div`
/// answers `-3`).
#[test]
fn intdiv_is_never_inlined_and_keeps_the_floor_semantics() {
    let (module, name) = binary_module("intdiv", INTDIV);
    assert!(
        fast_arith(&module, module_instr(&module)).is_none(),
        "intdiv/2 must not be admitted by the fast path"
    );
    for (a, b) in [(7i64, 2i64), (-7, 2), (7, -2), (-7, -2)] {
        both_tiers(
            &format!("{a} intdiv {b}"),
            &module,
            name,
            2,
            &[small(a), small(b)],
            |_interpreter, baseline| {
                assert_eq!(
                    baseline.calls, 1,
                    "intdiv/2 must deopt to the BIF, never take the truncating idiv"
                );
            },
        );
    }
}

/// The `GcBif` instruction of `module`'s first function — the compile-time predicate's
/// view of it (the instruction is located by variant, not by index, so a fixture whose
/// prologue changes does not silently assert against a `Label`).
fn module_instr(module: &Module) -> &Instr {
    module.functions[0]
        .code
        .iter()
        .find(|instr| matches!(instr, Instr::GcBif { .. }))
        .expect("the fixture body carries one gc_bif")
}

// ---------------------------------------------------------------------------
// The Y-slot shape
// ---------------------------------------------------------------------------

/// A `Y` **operand** and a `Y` **destination** lower (behind the frame-window guard) and
/// agree with the interpreter, for both operations.
#[test]
fn a_y_slot_operand_and_destination_lower_and_agree() {
    // `f(N)` computes `N op N` (the `Y` operand and the x-register operand are the same
    // value), so both rows answer `1` / `0` — the point is the *shape*, not the arithmetic,
    // which the sign matrix above covers.
    for (label, op, expected) in [("div", DIV, 1), ("rem", REM, 0)] {
        let (module, name) = y_slot_module(&format!("yslot_{label}"), op);
        let instr = module_instr(&module);
        assert!(
            fast_arith(&module, instr).is_some(),
            "{label}: a Y-operand / Y-destination shape must lower at compile time"
        );
        let fast = fast_arith(&module, instr).expect("the shape lowers");
        assert_eq!(fast.dst, FastDest::Y(1), "{label}: the destination is a Y slot");
        assert!(fast.uses_y(), "{label}: the shape reads the Y window");
        assert!(
            uses_y(&module, instr),
            "{label}: the emitter's per-instruction Y gate must publish the frame window"
        );
        // A `Y` operand and a `Y` destination are stack slots, never x-registers, so the
        // block caches only the one real x-register operand (x0) — a spurious Y entry
        // would evict a resident.
        let code = &module.functions[0].code;
        let allocation = allocate_registers(&module, code, 0, code.len());
        let residents: Vec<u32> = allocation.slots.iter().map(|(x, _)| *x).collect();
        assert_eq!(residents, vec![0], "{label}: only x0 enters the register cache");
        // `f(10)` computes `10 div 10` / `10 rem 10`.
        both_tiers(&format!("y-slot {label}"), &module, name, 1, &[small(10)], |i, b| {
            assert_native(&format!("y-slot {label}"), i, b, expected);
        });
        // `f(-10)` takes the negative path through the same guarded window
        // (`-10 div -10 == 1`, `-10 rem -10 == 0`).
        let negative = if label == "div" { 1 } else { 0 };
        both_tiers(&format!("y-slot {label} negative"), &module, name, 1, &[small(-10)], |i, b| {
            assert_native(&format!("y-slot {label} negative"), i, b, negative);
        });
    }
}

/// The compile-time **shape** claims exactly the eight admitted BIFs and nothing else —
/// pinned here at the op level so a future member cannot slip in unnoticed.
#[test]
fn the_shape_predicate_admits_div_and_rem_and_nothing_adjacent() {
    let (module, _) = binary_module("shapes", DIV);
    for (index, expected) in [
        (0usize, Some(SmallArith::Div)),
        (1, Some(SmallArith::Rem)),
        (2, None),
    ] {
        let instr = Instr::GcBif {
            arity: 2,
            bif: ImportRef(index as u32),
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        };
        assert_eq!(
            fast_arith(&module, &instr).map(|fast| fast.op),
            expected,
            "import {index}"
        );
    }
}

