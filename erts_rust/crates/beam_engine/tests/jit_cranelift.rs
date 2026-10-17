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

//! Cranelift hot-tier integration test (feature-gated on `jit-cranelift`).
//!
//! Drives the committed `beam_loader` oracle fixture
//! (`beam_loader/tests/fixtures/slice.beam`) through the **Cranelift tier** and
//! asserts it agrees with the interpreter — on the same [`MinimalContext`] BIF hook
//! `tests/slice.rs` uses (`erlang:'+'/2` and `erlang:'-'/2` over small integers).
//!
//! The second half is the evidence for the tier's inlined `Instr::Test` subset
//! (`src/jit/cranelift/`): every newly inlined `TestKind` is pinned on **both** its
//! success and its failure side against the interpreter and an independently
//! computed oracle, the *inline* path is proven to have been compiled (through the
//! artifact's `inlined_test_kinds()` introspection — a test that silently fell
//! through to `jit_step_one` fails), and the run-time guard's deopt (a float
//! operand) is exercised where only the shared helper can produce the answer.
//!
//! The third part is the evidence for the tier's other inlined instruction — the
//! guarded small-int `erlang:'+'/2`/`'-'/2`/`'*'/2` `gc_bif` fast path — pinned the same
//! way
//! and, in addition, **behaviourally**: the fixtures run against BIF floors that
//! count their calls, so "the fast path computed this" (no call) and "the guard
//! rejected this and the helper computed it" (exactly one call) are observable
//! rather than inferred.
//!
//! The fourth part covers the tier's *shape* instructions (`src/jit/cranelift/`):
//! `select_val` (the immediate-alternatives-only subset, unguarded), the guarded tuple
//! reads (`get_tuple_element` / `select_tuple_arity`) and the guarded list reads
//! (`get_list` / `get_hd` / `get_tl`). Every inlinable shape is pinned against the
//! interpreter **and** an independently computed oracle, the artifact's
//! `inlined_select_val_count()` / `inlined_tuple_read_kinds()` /
//! `inlined_list_read_kinds()` prove the fast path was compiled, and each run-time or
//! compile-time rejection is pinned to the interpreter's *exact* typed error.
//!
//! The whole file is `cfg`-ed out when the feature is off, so the default build
//! neither compiles nor links any Cranelift code (see `src/jit/cranelift/`).
#![cfg(feature = "jit-cranelift")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_engine::jit::cranelift::{CompiledFunctions, CraneliftJit, SmallArith};
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, terms, Machine, MinimalContext, Tier, MAX_REGISTER};
use beam_loader::{
    load_file, Export, Function, Import, ImportRef, Instr, Label, Module, Operand, TestKind,
    NO_FAIL,
};
use beam_term::{
    is_small, make_atom, make_list, make_small, signed_value, Atom, Eterm, MAX_SMALL, MIN_SMALL,
    NIL,
};

/// The `beam_loader` oracle fixture (reused, never copied).
const SLICE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../beam_loader/tests/fixtures/slice.beam"
);

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in module"))
}

/// The slice BIF hook: `erlang:'+'/2` and `erlang:'-'/2` over small integers.
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match function {
        "+" => Ok(make_small(a + b)),
        "-" => Ok(make_small(a - b)),
        _ => Err(BeamError::BadArg),
    }
}

fn slice_module() -> Module {
    load_file(SLICE_BEAM).expect("load slice.beam")
}

/// Run `export/arity` through the given tier on a fresh machine + context.
fn run_on_tier(tier: Tier, export: &str, arity: u32, args: &[Eterm]) -> Result<Eterm, BeamError> {
    let module = slice_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    machine.run_with_tier(&module, atom(&module, export), arity, args, &mut ctx, tier)
}

/// Assert the three tiers agree on `export/arity` and return the shared result.
fn agree(export: &str, arity: u32, args: &[Eterm]) -> Eterm {
    let interpreted = run_on_tier(Tier::Interpreter, export, arity, args)
        .unwrap_or_else(|err| panic!("interpreter {export}/{arity} failed: {err:?}"));
    for tier in [Tier::BaselineJit, Tier::Cranelift] {
        let got = run_on_tier(tier, export, arity, args)
            .unwrap_or_else(|err| panic!("{} {export}/{arity} failed: {err:?}", tier.name()));
        assert_eq!(got, interpreted, "{export}/{arity} differs on {}", tier.name());
    }
    interpreted
}

#[test]
fn available_tiers_lists_the_cranelift_backend() {
    assert!(available_tiers().contains(&"jit-cranelift"));
    assert!(Tier::Cranelift.is_available());
    assert_eq!(Tier::from_name("jit-cranelift"), Some(Tier::Cranelift));
    // The backend names the tier it implements.
    assert_eq!(JitBackend::name(&CraneliftJit::new()), "jit-cranelift");
    assert!(JitBackend::available(&CraneliftJit::new()));
}

#[test]
fn compile_emits_one_native_entry_per_non_empty_function() {
    let module = slice_module();
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module).expect("compile slice.beam");

    let expected: usize = module
        .functions
        .iter()
        .filter(|function| {
            !function.code.is_empty() && function.labels.contains_key(&function.entry_label)
        })
        .count();
    assert!(expected > 0, "the fixture must have functions");
    assert_eq!(compiled.function_count(), expected);

    for (index, function) in module.functions.iter().enumerate() {
        let Some(&pc) = function.labels.get(&function.entry_label) else {
            continue;
        };
        let func = index as u32;
        assert!(
            compiled.entry(func).is_some(),
            "function {func} must have a native entry"
        );
        assert_eq!(
            compiled.entry_pc(func),
            Some(pc as u32),
            "the compiled function must start at its entry label"
        );
    }
}

#[test]
fn add_matches_the_interpreter_on_the_cranelift_tier() {
    assert_eq!(agree("add", 2, &[make_small(2), make_small(3)]), make_small(5));
    assert_eq!(
        agree("add", 2, &[make_small(-4), make_small(10)]),
        make_small(6)
    );
}

#[test]
fn fib_matches_the_interpreter_on_the_cranelift_tier() {
    // `fib/1` runs `test`/`call`/`return`, so the compiled function's native input
    // crosses the helper many times and leaves/returns between functions.
    assert_eq!(agree("fib", 1, &[make_small(10)]), make_small(55));
    assert_eq!(agree("fib", 1, &[make_small(0)]), make_small(0));
    assert_eq!(agree("fib", 1, &[make_small(15)]), make_small(610));
}

#[test]
fn select_and_jump_paths_match_the_interpreter() {
    let module = slice_module();
    let a = make_atom(atom(&module, "a"));
    assert_eq!(agree("pick", 1, &[a]), make_small(1));
    assert_eq!(
        agree("pick", 1, &[make_atom(atom(&module, "slice"))]),
        make_small(2)
    );
}

#[test]
fn the_inlined_subset_agrees_over_a_wide_input_range() {
    // The tier inlines the metadata no-ops, `move`, `jump` and the small-int
    // `erlang:'+'/2`/`'-'/2`/`'*'/2` fast path, and routes everything else through the shared
    // `jit_step_one` helper plus the pc-block CFG's continuation dispatch. Running the
    // same exports over many inputs exercises every one of those paths (the inlined
    // fast path, its guard's rejection/deopt, the cross-function call/return
    // dispatch and the fall-through chain) and pins that none of them changes the
    // observable result.
    for a in -40i64..=40 {
        for b in -40i64..=40 {
            let args = [make_small(a), make_small(b)];
            assert_eq!(
                agree("add", 2, &args),
                make_small(a + b),
                "add({a}, {b}) on the inlined fast path"
            );
        }
    }
    // `fib/1` is the cross-function case: its native body `call`s into `fib/3` and
    // `return`s through the frame's continuation, so the helper's `(func, pc)` word
    // must resolve to the right block — or go back to the driver — every time.
    for n in 0i64..=16 {
        assert_eq!(agree("fib", 1, &[make_small(n)]), slice_fib(n), "fib({n})");
    }
}

/// The oracle value of `fib(n)` (the `slice.beam` fixture's own recursion), used to
/// pin the tier's agreement test against an independently computed value rather than
/// only against the interpreter.
fn slice_fib(n: i64) -> Eterm {
    fn fib(n: i64) -> i64 {
        if n < 2 {
            n
        } else {
            fib(n - 1) + fib(n - 2)
        }
    }
    make_small(fib(n))
}

#[test]
fn a_raising_bif_propagates_the_same_typed_error() {    // `add/2`'s arithmetic BIF has no fail label, so a failure is a raise; the helper
    // returns the `ERROR` word and the driver must hand back exactly that error.
    let bad = beam_term::NIL;
    let interpreted = run_on_tier(Tier::Interpreter, "add", 2, &[bad, make_small(1)])
        .expect_err("the interpreter must reject a non-integer arith argument");
    let jitted = run_on_tier(Tier::Cranelift, "add", 2, &[bad, make_small(1)])
        .expect_err("the Cranelift tier must reject it too");
    assert_eq!(format!("{jitted:?}"), format!("{interpreted:?}"));
}

#[test]
fn a_missing_export_is_a_typed_error_on_the_cranelift_tier() {
    let module = slice_module();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let err = machine
        .run_with_tier(
            &module,
            atom(&module, "add"),
            3,
            &[make_small(1), make_small(2), make_small(3)],
            &mut ctx,
            Tier::Cranelift,
        )
        .expect_err("add/3 is not exported");
    assert!(matches!(err, BeamError::Raise(_)));
}

#[test]
fn run_export_of_the_backend_matches_run_with_tier() {
    // The `JitBackend` entry and the tier-selecting entry are the same code path.
    let module = slice_module();
    let args = [make_small(2), make_small(3)];

    let mut backend = CraneliftJit::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let direct = backend
        .run_export(
            &mut Machine::new(),
            &module,
            atom(&module, "add"),
            2,
            &args,
            &mut ctx,
        )
        .expect("add(2, 3) via the backend directly");

    assert_eq!(direct, make_small(5));
}

// ---------------------------------------------------------------------------
// The inlined `test` subset (`src/jit/cranelift/`)
// ---------------------------------------------------------------------------
//
// `Instr::Test` is the instruction family with the heaviest `jit_step_one` traffic,
// so the tier inlines the shapes whose result is a **provably exact word compare**:
// the word-level type tests (`is_nil`/`is_atom`/`is_list`/`is_nonempty_list`/
// `is_boolean`) and `is_lt`/`is_ge`/`is_eq`/`is_ne`/`is_eq_exact`/`is_ne_exact` when
// at least one operand is a constant-immediate (a small int, an atom or `nil`) or,
// for the non-exact kinds, both are x-registers — after guarding the register
// operand(s) as untagged small ints, so the *tagged-word* signed order is the
// interpreter's value order. The fixtures below are hand-built modules (the loader
// is not involved) so each shape is exercised in isolation.

/// The `1`/`0` answer every fixture answers with.
fn flag(condition: bool) -> Eterm {
    make_small(if condition { 1 } else { 0 })
}

/// One operand of a hand-built `test`.
#[derive(Clone, Copy)]
enum Spec {
    /// The x-register `n`.
    X(u32),
    /// A small-integer constant operand (`{integer,V}`).
    Int(i64),
    /// The `nil` constant operand.
    Nil,
    /// An atom constant operand, interned in the fixture module's atom table.
    Atom(&'static str),
}

/// One hand-built exported function: `test <kind> [<operands>]`, answering `1` when
/// the test succeeds and `0` when it branches to its `fail` label.
struct TestFn {
    /// The exported name.
    name: &'static str,
    /// The arity (the number of x-register arguments the test reads).
    arity: u32,
    /// The classified test.
    kind: TestKind,
    /// The tested operands, in file order.
    operands: Vec<Spec>,
}

impl TestFn {
    /// A fixture named `name`, of `arity` arguments, running `kind` over `operands`.
    fn new(name: &'static str, arity: u32, kind: TestKind, operands: &[Spec]) -> TestFn {
        TestFn {
            name,
            arity,
            kind,
            operands: operands.to_vec(),
        }
    }
}

/// Build a hand-built module (the loader is not involved) whose exported functions
/// are exactly `functions`, each one basic-block pair:
///
/// ```text
///   label E:  test <kind> [<operands>] F
///             move {integer,1} -> x0    ; the test succeeded
///             return
///   label F:  move {integer,0} -> x0    ; the test failed
///             return
/// ```
///
/// `true`/`false` are interned up front because `is_boolean` compares against the
/// module's own atom words — both the interpreter and the tier look them up by name,
/// and an absent name contributes nothing.
fn test_module(name: &str, functions: &[TestFn]) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    module.atom_table.intern("true");
    module.atom_table.intern("false");

    let mut next_label = 1u32;
    for fixture in functions {
        let export = module.atom_table.intern(fixture.name);
        let entry = Label(next_label);
        next_label += 1;
        let fail = Label(next_label);
        next_label += 1;
        let args: Vec<Operand> = fixture
            .operands
            .iter()
            .map(|spec| match spec {
                Spec::X(n) => Operand::X(*n),
                Spec::Int(value) => Operand::Integer(*value),
                Spec::Nil => Operand::Nil,
                Spec::Atom(atom) => Operand::Atom(module.atom_table.intern(atom)),
            })
            .collect();
        let code = vec![
            Instr::Label(entry),
            Instr::Test {
                kind: fixture.kind,
                fail,
                args,
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(fail),
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module: module.name,
            name: export,
            arity: fixture.arity,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: export,
            arity: fixture.arity,
            label: entry,
        });
    }
    module
}

/// The fixture module for the comparison shapes: the x-register/x-register forms of
/// `<`, `>=`, `==`, `!=`, `=:=`, `=/=` and the small-int-constant forms of `<`,
/// `>=`, `==`, `=:=`, `=/=` — every one of which is inside the inlined subset.
fn comparisons_module() -> Module {
    test_module(
        "cmp",
        &[
            TestFn::new("lt_regs", 2, TestKind::IsLt, &[Spec::X(0), Spec::X(1)]),
            TestFn::new("ge_regs", 2, TestKind::IsGe, &[Spec::X(0), Spec::X(1)]),
            TestFn::new("eq_regs", 2, TestKind::IsEq, &[Spec::X(0), Spec::X(1)]),
            TestFn::new("ne_regs", 2, TestKind::IsNe, &[Spec::X(0), Spec::X(1)]),
            TestFn::new("lt_const", 1, TestKind::IsLt, &[Spec::X(0), Spec::Int(0)]),
            TestFn::new("ge_const", 1, TestKind::IsGe, &[Spec::X(0), Spec::Int(10)]),
            TestFn::new("eq_const", 1, TestKind::IsEq, &[Spec::X(0), Spec::Int(5)]),
            TestFn::new(
                "eq_exact_const",
                1,
                TestKind::IsEqExact,
                &[Spec::X(0), Spec::Int(5)],
            ),
            TestFn::new(
                "ne_exact_const",
                1,
                TestKind::IsNeExact,
                &[Spec::X(0), Spec::Int(5)],
            ),
        ],
    )
}

/// The fixture module for the word-level type tests and the immediate-constant
/// comparisons (an atom and `nil`).
fn word_module() -> Module {
    test_module(
        "word",
        &[
            TestFn::new("is_nil", 1, TestKind::IsNil, &[Spec::X(0)]),
            TestFn::new("is_atom", 1, TestKind::IsAtom, &[Spec::X(0)]),
            TestFn::new("is_list", 1, TestKind::IsList, &[Spec::X(0)]),
            TestFn::new(
                "is_nonempty_list",
                1,
                TestKind::IsNonemptyList,
                &[Spec::X(0)],
            ),
            TestFn::new("is_boolean", 1, TestKind::IsBoolean, &[Spec::X(0)]),
            TestFn::new(
                "eq_exact_atom",
                1,
                TestKind::IsEqExact,
                &[Spec::X(0), Spec::Atom("a")],
            ),
            TestFn::new(
                "ne_exact_atom",
                1,
                TestKind::IsNeExact,
                &[Spec::X(0), Spec::Atom("a")],
            ),
            TestFn::new("eq_atom", 1, TestKind::IsEq, &[Spec::X(0), Spec::Atom("a")]),
            TestFn::new(
                "eq_exact_nil",
                1,
                TestKind::IsEqExact,
                &[Spec::X(0), Spec::Nil],
            ),
        ],
    )
}

/// Run `export/arity` of `module` on `tier` with `ctx` as the execution context.
fn run_module(
    tier: Tier,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_with_tier(module, atom(module, export), arity, args, ctx, tier)
}

/// Run `export/arity` of `module` on the Cranelift tier through a **caller-owned**
/// compiled artifact — the tier's compile-once / run-many
/// [`JitBackend::run_compiled`] entry, on a fresh machine and context.
fn run_compiled(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    backend.run_compiled(
        compiled,
        &mut machine,
        module,
        atom(module, export),
        arity,
        args,
        &mut ctx,
    )
}

/// Assert the **interpreter** and the **Cranelift** tier both answer `expected`
/// (the independently computed oracle) for `export/arity`, and return it.
fn agree_tiers(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    expected: Eterm,
) -> Eterm {
    let interpreted = run_module(
        Tier::Interpreter,
        module,
        export,
        arity,
        args,
        &mut MinimalContext::new(),
    )
    .unwrap_or_else(|err| panic!("interpreter {export}/{arity} failed: {err:?}"));
    assert_eq!(
        interpreted, expected,
        "interpreter {export}/{arity} disagrees with the oracle"
    );

    let jitted = run_compiled(backend, compiled, module, export, arity, args)
        .unwrap_or_else(|err| panic!("cranelift {export}/{arity} failed: {err:?}"));
    assert_eq!(
        jitted, expected,
        "cranelift {export}/{arity} disagrees with the oracle"
    );
    expected
}

#[test]
fn the_tier_reports_which_test_kinds_it_inlined() {
    // Artifact-level evidence that the `test` fast path — not `jit_step_one` — was
    // *compiled*: a module that silently deopted would name no kind here.
    let compiled = CraneliftJit::new()
        .compile(&comparisons_module())
        .expect("compile the comparison fixtures");
    for kind in [
        TestKind::IsLt,
        TestKind::IsGe,
        TestKind::IsEq,
        TestKind::IsNe,
        TestKind::IsEqExact,
        TestKind::IsNeExact,
    ] {
        assert!(
            compiled.inlined_test_kinds().contains(&kind),
            "{kind:?} must take the inline path, got {:?}",
            compiled.inlined_test_kinds()
        );
    }
    // Every inlined `test` reads `x(0)` natively, so the driver's native-entry bound
    // guard must see it (the same introspection the `move`/arith fast paths report).
    assert!(compiled.highest_native_register().is_some());

    let compiled = CraneliftJit::new()
        .compile(&word_module())
        .expect("compile the word fixtures");
    for kind in [
        TestKind::IsNil,
        TestKind::IsAtom,
        TestKind::IsList,
        TestKind::IsNonemptyList,
        TestKind::IsBoolean,
    ] {
        assert!(
            compiled.inlined_test_kinds().contains(&kind),
            "{kind:?} must take the inline path, got {:?}",
            compiled.inlined_test_kinds()
        );
    }
    // The boxed-term / structural tests are outside the subset, so the artifact must
    // never name them.
    for kind in [
        TestKind::IsInteger,
        TestKind::IsFloat,
        TestKind::IsNumber,
        TestKind::IsTuple,
        TestKind::IsMap,
        TestKind::IsBinary,
        TestKind::IsFunction2,
        TestKind::IsTaggedTuple,
        TestKind::TestArity,
    ] {
        assert!(
            !compiled.inlined_test_kinds().contains(&kind),
            "{kind:?} must stay on the helper"
        );
    }
}

#[test]
fn inlined_small_int_comparisons_agree_over_a_wide_range() {
    let module = comparisons_module();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the comparison fixtures");

    // The inline path is only entered once the guard has accepted the operand(s) as
    // untagged small ints, so both the pass and the fail side of each comparison must
    // be pinned across the whole small-int range — including its two boundaries,
    // where the tagged-word order is easiest to get wrong.
    let values = [-40i64, -1, 0, 1, 5, 11, 40, MIN_SMALL, MAX_SMALL];
    for &a in &values {
        for &b in &values {
            let args = [make_small(a), make_small(b)];
            agree_tiers(&backend, &compiled, &module, "lt_regs", 2, &args, flag(a < b));
            agree_tiers(&backend, &compiled, &module, "ge_regs", 2, &args, flag(a >= b));
            agree_tiers(&backend, &compiled, &module, "eq_regs", 2, &args, flag(a == b));
            agree_tiers(&backend, &compiled, &module, "ne_regs", 2, &args, flag(a != b));
        }
    }

    // The small-int-constant forms guard only the x-register half; the constant is
    // the second operand, so the `fail` side is reached whenever the constant is on
    // the wrong side of the comparison.
    for &a in &values {
        let arg = [make_small(a)];
        agree_tiers(&backend, &compiled, &module, "lt_const", 1, &arg, flag(a < 0));
        agree_tiers(&backend, &compiled, &module, "ge_const", 1, &arg, flag(a >= 10));
        agree_tiers(&backend, &compiled, &module, "eq_const", 1, &arg, flag(a == 5));
        agree_tiers(
            &backend,
            &compiled,
            &module,
            "eq_exact_const",
            1,
            &arg,
            flag(a == 5),
        );
        agree_tiers(
            &backend,
            &compiled,
            &module,
            "ne_exact_const",
            1,
            &arg,
            flag(a != 5),
        );
    }

    // A representative subset on the *third* tier (the baseline backend through the
    // ordinary tier-selecting entry), so the fixtures are pinned on every tier.
    for &(a, b) in &[
        (-1i64, 0i64),
        (0, 0),
        (5, 5),
        (MIN_SMALL, MAX_SMALL),
        (MAX_SMALL, MIN_SMALL),
    ] {
        let args = [make_small(a), make_small(b)];
        for (export, expected) in [
            ("lt_regs", a < b),
            ("ge_regs", a >= b),
            ("eq_regs", a == b),
            ("ne_regs", a != b),
        ] {
            for tier in [Tier::Interpreter, Tier::BaselineJit, Tier::Cranelift] {
                let got = run_module(tier, &module, export, 2, &args, &mut MinimalContext::new())
                    .unwrap_or_else(|err| panic!("{} {export}/2 failed: {err:?}", tier.name()));
                assert_eq!(got, flag(expected), "{} {export}/2({a}, {b})", tier.name());
            }
        }
    }
}

#[test]
fn inlined_immediate_compares_and_word_type_tests_agree() {
    let mut module = word_module();
    let second = make_atom(module.atom_table.intern("b"));
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the word fixtures");

    let first = make_atom(atom(&module, "a"));
    let truthy = make_atom(atom(&module, "true"));
    let falsy = make_atom(atom(&module, "false"));
    // A cons cell — a list term that is not `[]`. The type tests are single-word
    // predicates, so a stack cell (never dereferenced) is a valid term.
    let cell = [NIL, NIL];
    let list = make_list(cell.as_ptr());

    let expect = |export: &str, arg: Eterm, succeeded: bool| {
        agree_tiers(
            &backend,
            &compiled,
            &module,
            export,
            1,
            std::slice::from_ref(&arg),
            flag(succeeded),
        );
    };

    // `is_nil` — `[]` and nothing else.
    expect("is_nil", NIL, true);
    expect("is_nil", list, false);
    expect("is_nil", make_small(0), false);
    expect("is_nil", first, false);

    // `is_atom` — the atom immediate tag; `nil` is an *IMMED2* but not an atom.
    expect("is_atom", first, true);
    expect("is_atom", truthy, true);
    expect("is_atom", NIL, false);
    expect("is_atom", make_small(3), false);
    expect("is_atom", list, false);

    // `is_list` is `[]` *or* a cons cell; `is_nonempty_list` is the cons cell only.
    expect("is_list", NIL, true);
    expect("is_list", list, true);
    expect("is_list", make_small(0), false);
    expect("is_list", first, false);
    expect("is_nonempty_list", list, true);
    expect("is_nonempty_list", NIL, false);
    expect("is_nonempty_list", make_small(0), false);

    // `is_boolean` — the module's own `true`/`false` atom words.
    expect("is_boolean", truthy, true);
    expect("is_boolean", falsy, true);
    expect("is_boolean", first, false);
    expect("is_boolean", NIL, false);
    expect("is_boolean", make_small(1), false);

    // An atom / `nil` constant in an exact or an arithmetic compare (no guard: an
    // immediate is `=:=`/`==` only to the identical word).
    expect("eq_exact_atom", first, true);
    expect("eq_exact_atom", second, false);
    expect("eq_exact_atom", NIL, false);
    expect("eq_exact_atom", make_small(0), false);
    expect("ne_exact_atom", first, false);
    expect("ne_exact_atom", second, true);
    expect("ne_exact_atom", make_small(0), true);
    expect("eq_atom", first, true);
    expect("eq_atom", second, false);
    expect("eq_atom", make_small(0), false);
    expect("eq_exact_nil", NIL, true);
    expect("eq_exact_nil", make_small(0), false);
    expect("eq_exact_nil", list, false);
}

#[test]
fn a_float_operand_deopts_to_the_helper_and_still_agrees() {
    // The guard is a *run-time* test, so the same instruction can inline one moment
    // and deopt the next. These cases force the deopt: a boxed float operand.
    let module = test_module(
        "deopt",
        &[
            TestFn::new("eq_five", 1, TestKind::IsEq, &[Spec::X(0), Spec::Int(5)]),
            TestFn::new("lt_five", 1, TestKind::IsLt, &[Spec::X(0), Spec::Int(5)]),
            TestFn::new("lt_regs", 2, TestKind::IsLt, &[Spec::X(0), Spec::X(1)]),
            TestFn::new(
                "eq_exact_five",
                1,
                TestKind::IsEqExact,
                &[Spec::X(0), Spec::Int(5)],
            ),
        ],
    );
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the deopt fixtures");
    // Compile-time evidence that these shapes are *inside* the inlined subset ...
    for kind in [TestKind::IsEq, TestKind::IsLt, TestKind::IsEqExact] {
        assert!(
            compiled.inlined_test_kinds().contains(&kind),
            "{kind:?} must take the inline path, got {:?}",
            compiled.inlined_test_kinds()
        );
    }

    // ... so the float cases can only reach the interpreter's answer by way of the
    // guard *rejecting* the boxed word and deopting to `jit_step_one`. A raw
    // tagged-word compare would answer `false` for all of `5.0 == 5`, `4.0 < 5` and
    // `4.0 < 5`: the boxed float's word is a large positive pointer, far above
    // `make_small(5) == 95` in signed order.
    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    let four_float = terms::make_float(&mut arena, 4.0).expect("the arena allocates a float");

    agree_tiers(&backend, &compiled, &module, "eq_five", 1, &[five_float], flag(true));
    agree_tiers(&backend, &compiled, &module, "lt_five", 1, &[four_float], flag(true));
    agree_tiers(
        &backend,
        &compiled,
        &module,
        "lt_regs",
        2,
        &[four_float, make_small(5)],
        flag(true),
    );
    // `5.0 =/= 5` (the exact compare is class-strict), on both paths.
    agree_tiers(
        &backend,
        &compiled,
        &module,
        "eq_exact_five",
        1,
        &[five_float],
        flag(false),
    );

    // The matching small-int cases below the constant, so the *fail* side of the
    // same instructions is pinned on the genuinely inlined path too.
    agree_tiers(&backend, &compiled, &module, "lt_five", 1, &[make_small(6)], flag(false));
    agree_tiers(&backend, &compiled, &module, "eq_five", 1, &[make_small(4)], flag(false));
    agree_tiers(
        &backend,
        &compiled,
        &module,
        "eq_exact_five",
        1,
        &[make_small(5)],
        flag(true),
    );
}

#[test]
fn non_inlinable_test_shapes_stay_on_the_helper() {
    // A boxed-term/structural test is outside the subset: the artifact must name no
    // inlined kind for this module, and the shapes must still run — through one
    // interpreter step each.
    let module = test_module(
        "helper",
        &[
            TestFn::new("is_integer", 1, TestKind::IsInteger, &[Spec::X(0)]),
            TestFn::new("is_tuple", 1, TestKind::IsTuple, &[Spec::X(0)]),
        ],
    );
    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile the helper fixtures");
    assert!(
        compiled.inlined_test_kinds().is_empty(),
        "no inlinable `test` shape was emitted, got {:?}",
        compiled.inlined_test_kinds()
    );

    let run = |export: &str, arity: u32, args: &[Eterm]| {
        run_module(
            Tier::Cranelift,
            &module,
            export,
            arity,
            args,
            &mut MinimalContext::new(),
        )
        .unwrap_or_else(|err| panic!("cranelift {export}/{arity} failed: {err:?}"))
    };
    assert_eq!(run("is_integer", 1, &[make_small(3)]), flag(true));
    assert_eq!(run("is_integer", 1, &[NIL]), flag(false));
    assert_eq!(run("is_tuple", 1, &[make_small(3)]), flag(false));
}

#[test]
fn inlined_exact_two_register_compares_agree() {
    // `is_eq_exact`/`is_ne_exact` over two x-registers is a plain word compare behind
    // a both-small-int guard, so the tier inlines it and must agree with the
    // interpreter bit for bit.
    let module = test_module(
        "exact_regs",
        &[
            TestFn::new("eq_regs", 2, TestKind::IsEqExact, &[Spec::X(0), Spec::X(1)]),
            TestFn::new("ne_regs", 2, TestKind::IsNeExact, &[Spec::X(0), Spec::X(1)]),
        ],
    );
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the exact-compare fixtures");
    let kinds = compiled.inlined_test_kinds();
    assert!(kinds.contains(&TestKind::IsEqExact), "got {kinds:?}");
    assert!(kinds.contains(&TestKind::IsNeExact), "got {kinds:?}");

    // The inline path is only entered once the guard has accepted *both* operands as
    // untagged small ints, so pin both the pass and the fail side across the whole
    // small-int range, boundaries included.
    let values = [MIN_SMALL, -40, -1, 0, 1, 5, 11, 40, MAX_SMALL];
    for &a in &values {
        for &b in &values {
            let args = [make_small(a), make_small(b)];
            agree_tiers(&backend, &compiled, &module, "eq_regs", 2, &args, flag(a == b));
            agree_tiers(&backend, &compiled, &module, "ne_regs", 2, &args, flag(a != b));
        }
    }

    // A non-small operand deopts the guard and lands on the shared helper; the values
    // still agree with the interpreter (and, for a float, with `=:=`'s answer — the
    // boxed word is a pointer, so a raw word compare would answer `false`).
    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    agree_tiers(
        &backend,
        &compiled,
        &module,
        "eq_regs",
        2,
        &[five_float, make_small(5)],
        flag(false),
    );
    agree_tiers(
        &backend,
        &compiled,
        &module,
        "ne_regs",
        2,
        &[five_float, make_small(5)],
        flag(true),
    );
    agree_tiers(
        &backend,
        &compiled,
        &module,
        "eq_regs",
        2,
        &[five_float, five_float],
        flag(true),
    );

    // The baseline tier inlines exactly the same shape, so the two native tiers must
    // answer identically on the same inputs (the interpreter is the reference).
    for (a, b) in [(5, 5), (5, 6), (MIN_SMALL, MAX_SMALL)] {
        let args = [make_small(a), make_small(b)];
        agree_tiers(&backend, &compiled, &module, "eq_regs", 2, &args, flag(a == b));
        let baseline = run_module(
            Tier::BaselineJit,
            &module,
            "eq_regs",
            2,
            &args,
            &mut MinimalContext::new(),
        )
        .expect("baseline eq_regs");
        assert_eq!(baseline, flag(a == b), "baseline diverged from the interpreter");
    }
}

#[test]
fn a_no_fail_test_raises_through_the_helper_and_is_never_inlined() {
    // `fail == NO_FAIL` means "raise on failure", so the tier must never inline it
    // even for a `TestKind` it otherwise lowers (`is_boolean`).
    let mut module = Module::default();
    module.name = module.atom_table.intern("nofail");
    module.atom_table.intern("true");
    module.atom_table.intern("false");
    let export = module.atom_table.intern("bool");
    let entry = Label(1);
    let code = vec![
        Instr::Label(entry),
        Instr::Test {
            kind: TestKind::IsBoolean,
            fail: NO_FAIL,
            args: vec![Operand::X(0)],
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module.name,
        name: export,
        arity: 1,
        code,
        blocks,
        labels,
    });
    module.exports.push(Export {
        function: export,
        arity: 1,
        label: entry,
    });

    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile the no_fail fixture");
    assert!(
        compiled.inlined_test_kinds().is_empty(),
        "a NO_FAIL test raises, so it is never inlined: {:?}",
        compiled.inlined_test_kinds()
    );

    // A *succeeding* `NO_FAIL` test runs through the helper exactly like the
    // interpreter's: it falls through to the following `move`.
    let truthy = make_atom(atom(&module, "true"));
    assert_eq!(
        run_module(
            Tier::Cranelift,
            &module,
            "bool",
            1,
            &[truthy],
            &mut MinimalContext::new(),
        )
        .expect("is_boolean(true) with no fail label"),
        make_small(1)
    );
    // A *failing* one raises the interpreter's typed `test/no_fail` error on both
    // tiers (never a silent fall-through with a wrong value).
    let interpreted = run_module(
        Tier::Interpreter,
        &module,
        "bool",
        1,
        &[NIL],
        &mut MinimalContext::new(),
    )
    .expect_err("the interpreter raises `test/no_fail`");
    let jitted = run_module(
        Tier::Cranelift,
        &module,
        "bool",
        1,
        &[NIL],
        &mut MinimalContext::new(),
    )
    .expect_err("the Cranelift tier raises `test/no_fail` too");
    assert_eq!(format!("{jitted:?}"), format!("{interpreted:?}"));
}

// ---------------------------------------------------------------------------
// The inlined small-int `erlang:'+'/2` / `'-'/2` / `'*'/2` fast path
// (`src/jit/cranelift/`)
// ---------------------------------------------------------------------------
//
// A `gc_bif erlang:'+'/2`/`'-'/2`/`'*'/2` is the hot instruction of the compute-bound
// workload (`jit_bench`'s `kernel/2`), so the tier lowers it natively for the shape
// the **baseline** tier lowers (`jit/baseline/emit.rs::try_inline_gc_bif`): two
// operands that are each an x-register or a small-int constant, and an x-register
// destination.
//
// The lowering is guarded on **run-time** values — both operands must be tagged
// small integers (`(word & 0xF) == 0xF`) and the resulting tagged word must lie in
// `[make_small(MIN_SMALL), make_small(MAX_SMALL)]` — and every rejection branches to
// the shared `jit_step_one` helper, whose interpreter step is the BIF's exact
// semantics (the guard-fail/raise split, the bignum/float paths, the `badarith`).
// The fast path can therefore only ever *replace* a successful small-int BIF call
// with the identical tagged word; it can never fabricate a result.
//
// The fixtures below are **hand-built** modules (the loader is not involved), so each
// shape is exercised in isolation, and they run against BIF floors designed to make
// the fast path and the helper *observably* different:
//
// * a **counting** floor — the interpreter steps the BIF once for the same program,
//   so a call under the Cranelift tier is positive proof that the helper (not the
//   inlined fast path) produced the answer (`jit.rs` uses the same technique for the
//   baseline tier);
// * a **marker** floor that answers a distinctive small integer for a non-small
//   operand or a non-representable result, so the helper's involvement shows up in
//   the returned **value** as well as in the call count.

/// The BIF floor the arithmetic fixtures run against, as a stateless `fn` pointer so
/// the same floor can be handed to several runs.
type ArithHook = fn(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError>;

/// The marker the marker floor answers with when an operand is not a small integer
/// (a float, a bignum, an atom, …) — a value the inlined fast path's guard makes it
/// impossible for that path to produce for these fixtures.
const NON_SMALL_MARKER: i64 = 10_000_000;

/// The marker the marker floor answers with when the operands are small integers but
/// the result is outside `MIN_SMALL..=MAX_SMALL` — a value the inlined fast path's
/// range check makes it impossible for that path to produce.
const OVERFLOW_MARKER: i64 = 10_000_001;

/// `erlang:'+'/2` / `'-'/2` over small integers, with the engine's `BadArith` for a
/// non-small operand or a result outside the small range — the range-checked floor
/// the sweeps run against (the same shape `jit.rs` uses for the baseline tier).
fn checked_arith(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match small_result(a, b, function) {
        Some(value) => Ok(make_small(value)),
        None => Err(BeamError::BadArith),
    }
}

/// The in-range result of `a <op> b`, or `None` when it is not a small integer.
fn small_result(a: i64, b: i64, function: &str) -> Option<i64> {
    let result = match function {
        "+" => a.checked_add(b),
        "-" => a.checked_sub(b),
        "*" => a.checked_mul(b),
        _ => return None,
    }?;
    (MIN_SMALL..=MAX_SMALL).contains(&result).then_some(result)
}

/// The marker floor: [`checked_arith`], except that the two cases the inlined fast
/// path must have **refused** answer with a distinctive marker instead of raising —
/// so the helper's involvement is visible in the returned value.
///
/// A non-small operand answers [`NON_SMALL_MARKER`] (the fast path's operand guard
/// rejects it, so only the helper can reach the floor with it), and an
/// out-of-range result answers [`OVERFLOW_MARKER`] (the fast path's range check
/// rejects it). A call that raises `BadArith` for a non-small operand would agree
/// between the tiers just as well, but a marker makes the *deopt* a positive
/// observation rather than an inference from matching errors.
fn marker_arith(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if function != "+" && function != "-" && function != "*" {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Ok(make_small(NON_SMALL_MARKER));
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match small_result(a, b, function) {
        Some(value) => Ok(make_small(value)),
        None => Ok(make_small(OVERFLOW_MARKER)),
    }
}

/// The marker floor behind a call counter: exactly the [`marker_arith`] behaviour,
/// with every call counted, so a test can tell "the fast path computed this" (zero
/// calls) from "the guard rejected this and the helper ran the BIF" (one call).
fn counting_marker_arith(
    calls: Arc<AtomicUsize>,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static {
    move |module, function, arity, args| {
        calls.fetch_add(1, Ordering::SeqCst);
        marker_arith(module, function, arity, args)
    }
}

/// One operand of a hand-built `gc_bif` fixture.
#[derive(Clone, Copy)]
enum ArithSpec {
    /// The x-register `n` — a register operand the fast path can inline.
    X(u32),
    /// An integer constant operand (`{integer,V}`): an inlinable **small-int**
    /// constant when `V` is inside the 60-bit range, otherwise the bignum constant
    /// the fast path refuses.
    Int(i64),
    /// A float-register operand (`z,2`) — never a small-int constant, so the fast
    /// path refuses it.
    FloatReg(u32),
    /// A static-literal operand (`z,4`) — never a small-int constant either.
    Literal(u32),
    /// A`Y` (stack slot) operand — neither an x-register nor a constant.
    Y(u32),
    /// An atom constant operand, interned in the fixture module's atom table.
    Atom(&'static str),
    /// The `nil` constant operand.
    Nil,
}

impl ArithSpec {
    /// The loader operand this spec stands for, interning an atom name in `atoms`.
    fn operand(self, atoms: &mut beam_term::AtomTable) -> Operand {
        match self {
            ArithSpec::X(n) => Operand::X(n),
            ArithSpec::Int(value) => Operand::Integer(value),
            ArithSpec::FloatReg(n) => Operand::FloatReg(n),
            ArithSpec::Literal(index) => Operand::Literal(index),
            ArithSpec::Y(n) => Operand::Y(n),
            ArithSpec::Atom(name) => Operand::Atom(atoms.intern(name)),
            ArithSpec::Nil => Operand::Nil,
        }
    }
}

/// One hand-built exported function of an arithmetic fixture:
///
/// ```text
///   label E:  gc_bif M:F/<args.len()> [<args>] -> <dst>
///             [move <dst> -> x(0)]        ; only when `tail_move`
///             return
/// ```
struct ArithFn {
    /// The exported name.
    name: &'static str,
    /// The arity of the *exported* function (the number of x-register arguments the
    /// fixture reads).
    arity: u32,
    /// The imported module name (`"erlang"` for every fast-path-eligible shape).
    module: &'static str,
    /// The imported function name (`"+"` / `"-"` / anything else).
    bif: &'static str,
    /// The argument operands, in `gc_bif` argument order.
    args: Vec<ArithSpec>,
    /// The destination operand.
    dst: ArithSpec,
    /// Whether the fixture appends `move <dst> -> x(0)`, so a fixture whose
    /// destination is **not** `x(0)` still answers with its result.
    tail_move: bool,
}

impl ArithFn {
    /// A fixture named `name`, exported at `arity`, running `module:bif/`[`args.len()`]
    /// over `args` into `dst`.
    fn new(
        name: &'static str,
        arity: u32,
        module: &'static str,
        bif: &'static str,
        args: &[ArithSpec],
        dst: ArithSpec,
    ) -> ArithFn {
        ArithFn {
            name,
            arity,
            module,
            bif,
            args: args.to_vec(),
            dst,
            tail_move: false,
        }
    }

    /// Append `move <dst> -> x(0)` before the `return`, so the fixture's answer is
    /// the `gc_bif`'s result even when its destination is not `x(0)`.
    fn moving_the_result_to_x0(mut self) -> ArithFn {
        self.tail_move = true;
        self
    }
}

/// Build a hand-built module (the loader is not involved) whose exported functions
/// are exactly `functions`, each one basic block, against a fresh `erlang` import per
/// function (a function's `ImportRef` therefore indexes its own import).
fn arith_module(name: &str, functions: &[ArithFn]) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);

    let mut next_label = 1u32;
    for fixture in functions {
        let export = module.atom_table.intern(fixture.name);
        let module_atom = module.atom_table.intern(fixture.module);
        let function_atom = module.atom_table.intern(fixture.bif);
        module.imports.push(Import {
            module: module_atom,
            function: function_atom,
            arity: fixture.args.len() as u32,
        });
        let bif = ImportRef((module.imports.len() - 1) as u32);

        let label = Label(next_label);
        next_label += 1;
        let mut code = vec![
            Instr::Label(label),
            Instr::GcBif {
                arity: fixture.args.len() as u8,
                bif,
                fail: NO_FAIL,
                live: fixture.arity,
                args: fixture
                    .args
                    .iter()
                    .map(|spec| spec.operand(&mut module.atom_table))
                    .collect(),
                dst: fixture.dst.operand(&mut module.atom_table),
            },
        ];
        if fixture.tail_move {
            code.push(Instr::Move {
                src: fixture.dst.operand(&mut module.atom_table),
                dst: Operand::X(0),
            });
        }
        code.push(Instr::Return);

        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: label,
            entry_label: label,
            module: module.name,
            name: export,
            arity: fixture.arity,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: export,
            arity: fixture.arity,
            label,
        });
    }
    module
}

/// The fast-path fixtures: every shape the tier inlines — the three BIFs, an
/// x-register operand, a small-int constant operand on either side, and a
/// destination that is not `x(0)`.
fn arith_fixtures() -> Module {
    arith_module(
        "arith",
        &[
            ArithFn::new(
                "plus_regs",
                2,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "minus_regs",
                2,
                "erlang",
                "-",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "plus_const",
                1,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::Int(7)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "minus_const",
                1,
                "erlang",
                "-",
                &[ArithSpec::Int(1000), ArithSpec::X(0)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "mul_regs",
                2,
                "erlang",
                "*",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "mul_const",
                1,
                "erlang",
                "*",
                &[ArithSpec::X(0), ArithSpec::Int(6)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "plus_dst_two",
                2,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(2),
            )
            .moving_the_result_to_x0(),
        ],
    )
}

/// The shapes the tier must **not** inline — one per clause of the fast path's
/// shape test: another BIF, a bignum constant, a float register, a static literal, a
/// `Y` destination, a non-2 arity, and the right BIF name in the wrong module.
fn non_inlinable_arith_module() -> Module {
    arith_module(
        "arith_helper",
        &[
            ArithFn::new(
                "divide",
                2,
                "erlang",
                "div",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "plus_bigconst",
                1,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::Int(i64::MAX)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "plus_floatreg",
                1,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::FloatReg(0)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "plus_literal",
                1,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::Literal(0)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "plus_y_dst",
                1,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::Int(1)],
                ArithSpec::Y(0),
            ),
            ArithFn::new(
                "plus_arity3",
                1,
                "erlang",
                "+",
                &[ArithSpec::X(0), ArithSpec::Int(1), ArithSpec::Int(2)],
                ArithSpec::X(0),
            ),
            ArithFn::new(
                "plus_foreign",
                2,
                "mymod",
                "+",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            ),
        ],
    )
}

/// Run `export/arity` of `module` on `tier` with `hook` as the BIF floor.
fn run_hooked<H>(
    tier: Tier,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    hook: H,
) -> Result<Eterm, BeamError>
where
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
{
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(hook));
    machine.run_with_tier(module, atom(module, export), arity, args, &mut ctx, tier)
}

/// Run `export/arity` of `module` through the **Cranelift tier** on a caller-owned
/// compiled artifact (compile-once / run-many) with `hook` as the BIF floor.
fn run_compiled_hooked<H>(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    hook: H,
) -> Result<Eterm, BeamError>
where
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
{
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(hook));
    backend.run_compiled(
        compiled,
        &mut machine,
        module,
        atom(module, export),
        arity,
        args,
        &mut ctx,
    )
}

/// Assert two outcomes agree: the same value, or the same typed error.
fn assert_same(got: &Result<Eterm, BeamError>, interpreted: &Result<Eterm, BeamError>, what: &str) {
    match (got, interpreted) {
        (Ok(got), Ok(interpreted)) => {
            assert_eq!(got, interpreted, "{what} differs from the interpreter")
        }
        (Err(got), Err(interpreted)) => assert_eq!(
            format!("{got:?}"),
            format!("{interpreted:?}"),
            "{what} disagrees with the interpreter on the typed error"
        ),
        _ => panic!("{what}: got {got:?}, the interpreter produced {interpreted:?}"),
    }
}

/// Assert the interpreter, the baseline tier and the Cranelift tier (through a
/// caller-owned compiled artifact) all produce the same outcome for `export/arity`
/// under the same BIF floor `hook`, and return that shared outcome.
fn agree_arith(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    hook: ArithHook,
) -> Result<Eterm, BeamError> {
    let interpreted = run_hooked(Tier::Interpreter, module, export, arity, args, hook);
    let baseline = run_hooked(Tier::BaselineJit, module, export, arity, args, hook);
    let cranelift = run_compiled_hooked(backend, compiled, module, export, arity, args, hook);
    assert_same(&baseline, &interpreted, &format!("baseline {export}/{arity}"));
    assert_same(&cranelift, &interpreted, &format!("cranelift {export}/{arity}"));
    interpreted
}

#[test]
fn the_tier_reports_which_gc_bifs_it_inlined() {
    // Artifact-level evidence that the arithmetic fast path — not `jit_step_one` —
    // was *compiled*: a module that silently deopted would name no op here.
    let compiled = CraneliftJit::new()
        .compile(&arith_fixtures())
        .expect("compile the arithmetic fixtures");
    let ops = compiled.inlined_gc_bif_ops();
    for op in [SmallArith::Add, SmallArith::Sub, SmallArith::Mul] {
        assert!(
            ops.contains(&op),
            "{} must take the inline path, got {ops:?}",
            op.name()
        );
    }
    assert_eq!(
        ops.len(),
        3,
        "only the three `+`/`-`/`*` ops are inlined: {ops:?}"
    );
    // The inlined `gc_bif` addresses x-registers natively, and so does the `move` of
    // the `plus_dst_two` fixture, so the driver's native-entry bound guard must see
    // them (the same introspection the `test`/`move` fast paths report).
    assert!(compiled.highest_native_register().is_some());

    // The negative module holds one fixture per clause of the shape test: none of
    // them may be inlined, and neither may the module whose only `gc_bif` is the
    // `NO_FAIL` test fixture (which contains no `gc_bif` at all).
    let compiled = CraneliftJit::new()
        .compile(&non_inlinable_arith_module())
        .expect("compile the non-inlinable fixtures");
    assert!(
        compiled.inlined_gc_bif_ops().is_empty(),
        "no non-inlinable shape may take the fast path, got {:?}",
        compiled.inlined_gc_bif_ops()
    );
    let compiled = CraneliftJit::new()
        .compile(&comparisons_module())
        .expect("compile the comparison fixtures");
    assert!(compiled.inlined_gc_bif_ops().is_empty());
}

#[test]
fn the_inlined_arith_fast_path_never_consults_the_bif_hook() {
    // Behavioural proof, not an inference: for the same program the interpreter steps
    // the BIF exactly once, so the Cranelift tier's *zero* calls can only mean the
    // inlined fast path computed the tagged word itself.
    let module = arith_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the arithmetic fixtures");
    assert!(compiled.inlined_gc_bif_ops().contains(&SmallArith::Add));

    let args = [make_small(19), make_small(23)];

    let interpreted_calls = Arc::new(AtomicUsize::new(0));
    let interpreted = run_hooked(
        Tier::Interpreter,
        &module,
        "plus_regs",
        2,
        &args,
        counting_marker_arith(Arc::clone(&interpreted_calls)),
    )
    .expect("plus_regs(19, 23) via the interpreter");
    assert_eq!(interpreted, make_small(42));
    assert_eq!(
        interpreted_calls.load(Ordering::SeqCst),
        1,
        "the interpreter must call the BIF exactly once"
    );

    let cranelift_calls = Arc::new(AtomicUsize::new(0));
    let jitted = run_compiled_hooked(
        &backend,
        &compiled,
        &module,
        "plus_regs",
        2,
        &args,
        counting_marker_arith(Arc::clone(&cranelift_calls)),
    )
    .expect("plus_regs(19, 23) via the Cranelift tier");
    assert_eq!(jitted, interpreted);
    assert_eq!(
        cranelift_calls.load(Ordering::SeqCst),
        0,
        "the inlined fast path must not call the BIF"
    );
}

#[test]
fn the_inlined_arith_inlines_the_small_int_extremes_and_falls_back_on_overflow() {
    let module = arith_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the arithmetic fixtures");
    assert!(compiled.inlined_gc_bif_ops().contains(&SmallArith::Add));

    // Both boundaries and their immediate neighbours are representable, so the
    // inlined range check must **accept** them and compute the result natively — the
    // BIF floor must never be consulted.
    for (a, b) in [
        (MAX_SMALL, 0i64),
        (MIN_SMALL, 0),
        (MAX_SMALL, -1),
        (MIN_SMALL, 1),
        (MAX_SMALL - 1, 1),
    ] {
        let args = [make_small(a), make_small(b)];
        let calls = Arc::new(AtomicUsize::new(0));
        let got = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            "plus_regs",
            2,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("plus_regs({a}, {b}) failed: {err:?}"));
        assert_eq!(got, make_small(a + b), "plus_regs({a}, {b})");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the fast path must compute {a} + {b} itself"
        );
    }

    // One past either boundary is not representable as a tagged small word, so the
    // inlined range check must **reject** it: the helper runs the BIF, which is the
    // only way the floor is consulted at all. (The floor is the marker one, so a fast
    // path that skipped its range check would have to return `OVERFLOW_MARKER` to
    // agree — and it cannot reach the floor to learn it.)
    for (export, op, a, b) in [
        ("plus_regs", "+", MAX_SMALL, 1i64),
        ("plus_regs", "+", MIN_SMALL, -1),
        ("plus_regs", "+", MAX_SMALL, MAX_SMALL),
        ("plus_regs", "+", MIN_SMALL, MIN_SMALL),
        ("minus_regs", "-", MIN_SMALL, 1),
        ("minus_regs", "-", MAX_SMALL, -1),
    ] {
        let args = [make_small(a), make_small(b)];
        assert!(
            small_result(a, b, op).is_none(),
            "{a} {op} {b} is meant to overflow the small range"
        );

        let interpreted = run_hooked(Tier::Interpreter, &module, export, 2, &args, marker_arith)
            .unwrap_or_else(|err| panic!("interpreter {export}({a}, {b}) failed: {err:?}"));
        assert_eq!(
            interpreted,
            make_small(OVERFLOW_MARKER),
            "the marker floor must have stepped the overflowing BIF"
        );

        let calls = Arc::new(AtomicUsize::new(0));
        let jitted = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            export,
            2,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("cranelift {export}({a}, {b}) failed: {err:?}"));
        assert_eq!(jitted, interpreted, "{export}({a}, {b}) on the Cranelift tier");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the overflowing {export}({a}, {b}) must fall back to the BIF floor"
        );
    }
}

#[test]
fn a_float_or_bignum_operand_deopts_and_yields_the_interpreters_result() {
    // The operand guard is a *run-time* test, so the same instruction can inline one
    // moment and deopt the next. These cases force the deopt: a boxed float, a
    // bignum, an atom and `[]` — none of which can be a tagged small word — while the
    // artifact proves the very same instruction was compiled with the inline path.
    let module = arith_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the arithmetic fixtures");
    assert!(compiled.inlined_gc_bif_ops().contains(&SmallArith::Add));

    // The arena owns the heap these two boxed terms live on; it outlives every run.
    let mut arena = MinimalContext::new();
    let float = terms::make_float(&mut arena, 4.0).expect("the arena allocates a float");
    let bignum = terms::make_integer(&mut arena, i64::MAX).expect("the arena allocates a bignum");
    let an_atom = make_atom(atom(&module, "erlang"));

    for bad in [float, bignum, an_atom, NIL] {
        let args = [bad, make_small(1)];

        // Only the helper can produce the non-small marker, so its value in the
        // result is the positive observation that the guard rejected the operand.
        let interpreted = run_hooked(
            Tier::Interpreter,
            &module,
            "plus_regs",
            2,
            &args,
            marker_arith,
        )
        .unwrap_or_else(|err| panic!("interpreter plus_regs({bad:?}, 1) failed: {err:?}"));
        assert_eq!(interpreted, make_small(NON_SMALL_MARKER));

        let calls = Arc::new(AtomicUsize::new(0));
        let jitted = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            "plus_regs",
            2,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("cranelift plus_regs({bad:?}, 1) failed: {err:?}"));
        assert_eq!(jitted, interpreted, "plus_regs({bad:?}, 1) on either tier");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the rejected operand must fall back to the BIF floor"
        );
    }
}

#[test]
fn the_inlined_arith_agrees_with_the_interpreter_over_a_wide_range() {
    // A dense differential sweep: every pair from a value set clustered at 0, at the
    // 8-bit/kernel-ish magnitudes and at both ends of the small-int range must agree
    // with the interpreter *and* with an independently computed oracle — including
    // every pair whose sum/difference is not a small integer, which must be the
    // floor's typed `BadArith` on every tier.
    let module = arith_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the arithmetic fixtures");

    let values = [
        MIN_SMALL,
        MIN_SMALL + 1,
        -(1i64 << 30),
        -16,
        -1,
        0,
        1,
        16,
        1i64 << 30,
        MAX_SMALL - 2,
        MAX_SMALL,
    ];
    for a in values {
        for b in values {
            let args = [make_small(a), make_small(b)];
            for (export, op) in [("plus_regs", "+"), ("minus_regs", "-")] {
                let got = agree_arith(
                    &backend,
                    &compiled,
                    &module,
                    export,
                    2,
                    &args,
                    checked_arith,
                );
                match (got, small_result(a, b, op)) {
                    (Ok(got), Some(expected)) => {
                        assert_eq!(got, make_small(expected), "{export}({a}, {b})")
                    }
                    (Err(_), None) => {}
                    (got, expected) => panic!("{export}({a}, {b}): {got:?} vs {expected:?}"),
                }
            }
        }
    }

    // The small-int-constant forms (a constant operand on either side) and the
    // non-`x(0)` destination, over the same value set.
    for a in values {
        let arg = [make_small(a)];
        for (export, expected) in [
            ("plus_const", small_result(a, 7, "+")),
            ("minus_const", small_result(1000, a, "-")),
        ] {
            let got = agree_arith(&backend, &compiled, &module, export, 1, &arg, checked_arith);
            match (got, expected) {
                (Ok(got), Some(expected)) => {
                    assert_eq!(got, make_small(expected), "{export}({a})")
                }
                (Err(_), None) => {}
                (got, expected) => panic!("{export}({a}): {got:?} vs {expected:?}"),
            }
        }
    }
    for a in values {
        for b in values {
            let args = [make_small(a), make_small(b)];
            let got = agree_arith(
                &backend,
                &compiled,
                &module,
                "plus_dst_two",
                2,
                &args,
                checked_arith,
            );
            match (got, small_result(a, b, "+")) {
                (Ok(got), Some(expected)) => {
                    assert_eq!(got, make_small(expected), "plus_dst_two({a}, {b})")
                }
                (Err(_), None) => {}
                (got, expected) => panic!("plus_dst_two({a}, {b}): {got:?} vs {expected:?}"),
            }
        }
    }
}

#[test]
fn non_inlinable_gc_bif_shapes_stay_on_the_helper() {
    // Each fixture violates exactly one clause of the fast path's shape test, so the
    // artifact must inline no op for this module — and the shapes must still run, with
    // the helper's *exact* semantics (the interpreter's answer on every tier).
    let module = non_inlinable_arith_module();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the non-inlinable fixtures");
    assert!(
        compiled.inlined_gc_bif_ops().is_empty(),
        "no non-inlinable shape may take the fast path, got {:?}",
        compiled.inlined_gc_bif_ops()
    );

    // Another BIF: the floor raises `BadArg`, on every tier.
    let got = agree_arith(
        &backend,
        &compiled,
        &module,
        "divide",
        2,
        &[make_small(6), make_small(7)],
        marker_arith,
    );
    assert!(matches!(got, Err(BeamError::BadArg)), "got {got:?}");

    // A bignum **constant** operand: the fast path refuses the shape, and the
    // helper's operand reader materialises the bignum, which the floor marks.
    let got = agree_arith(
        &backend,
        &compiled,
        &module,
        "plus_bigconst",
        1,
        &[make_small(2)],
        marker_arith,
    );
    assert_eq!(got.ok(), Some(make_small(NON_SMALL_MARKER)), "plus_bigconst");

    // A float-register operand: likewise refused at compile time; the register file
    // is `NIL`-filled, so the floor sees a non-small operand.
    let got = agree_arith(
        &backend,
        &compiled,
        &module,
        "plus_floatreg",
        1,
        &[make_small(2)],
        marker_arith,
    );
    assert_eq!(got.ok(), Some(make_small(NON_SMALL_MARKER)), "plus_floatreg");

    // A static-literal operand the module has no literal area for: the helper's
    // operand read is the engine's typed error, and both tiers report it alike
    // (never a panic, never a fabricated result).
    let got = agree_arith(
        &backend,
        &compiled,
        &module,
        "plus_literal",
        1,
        &[make_small(2)],
        marker_arith,
    );
    assert!(got.is_err(), "an unresolvable literal must be a typed error");

    // Three operands: the fast path is arity-2 only, so the helper calls the BIF
    // with three arguments — which the floor rejects.
    let got = agree_arith(
        &backend,
        &compiled,
        &module,
        "plus_arity3",
        1,
        &[make_small(2)],
        marker_arith,
    );
    assert!(matches!(got, Err(BeamError::BadArg)), "got {got:?}");

    // The right BIF name in the wrong module: only `erlang` is recognised.
    let got = agree_arith(
        &backend,
        &compiled,
        &module,
        "plus_foreign",
        2,
        &[make_small(6), make_small(7)],
        marker_arith,
    );
    assert!(matches!(got, Err(BeamError::BadArg)), "got {got:?}");
}

// ---------------------------------------------------------------------------
// The inlined `'*'/2` fast path (`src/jit/cranelift/`)
// ---------------------------------------------------------------------------
//
// `SmallArith::Mul` is the third BIF the small-int fast path lowers. The product is
// computed with an `imul`/`smulhi` pair whose explicit overflow check
// (`high_half == low_half >> 63`) is **ANDed into** the tagged-range guard, so a
// wrapped 64-bit product can never slip past the range test — a rejection can only
// ever deopt to the helper, whose interpreter step is the BIF's exact semantics. The
// fixtures and the counting/marker floors are the same as for `+`/`-`, so "the fast
// path computed this" (zero BIF calls) and "the guard rejected this and the helper
// computed it" (exactly one call) are observable rather than inferred.

#[test]
fn the_inlined_mul_agrees_and_never_consults_the_bif_hook() {
    let module = arith_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the arithmetic fixtures");
    assert!(compiled.inlined_gc_bif_ops().contains(&SmallArith::Mul));

    // In-range small products: the tagged word must be computed natively, so the
    // counting floor must record *zero* calls.
    for (a, b) in [
        (6i64, 7i64),
        (-6, 7),
        (0, 12_345),
        (1, -9_999),
        (MAX_SMALL, 1),
        (1_234, -5_678),
        (MAX_SMALL, 0),
    ] {
        assert!(
            small_result(a, b, "*").is_some(),
            "{a} * {b} must be a small integer"
        );
        let args = [make_small(a), make_small(b)];
        let calls = Arc::new(AtomicUsize::new(0));
        let got = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            "mul_regs",
            2,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("mul_regs({a}, {b}) failed: {err:?}"));
        assert_eq!(got, make_small(a * b), "mul_regs({a}, {b})");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "the fast path must compute {a} * {b} itself"
        );
    }

    // The small-int-constant form (`mul_const` is `x0 * 6`).
    for a in [1i64, 2, -3, 0, 1_000, MAX_SMALL / 6] {
        let args = [make_small(a)];
        let calls = Arc::new(AtomicUsize::new(0));
        let got = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            "mul_const",
            1,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("mul_const({a}) failed: {err:?}"));
        assert_eq!(got, make_small(a * 6), "mul_const({a})");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "mul_const({a}) must inline the product"
        );
    }
}

#[test]
fn the_inlined_mul_deopts_on_overflow_and_non_small_operands() {
    let module = arith_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the arithmetic fixtures");
    assert!(compiled.inlined_gc_bif_ops().contains(&SmallArith::Mul));

    // Products that leave the small range — including two that overflow a 64-bit word
    // (whose *unchecked* `imul` would wrap) — must fail the guard's range/overflow test
    // and fall back to the BIF floor, which answers the overflow marker.
    for (a, b) in [
        (MAX_SMALL, 2i64),
        (MIN_SMALL, 2),
        (MIN_SMALL, -1),
        (1i64 << 40, 1i64 << 40),
    ] {
        assert!(
            small_result(a, b, "*").is_none(),
            "{a} * {b} is meant to leave the small range"
        );
        let args = [make_small(a), make_small(b)];

        let interpreted =
            run_hooked(Tier::Interpreter, &module, "mul_regs", 2, &args, marker_arith)
                .unwrap_or_else(|err| panic!("interpreter mul_regs({a}, {b}) failed: {err:?}"));
        assert_eq!(interpreted, make_small(OVERFLOW_MARKER));

        let calls = Arc::new(AtomicUsize::new(0));
        let jitted = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            "mul_regs",
            2,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("cranelift mul_regs({a}, {b}) failed: {err:?}"));
        assert_eq!(jitted, interpreted, "mul_regs({a}, {b}) on the Cranelift tier");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the overflowing mul_regs({a}, {b}) must fall back to the BIF floor"
        );
    }

    // A boxed / non-small operand (a float, a bignum, a non-`erlang` atom, `[]`): the
    // operand guard rejects it before any arithmetic, so only the helper can reach the
    // floor — and the floor's marker shows up in the returned value.
    let mut arena = MinimalContext::new();
    let float = terms::make_float(&mut arena, 4.0).expect("the arena allocates a float");
    let bignum = terms::make_integer(&mut arena, i64::MAX).expect("the arena allocates a bignum");
    let an_atom = make_atom(atom(&module, "erlang"));
    for bad in [float, bignum, an_atom, NIL] {
        let args = [bad, make_small(1)];

        let interpreted =
            run_hooked(Tier::Interpreter, &module, "mul_regs", 2, &args, marker_arith)
                .unwrap_or_else(|err| panic!("interpreter mul_regs({bad:?}, 1) failed: {err:?}"));
        assert_eq!(interpreted, make_small(NON_SMALL_MARKER));

        let calls = Arc::new(AtomicUsize::new(0));
        let jitted = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            "mul_regs",
            2,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("cranelift mul_regs({bad:?}, 1) failed: {err:?}"));
        assert_eq!(jitted, interpreted, "mul_regs({bad:?}, 1) on either tier");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the rejected operand must fall back to the BIF floor"
        );
    }

    // The `0`/`1` identities are ordinary in-range products, so they stay on the fast
    // path (zero calls).
    for (a, b) in [(0i64, 777i64), (1, 777), (777, 0), (777, 1)] {
        let args = [make_small(a), make_small(b)];
        let calls = Arc::new(AtomicUsize::new(0));
        let got = run_compiled_hooked(
            &backend,
            &compiled,
            &module,
            "mul_regs",
            2,
            &args,
            counting_marker_arith(Arc::clone(&calls)),
        )
        .unwrap_or_else(|err| panic!("mul_regs({a}, {b}) failed: {err:?}"));
        assert_eq!(got, make_small(a * b), "mul_regs({a}, {b})");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "mul_regs({a}, {b}) must inline the product"
        );
    }
}

#[test]
fn the_inlined_mul_agrees_with_the_interpreter_over_a_wide_range() {
    // The same dense differential sweep as the `+`/`-` fast path, over the same value
    // set: every product that is a small integer must agree with the interpreter *and*
    // with an independently computed oracle, and every product that is not (including
    // the ones that overflow a 64-bit word) must be the floor's typed `BadArith` on
    // every tier.
    let module = arith_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the arithmetic fixtures");

    let values = [
        MIN_SMALL,
        MIN_SMALL + 1,
        -(1i64 << 30),
        -16,
        -1,
        0,
        1,
        16,
        1i64 << 30,
        MAX_SMALL - 2,
        MAX_SMALL,
    ];
    for a in values {
        for b in values {
            let args = [make_small(a), make_small(b)];
            let got = agree_arith(
                &backend,
                &compiled,
                &module,
                "mul_regs",
                2,
                &args,
                checked_arith,
            );
            match (got, small_result(a, b, "*")) {
                (Ok(got), Some(expected)) => {
                    assert_eq!(got, make_small(expected), "mul_regs({a}, {b})")
                }
                (Err(_), None) => {}
                (got, expected) => panic!("mul_regs({a}, {b}): {got:?} vs {expected:?}"),
            }
        }
    }

    // The small-int-constant form, over the same value set.
    for a in values {
        let arg = [make_small(a)];
        let got = agree_arith(&backend, &compiled, &module, "mul_const", 1, &arg, checked_arith);
        match (got, small_result(a, 6, "*")) {
            (Ok(got), Some(expected)) => assert_eq!(got, make_small(expected), "mul_const({a})"),
            (Err(_), None) => {}
            (got, expected) => panic!("mul_const({a}): {got:?} vs {expected:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// The inlined comparison BIFs (`erlang:'=:='/2` … `erlang:'>='/2`)
// (`src/jit/cranelift/compare.rs`)
// ---------------------------------------------------------------------------
//
// The compiler emits every comparison as a **non-collecting** `{bif,Op,{f,0},Args,Dst}`
// (`fail = 0`), so leaving one to `jit_step_one` costs a helper round-trip per clause
// guard. The tier's fast path is a *shape* test (never a value test): load the two
// operand words, guard the **register** operand(s) as untagged small ints — the only
// class whose tagged-word signed order is `cmp_arith`'s value order, `make_small` being
// injective and monotone — `cmp`/`jcc`, and store the module's own `true`/`false` atom
// into the destination. `=:=`/`=/=` are that word compare; `==`/`/=` and the four
// orderings are the same compare behind the small-int guard(s).
//
// The fast path is entered only when the call is `erlang:<one of the eight>/2` with two
// addressable x-register/immediate operands, an ordering operator's operands are both
// *small-int* (an atom/`nil` constant is not word-ordered), the destination is an
// x-register below `MAX_REGISTER`, and the module's atom table carries both `true` and
// `false`. Everything else — and every run-time guard rejection — deopts to the shared
// `jit_step_one`, whose interpreter step is the BIF's exact semantics.
//
// Each case below is pinned three ways: the interpreter's answer comes from an installed
// `beam_term`-ordering floor (so the tier is compared against the real comparison order,
// not a reimplementation), the artifact's `inlined_comparison_ops()` proves the fast
// path was **compiled** rather than silently deopted, and a *counting* floor makes "the
// fast path computed this" (zero calls) vs "the guard rejected this and the helper ran
// the BIF" (exactly one call) a positive observation.

/// The eight comparison operators with the three fixture names each one uses: the
/// two-x-register form, the `X(0) <op> 5` form, and the `5 <op> X(0)` form.
const COMPARISONS: [(&str, &str, &str, &str); 8] = [
    ("=:=", "exact_eq", "exact_eq_const", "exact_eq_const_lhs"),
    ("=/=", "exact_ne", "exact_ne_const", "exact_ne_const_lhs"),
    ("==", "arith_eq", "arith_eq_const", "arith_eq_const_lhs"),
    ("/=", "arith_ne", "arith_ne_const", "arith_ne_const_lhs"),
    ("<", "lt", "lt_const", "lt_const_lhs"),
    ("=<", "le", "le_const", "le_const_lhs"),
    (">", "gt", "gt_const", "gt_const_lhs"),
    (">=", "ge", "ge_const", "ge_const_lhs"),
];

/// The oracle answer of `a <op> b` for two small integers (where `=:=` and `==`, and
/// `=/=` and `/=`, coincide).
fn comparison_oracle(op: &str, a: i64, b: i64) -> bool {
    match op {
        "=:=" | "==" => a == b,
        "=/=" | "/=" => a != b,
        "<" => a < b,
        "=<" => a <= b,
        ">" => a > b,
        ">=" => a >= b,
        other => panic!("unknown comparison operator {other:?}"),
    }
}

/// The fixture module's own `true`/`false` atom as a term — a comparison BIF answers
/// with an **atom**, not a small integer.
fn boolean(module: &Module, answer: bool) -> Eterm {
    make_atom(atom(module, if answer { "true" } else { "false" }))
}

/// A name resolved in a cloned atom table (see [`comparison_floor`]).
fn atom_of(atoms: &beam_term::AtomTable, name: &str) -> Atom {
    atoms
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// The ground-truth floor of the eight comparison BIFs: `beam_term`'s own exact /
/// arithmetic ordering against a clone of the fixture module's atom table. The
/// interpreter's answer therefore comes from the *real* ordering, so the tier's word
/// compare is checked against it rather than against a second implementation of it.
fn comparison_floor(
    atoms: beam_term::AtomTable,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static {
    move |module, function, arity, args| {
        if module != "erlang" || arity != 2 || args.len() != 2 {
            return Err(BeamError::BadArg);
        }
        let (a, b) = (args[0], args[1]);
        let answer = match function {
            "=:=" => beam_term::cmp(a, b, &atoms).is_eq(),
            "=/=" => !beam_term::cmp(a, b, &atoms).is_eq(),
            "==" => beam_term::cmp_arith(a, b, &atoms).is_eq(),
            "/=" => !beam_term::cmp_arith(a, b, &atoms).is_eq(),
            "<" => beam_term::cmp_arith(a, b, &atoms).is_lt(),
            "=<" => !beam_term::cmp_arith(a, b, &atoms).is_gt(),
            ">" => beam_term::cmp_arith(a, b, &atoms).is_gt(),
            ">=" => !beam_term::cmp_arith(a, b, &atoms).is_lt(),
            _ => return Err(BeamError::BadArg),
        };
        let name = if answer { "true" } else { "false" };
        Ok(make_atom(atom_of(&atoms, name)))
    }
}

/// [`comparison_floor`] behind a call counter: the same behaviour, with every call
/// counted, so a test can tell "the fast path computed this" (zero calls) from "the
/// guard rejected this and the helper ran the BIF" (one call).
fn counting_comparison_floor(
    atoms: beam_term::AtomTable,
    calls: Arc<AtomicUsize>,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static {
    let mut floor = comparison_floor(atoms);
    move |module, function, arity, args| {
        calls.fetch_add(1, Ordering::SeqCst);
        floor(module, function, arity, args)
    }
}

/// One hand-built exported function of a comparison fixture:
///
/// ```text
///   label E:  {bif|gc_bif} M:F/2 [<args>] -> <dst>
///             [move <dst> -> x(0)]        ; only when `tail_move`
///             return
/// ```
struct CmpFn {
    /// The exported name.
    name: &'static str,
    /// The arity of the *exported* function (the number of x-register arguments it
    /// reads) — never the BIF's own arity, which is always 2 here.
    arity: u32,
    /// The imported module name (`"erlang"` for every fast-path-eligible shape).
    module: &'static str,
    /// The imported function name (one of [`COMPARISONS`] for the positive shapes).
    bif: &'static str,
    /// The argument operands, in BIF argument order.
    args: Vec<ArithSpec>,
    /// The destination operand.
    dst: ArithSpec,
    /// Whether the call is spelled `gc_bif` (the compiler emits `bif` for comparisons,
    /// but the tier inlines both variants).
    gc: bool,
    /// An instruction arity that differs from the import's (a hand-built shape the tier
    /// must refuse), when set.
    instr_arity: Option<u8>,
    /// Whether the fixture appends `move <dst> -> x(0)`, so a fixture whose destination
    /// is **not** `x(0)` still answers with its result.
    tail_move: bool,
}

impl CmpFn {
    /// A `bif` fixture named `name`, exported at `arity`, calling `erlang:<bif>/2` over
    /// `args` into `dst`.
    fn new(
        name: &'static str,
        arity: u32,
        bif: &'static str,
        args: &[ArithSpec],
        dst: ArithSpec,
    ) -> CmpFn {
        CmpFn {
            name,
            arity,
            module: "erlang",
            bif,
            args: args.to_vec(),
            dst,
            gc: false,
            instr_arity: None,
            tail_move: false,
        }
    }

    /// Spell the call `gc_bif` instead of `bif`.
    fn gc(mut self) -> CmpFn {
        self.gc = true;
        self
    }

    /// Call `<module>:<bif>/2` instead of `erlang:<bif>/2`.
    fn in_module(mut self, module: &'static str) -> CmpFn {
        self.module = module;
        self
    }

    /// Emit an instruction arity that differs from the import's.
    fn with_instr_arity(mut self, arity: u8) -> CmpFn {
        self.instr_arity = Some(arity);
        self
    }

    /// Append `move <dst> -> x(0)` before the `return`.
    fn moving_the_result_to_x0(mut self) -> CmpFn {
        self.tail_move = true;
        self
    }
}

/// Build a hand-built module (the loader is not involved) whose exported functions are
/// exactly `fixtures`, against a fresh import per function — with `true`/`false`
/// interned.
fn comparison_bif_module(name: &str, fixtures: &[CmpFn]) -> Module {
    comparison_bif_module_with_atoms(name, fixtures, true)
}

/// A comparison fixture whose atom table does **not** carry `true`/`false`: the result
/// words cannot be resolved, so the whole instruction must stay on the helper.
fn comparison_bif_module_without_booleans(name: &str, fixtures: &[CmpFn]) -> Module {
    comparison_bif_module_with_atoms(name, fixtures, false)
}

/// The shared builder of [`comparison_bif_module`].
fn comparison_bif_module_with_atoms(name: &str, fixtures: &[CmpFn], booleans: bool) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    if booleans {
        module.atom_table.intern("true");
        module.atom_table.intern("false");
    }

    let mut next_label = 1u32;
    for fixture in fixtures {
        let export = module.atom_table.intern(fixture.name);
        let module_atom = module.atom_table.intern(fixture.module);
        let function_atom = module.atom_table.intern(fixture.bif);
        module.imports.push(Import {
            module: module_atom,
            function: function_atom,
            arity: fixture.args.len() as u32,
        });
        let bif = ImportRef((module.imports.len() - 1) as u32);

        let label = Label(next_label);
        next_label += 1;
        let arity = fixture.instr_arity.unwrap_or(fixture.args.len() as u8);
        let args: Vec<Operand> = fixture
            .args
            .iter()
            .map(|spec| spec.operand(&mut module.atom_table))
            .collect();
        let dst = fixture.dst.operand(&mut module.atom_table);
        let call = if fixture.gc {
            Instr::GcBif {
                arity,
                bif,
                fail: NO_FAIL,
                live: fixture.arity,
                args,
                dst,
            }
        } else {
            Instr::Bif {
                arity,
                bif,
                fail: NO_FAIL,
                args,
                dst,
            }
        };
        let mut code = vec![Instr::Label(label), call];
        if fixture.tail_move {
            code.push(Instr::Move {
                src: fixture.dst.operand(&mut module.atom_table),
                dst: Operand::X(0),
            });
        }
        code.push(Instr::Return);

        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: label,
            entry_label: label,
            module: module.name,
            name: export,
            arity: fixture.arity,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: export,
            arity: fixture.arity,
            label,
        });
    }
    module
}

/// The fast-path comparison fixtures: the eight operators over two x-registers, the
/// eight with a small-int constant on either side (so both the `Guard::Lhs` and the
/// `Guard::Rhs` shapes are covered), a `gc_bif` spelling, a destination that is not
/// `x(0)`, and the two `Guard::None` constant forms (an atom under `==`, `nil` under
/// `=:=`).
fn comparison_bif_fixtures() -> Module {
    let mut fixtures: Vec<CmpFn> = Vec::new();
    for &(op, regs, const_rhs, const_lhs) in &COMPARISONS {
        fixtures.push(CmpFn::new(
            regs,
            2,
            op,
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(0),
        ));
        fixtures.push(CmpFn::new(
            const_rhs,
            1,
            op,
            &[ArithSpec::X(0), ArithSpec::Int(5)],
            ArithSpec::X(0),
        ));
        fixtures.push(CmpFn::new(
            const_lhs,
            1,
            op,
            &[ArithSpec::Int(5), ArithSpec::X(0)],
            ArithSpec::X(0),
        ));
    }
    fixtures.push(
        CmpFn::new(
            "gc_eq",
            2,
            "=:=",
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(0),
        )
        .gc(),
    );
    fixtures.push(
        CmpFn::new(
            "gt_dst_two",
            2,
            ">",
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(2),
        )
        .moving_the_result_to_x0(),
    );
    fixtures.push(CmpFn::new(
        "arith_eq_atom",
        1,
        "==",
        &[ArithSpec::X(0), ArithSpec::Atom("a")],
        ArithSpec::X(0),
    ));
    fixtures.push(CmpFn::new(
        "exact_eq_nil",
        1,
        "=:=",
        &[ArithSpec::X(0), ArithSpec::Nil],
        ArithSpec::X(0),
    ));
    comparison_bif_module("cmpbif", &fixtures)
}

/// The shapes the tier must **not** inline — one per clause of the fast path's shape
/// test: another BIF, the right BIF name in the wrong module, a non-2 arity (the
/// import's and the instruction's), a `Y`/float-register/literal operand, a bignum
/// constant, an atom and a `nil` constant under an *ordering* operator, a `Y`
/// destination, and a destination past [`MAX_REGISTER`].
fn non_inlinable_comparison_module() -> Module {
    comparison_bif_module(
        "cmpbif_helper",
        &[
            CmpFn::new(
                "divide",
                2,
                "div",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "eq_foreign",
                2,
                "==",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            )
            .in_module("mymod"),
            CmpFn::new(
                "eq_arity3",
                1,
                "==",
                &[ArithSpec::X(0), ArithSpec::Int(1), ArithSpec::Int(2)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "eq_instr_arity3",
                2,
                "==",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            )
            .with_instr_arity(3),
            CmpFn::new(
                "lt_y_operand",
                2,
                "<",
                &[ArithSpec::X(0), ArithSpec::Y(0)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_floatreg",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::FloatReg(0)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_literal",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Literal(0)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_bigconst",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Int(i64::MAX)],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_atom",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Atom("a")],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_nil",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Nil],
                ArithSpec::X(0),
            ),
            CmpFn::new(
                "lt_y_dst",
                1,
                "<",
                &[ArithSpec::X(0), ArithSpec::Int(5)],
                ArithSpec::Y(0),
            ),
            CmpFn::new(
                "lt_dst_oob",
                2,
                "<",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(MAX_REGISTER),
            ),
        ],
    )
}

/// The comparison-BIF counterpart of [`agree_arith`] (whose hook is a plain `fn`
/// pointer, which cannot carry the cloned atom table): assert the interpreter, the
/// baseline tier and the Cranelift tier all produce the same outcome for `export/arity`
/// under the same comparison floor, and return that outcome. `make_hook` builds one
/// fresh floor per tier (the interpreter and the baseline tier both *own* the hook).
fn agree_comparison<M, H>(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    mut make_hook: M,
) -> Result<Eterm, BeamError>
where
    M: FnMut() -> H,
    H: FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
{
    let interpreted = run_hooked(Tier::Interpreter, module, export, arity, args, make_hook());
    let baseline = run_hooked(Tier::BaselineJit, module, export, arity, args, make_hook());
    let cranelift = run_compiled_hooked(backend, compiled, module, export, arity, args, make_hook());
    assert_same(&baseline, &interpreted, &format!("baseline {export}/{arity}"));
    assert_same(&cranelift, &interpreted, &format!("cranelift {export}/{arity}"));
    interpreted
}

#[test]
fn the_tier_reports_which_comparison_bifs_it_inlined() {
    // Artifact-level evidence that the comparison fast path — not `jit_step_one` — was
    // *compiled*: a module that silently deopted would name no operator here.
    let compiled = CraneliftJit::new()
        .compile(&comparison_bif_fixtures())
        .expect("compile the comparison-BIF fixtures");
    let ops = compiled.inlined_comparison_ops();
    for &(op, ..) in &COMPARISONS {
        assert!(
            ops.contains(&op),
            "{op} must take the inline path, got {ops:?}"
        );
    }
    assert_eq!(
        ops.len(),
        8,
        "each of the eight operators is recorded exactly once: {ops:?}"
    );
    // The inlined comparison writes x-register `dst` natively, so the driver's
    // native-entry bound guard must see it.
    assert!(compiled.highest_native_register().is_some());

    // The negative module holds one fixture per clause of the shape test: none of them
    // may take the fast path.
    let compiled = CraneliftJit::new()
        .compile(&non_inlinable_comparison_module())
        .expect("compile the non-inlinable comparison fixtures");
    assert!(
        compiled.inlined_comparison_ops().is_empty(),
        "no non-inlinable shape may take the fast path, got {:?}",
        compiled.inlined_comparison_ops()
    );

    // ... and neither may a module whose atom table lacks `true`/`false`: the result
    // word cannot be resolved, so the whole instruction stays on the helper.
    let compiled = CraneliftJit::new()
        .compile(&comparison_bif_module_without_booleans(
            "cmpbif_no_bools",
            &[CmpFn::new(
                "lt",
                2,
                "<",
                &[ArithSpec::X(0), ArithSpec::X(1)],
                ArithSpec::X(0),
            )],
        ))
        .expect("compile the boolean-free fixture");
    assert!(
        compiled.inlined_comparison_ops().is_empty(),
        "a module without `true`/`false` must stay on the helper, got {:?}",
        compiled.inlined_comparison_ops()
    );
}

#[test]
fn the_inlined_comparison_bifs_agree_over_a_wide_range() {
    // Every inlined shape is pinned on both its pass and its fail side, across the whole
    // small-int range (boundaries included, where the tagged-word order is easiest to
    // get wrong), against the interpreter **and** an independently computed oracle.
    let module = comparison_bif_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the comparison-BIF fixtures");
    let atoms = module.atom_table.clone();

    let values = [-40i64, -1, 0, 1, 5, 6, 40, MIN_SMALL, MAX_SMALL];
    for &(op, regs, const_rhs, const_lhs) in &COMPARISONS {
        for &a in &values {
            for &b in &values {
                let args = [make_small(a), make_small(b)];
                let got = agree_comparison(
                    &backend,
                    &compiled,
                    &module,
                    regs,
                    2,
                    &args,
                    || comparison_floor(atoms.clone()),
                );
                assert_eq!(
                    got.ok(),
                    Some(boolean(&module, comparison_oracle(op, a, b))),
                    "{op}: {a} vs {b}"
                );
            }
            let arg = [make_small(a)];
            let got = agree_comparison(
                &backend,
                &compiled,
                &module,
                const_rhs,
                1,
                &arg,
                || comparison_floor(atoms.clone()),
            );
            assert_eq!(
                got.ok(),
                Some(boolean(&module, comparison_oracle(op, a, 5))),
                "{op}: {a} vs 5"
            );
            let got = agree_comparison(
                &backend,
                &compiled,
                &module,
                const_lhs,
                1,
                &arg,
                || comparison_floor(atoms.clone()),
            );
            assert_eq!(
                got.ok(),
                Some(boolean(&module, comparison_oracle(op, 5, a))),
                "{op}: 5 vs {a}"
            );
        }
    }

    // The `gc_bif` spelling, the non-`x(0)` destination, and the two guard-free constant
    // forms (an atom under `==`, `nil` under `=:=`).
    let args = [make_small(7), make_small(7)];
    for (export, arity, args, expected) in [
        ("gc_eq", 2, &args[..], true),
        ("gt_dst_two", 2, &args[..], false),
    ] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            export,
            arity,
            args,
            || comparison_floor(atoms.clone()),
        );
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "{export}");
    }
    let an_atom = make_atom(atom(&module, "a"));
    for (arg, expected) in [(an_atom, true), (make_small(7), false), (NIL, false)] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            "arith_eq_atom",
            1,
            &[arg],
            || comparison_floor(atoms.clone()),
        );
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "arith_eq_atom");
    }
    for (arg, expected) in [(NIL, true), (make_small(7), false), (an_atom, false)] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            "exact_eq_nil",
            1,
            &[arg],
            || comparison_floor(atoms.clone()),
        );
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "exact_eq_nil");
    }
}

#[test]
fn a_non_small_comparison_operand_deopts_to_the_helper_and_still_agrees() {
    // The operand guard is a *run-time* test, so the very same instruction inlines one
    // moment and deopts the next. These cases force the deopt: a boxed float cannot be a
    // tagged small word, and neither can a bignum, an atom or `[]` — while the artifact
    // proves the same operator was compiled with the inline path.
    let module = comparison_bif_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the comparison-BIF fixtures");
    assert!(compiled.inlined_comparison_ops().contains(&"<"));
    let atoms = module.atom_table.clone();

    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    let four_float = terms::make_float(&mut arena, 4.0).expect("the arena allocates a float");
    let bignum = terms::make_integer(&mut arena, i64::MAX).expect("the arena allocates a bignum");
    let an_atom = make_atom(atom(&module, "a"));

    // The float cases are the interesting ones: `5.0 =:= 5` is `false` (the exact
    // compare is class-strict) while `5.0 == 5` is `true` (the arithmetic compare is
    // value-based) — so a raw tagged-word compare on the boxed pointer would answer
    // `false`/`false` and be caught here.
    for (export, arity, args, expected) in [
        ("exact_eq", 2, [five_float, make_small(5)], false),
        ("exact_ne", 2, [five_float, make_small(5)], true),
        ("arith_eq", 2, [five_float, make_small(5)], true),
        ("arith_ne", 2, [five_float, make_small(5)], false),
        ("lt", 2, [four_float, make_small(5)], true),
        ("gt", 2, [four_float, make_small(5)], false),
        ("le", 2, [five_float, make_small(5)], true),
        ("ge", 2, [five_float, make_small(5)], true),
    ] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            export,
            arity,
            &args,
            || comparison_floor(atoms.clone()),
        );
        assert_eq!(got.ok(), Some(boolean(&module, expected)), "{export}(float)");
    }

    // A bignum, an atom and `[]` against a small: agreed with the interpreter (the
    // comparison itself has no oracle worth hand-writing, but the deopt must not
    // fabricate a word-compare answer).
    for bad in [bignum, an_atom, NIL] {
        for &(op, regs, ..) in &COMPARISONS {
            let args = [bad, make_small(5)];
            let got = agree_comparison(
                &backend,
                &compiled,
                &module,
                regs,
                2,
                &args,
                || comparison_floor(atoms.clone()),
            );
            assert!(got.is_ok(), "{op}(boxed, 5) must answer, not raise: {got:?}");
        }
    }
}

#[test]
fn the_inlined_comparison_never_consults_the_bif_hook() {
    // Behavioural proof, not an inference: for the same program the interpreter steps the
    // BIF exactly once, so the Cranelift tier's *zero* calls can only mean the inlined
    // fast path computed the tagged word itself.
    let module = comparison_bif_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the comparison-BIF fixtures");
    assert!(compiled.inlined_comparison_ops().contains(&"<"));
    let atoms = module.atom_table.clone();
    let args = [make_small(3), make_small(4)];

    let interpreted_calls = Arc::new(AtomicUsize::new(0));
    let interpreted = run_hooked(
        Tier::Interpreter,
        &module,
        "lt",
        2,
        &args,
        counting_comparison_floor(atoms.clone(), Arc::clone(&interpreted_calls)),
    )
    .expect("lt(3, 4) via the interpreter");
    assert_eq!(interpreted, boolean(&module, true));
    assert_eq!(
        interpreted_calls.load(Ordering::SeqCst),
        1,
        "the interpreter must call the BIF exactly once"
    );

    let jit_calls = Arc::new(AtomicUsize::new(0));
    let jitted = run_compiled_hooked(
        &backend,
        &compiled,
        &module,
        "lt",
        2,
        &args,
        counting_comparison_floor(atoms.clone(), Arc::clone(&jit_calls)),
    )
    .expect("lt(3, 4) via the cranelift tier");
    assert_eq!(jitted, boolean(&module, true));
    assert_eq!(
        jit_calls.load(Ordering::SeqCst),
        0,
        "the inlined fast path must not call the BIF"
    );

    // A run-time guard rejection *does* reach the floor (a boxed float is not a tagged
    // small), so the zero above is the fast path's own doing rather than a hook that is
    // never installed.
    let mut arena = MinimalContext::new();
    let five_float = terms::make_float(&mut arena, 5.0).expect("the arena allocates a float");
    let deopt_calls = Arc::new(AtomicUsize::new(0));
    let deopted = run_compiled_hooked(
        &backend,
        &compiled,
        &module,
        "lt",
        2,
        &[five_float, make_small(5)],
        counting_comparison_floor(atoms, Arc::clone(&deopt_calls)),
    )
    .expect("lt(5.0, 5) via the cranelift tier");
    assert_eq!(deopted, boolean(&module, false));
    assert_eq!(
        deopt_calls.load(Ordering::SeqCst),
        1,
        "a rejected operand must deopt to the helper, which calls the BIF"
    );
}

#[test]
fn non_inlinable_comparison_shapes_stay_on_the_helper() {
    // Each fixture violates exactly one clause of the fast path's shape test, so the
    // artifact must inline no operator for this module — and the shapes must still run,
    // with the helper's *exact* semantics (the interpreter's answer on every tier).
    let module = non_inlinable_comparison_module();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the non-inlinable comparison fixtures");
    assert!(
        compiled.inlined_comparison_ops().is_empty(),
        "no non-inlinable shape may take the fast path, got {:?}",
        compiled.inlined_comparison_ops()
    );
    let atoms = module.atom_table.clone();

    // Another BIF, the right BIF name in the wrong module, and a non-2 BIF arity: the
    // floor receives a call it rejects, so every tier reports its typed `BadArg`.
    for (export, arity, args) in [
        ("divide", 2, vec![make_small(6), make_small(7)]),
        ("eq_foreign", 2, vec![make_small(6), make_small(7)]),
        ("eq_arity3", 1, vec![make_small(6)]),
    ] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            export,
            arity,
            &args,
            || comparison_floor(atoms.clone()),
        );
        assert!(matches!(got, Err(BeamError::BadArg)), "{export}: got {got:?}");
    }

    // A *non-2 instruction* arity over a 2-arity import: the interpreter resolves the
    // import (so it computes the comparison), while the tier's shape test refuses the
    // instruction — and the deopt reproduces the interpreter exactly.
    let got = agree_comparison(
        &backend,
        &compiled,
        &module,
        "eq_instr_arity3",
        2,
        &[make_small(6), make_small(6)],
        || comparison_floor(atoms.clone()),
    );
    assert_eq!(got.ok(), Some(boolean(&module, true)), "eq_instr_arity3");

    // A float-register operand (the register file is `NIL`-filled) and an unresolvable
    // literal: refused at compile time, and the helper's operand read is the engine's
    // exact semantics.
    for (export, arity, args) in [
        ("lt_floatreg", 1, vec![make_small(2)]),
    ] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            export,
            arity,
            &args,
            || comparison_floor(atoms.clone()),
        );
        assert!(got.is_ok(), "{export} must be an answer, not a raise: {got:?}");
    }
    // An unresolvable static literal: the helper's operand read is the engine's typed
    // error, and both native tiers report it alike (never a panic, never a fabricated
    // result).
    let got = agree_comparison(
        &backend,
        &compiled,
        &module,
        "lt_literal",
        1,
        &[make_small(2)],
        || comparison_floor(atoms.clone()),
    );
    assert!(got.is_err(), "an unresolvable literal must be a typed error: {got:?}");
    let got = agree_comparison(
        &backend,
        &compiled,
        &module,
        "lt_bigconst",
        1,
        &[make_small(2)],
        || comparison_floor(atoms.clone()),
    );
    assert_eq!(
        got.ok(),
        Some(boolean(&module, true)),
        "2 < a bignum constant"
    );

    // The ordering operators with an atom or `nil` constant are refused too (a
    // non-number is not word-ordered), and the shape still answers.
    for (export, arg) in [
        ("lt_atom", make_small(2)),
        ("lt_nil", make_small(2)),
        ("lt_atom", NIL),
        ("lt_nil", NIL),
    ] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            export,
            1,
            &[arg],
            || comparison_floor(atoms.clone()),
        );
        assert!(got.is_ok(), "{export} must be an answer, not a raise: {got:?}");
    }

    // A `Y` destination (the helper's write is the engine's typed `bad_register`, since
    // the fixture allocates no frame) and a destination past `MAX_REGISTER`.
    for (export, arity, args) in [
        ("lt_y_dst", 1, vec![make_small(2)]),
        ("lt_dst_oob", 2, vec![make_small(2), make_small(3)]),
    ] {
        let got = agree_comparison(
            &backend,
            &compiled,
            &module,
            export,
            arity,
            &args,
            || comparison_floor(atoms.clone()),
        );
        assert!(got.is_err(), "{export} must be a typed error: {got:?}");
    }
}

#[test]
fn a_comparison_over_a_module_without_booleans_stays_on_the_helper() {
    // The result words are the *module's* `true`/`false` atoms, so a module whose atom
    // table lacks them cannot be lowered natively — the instruction must stay on the
    // helper, whose interpreter step is the BIF's exact semantics (here: the floor
    // reports it, and a context without one surfaces the same typed error on every tier).
    let module = comparison_bif_module_without_booleans(
        "cmpbif_no_bools",
        &[CmpFn::new(
            "lt",
            2,
            "<",
            &[ArithSpec::X(0), ArithSpec::X(1)],
            ArithSpec::X(0),
        )],
    );
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module).expect("compile the fixture");
    assert!(compiled.inlined_comparison_ops().is_empty());

    let args = [make_small(3), make_small(4)];
    let interpreted = run_module(
        Tier::Interpreter,
        &module,
        "lt",
        2,
        &args,
        &mut MinimalContext::new(),
    );
    assert_same(
        &run_module(
            Tier::BaselineJit,
            &module,
            "lt",
            2,
            &args,
            &mut MinimalContext::new(),
        ),
        &interpreted,
        "baseline lt/2 without result atoms",
    );
    assert_same(
        &run_compiled(&backend, &compiled, &module, "lt", 2, &args),
        &interpreted,
        "cranelift lt/2 without result atoms",
    );
    assert!(interpreted.is_err(), "the fixture has no BIF floor");
}

// ---------------------------------------------------------------------------
// The inlined shape instructions (`select_val`, the tuple reads, the list reads)
// (`src/jit/cranelift/`)
// ---------------------------------------------------------------------------
//
// The other Phase-A additions lower one *shape* each, guarded at run time:
//
// * `select_val` — the immediate-alternatives-only subset, with **no** run-time guard
//   (`=:=` over immediates *is* a raw-word compare; the immediate classes have disjoint
//   words, so a boxed scrutinee can never collide with one). A `NO_FAIL` switch (which
//   raises) or any non-immediate alternative deopts the whole instruction.
// * `get_tuple_element` / `select_tuple_arity` — guard the source as an `arityval`
//   header (`is_boxed` then the arity sub-tag), then read / switch on the arity; a
//   non-tuple, a too-short tuple or a `NO_FAIL` arity switch deopts to the helper.
// * `get_list` / `get_hd` / `get_tl` — guard the source as a cons cell
//   (`(word & 0x2) == 0`; `[]` is not one), then read `CAR`/`CDR`; a non-cons deopts.
//
// Every deopt lands on the shared interpreter step, whose family raise (`{badmatch,
// V}`) or typed error is the exact ground truth — so the deopt cases below agree with
// the interpreter **including on the typed error**, never a fabricated value.

/// The body of one hand-built shape fixture: the instructions after the entry label,
/// with the four labels (`entry`, `a1`, `a2`, `fail`) available for branches. Only
/// x-register / integer / `nil` operands are used (no imports, no ids).
type ShapeBody = fn(Label, Label, Label, Label) -> Vec<Instr>;

/// Build a hand-built module (the loader is not involved) whose exported functions are
/// exactly `fixtures`, each shaped like [`test_module`]'s functions: a fresh entry
/// label plus three fresh branch targets, the body appended after `label entry`.
fn shape_module(name: &str, fixtures: &[(&'static str, u32, ShapeBody)]) -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    // A structured `{badmatch, V}` raise means the raising family resolves `badmatch`
    // by name, so intern it up front.
    module.atom_table.intern("badmatch");

    let mut next_label = 1u32;
    for &(fixture_name, arity, body) in fixtures {
        let export = module.atom_table.intern(fixture_name);
        let entry = Label(next_label);
        let a1 = Label(next_label + 1);
        let a2 = Label(next_label + 2);
        let fail = Label(next_label + 3);
        next_label += 4;

        let mut code = vec![Instr::Label(entry)];
        code.extend(body(entry, a1, a2, fail));
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module: module.name,
            name: export,
            arity,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function: export,
            arity,
            label: entry,
        });
    }
    module
}

/// `move {integer,V} -> x0; return` — the answer of a fixture's terminal branch.
fn shape_answer(value: i64) -> Vec<Instr> {
    vec![
        Instr::Move {
            src: Operand::Integer(value),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

// -- tuple reads -------------------------------------------------------------

/// `get_tuple_element x0 0 x0` — the element aliases the source register.
fn tuple_elem0(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 0,
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `get_tuple_element x0 1 x0`.
fn tuple_elem1(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 1,
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `get_tuple_element x0 9 x0` — an index past a short tuple is a run-time guard
/// rejection, but a long-enough tuple takes the fast path.
fn tuple_elem_oob(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 9,
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `get_tuple_element nil 0 x0` — a constant source is a compile-time shape rejection.
fn tuple_elem_const_src(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetTupleElement {
            source: Operand::Nil,
            element: 0,
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `select_tuple_arity x0 fail {2 -> a1, 3 -> a2}`; `a1`/`a2` answer the arity, `fail`
/// answers `0`.
fn tuple_arity(_entry: Label, a1: Label, a2: Label, fail: Label) -> Vec<Instr> {
    let mut code = vec![Instr::SelectTupleArity {
        tuple: Operand::X(0),
        fail,
        destinations: vec![(Operand::Integer(2), a1), (Operand::Integer(3), a2)],
    }];
    code.push(Instr::Label(a1));
    code.extend(shape_answer(2));
    code.push(Instr::Label(a2));
    code.extend(shape_answer(3));
    code.push(Instr::Label(fail));
    code.extend(shape_answer(0));
    code
}

/// `select_tuple_arity x0 NO_FAIL {2 -> a1}` — a `NO_FAIL` switch deopts as a whole
/// (its no-match path raises `{badmatch, Tuple}`).
fn tuple_arity_nofail(_entry: Label, a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    let mut code = vec![Instr::SelectTupleArity {
        tuple: Operand::X(0),
        fail: NO_FAIL,
        destinations: vec![(Operand::Integer(2), a1)],
    }];
    code.push(Instr::Label(a1));
    code.extend(shape_answer(2));
    code
}

// -- list reads --------------------------------------------------------------

/// `get_hd x0 x0`.
fn list_hd(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetHd {
            source: Operand::X(0),
            head: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `get_tl x0 x0`.
fn list_tl(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetTl {
            source: Operand::X(0),
            tail: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `get_list x0 x1 x2; move x1 -> x0` — split with distinct head/tail destinations.
fn list_split_head(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::X(1),
            tail: Operand::X(2),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `get_list x0 x1 x2; move x2 -> x0`.
fn list_split_tail(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::GetList {
            source: Operand::X(0),
            head: Operand::X(1),
            tail: Operand::X(2),
        },
        Instr::Move {
            src: Operand::X(2),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

// -- select_val --------------------------------------------------------------

/// `select_val x0 fail {1 -> a1, 2 -> a2}`; `a1`/`a2` answer `10`/`20`, `fail`
/// answers `0`.
fn sel_int(_entry: Label, a1: Label, a2: Label, fail: Label) -> Vec<Instr> {
    let mut code = vec![Instr::SelectVal {
        value: Operand::X(0),
        fail,
        pairs: vec![(Operand::Integer(1), a1), (Operand::Integer(2), a2)],
    }];
    code.push(Instr::Label(a1));
    code.extend(shape_answer(10));
    code.push(Instr::Label(a2));
    code.extend(shape_answer(20));
    code.push(Instr::Label(fail));
    code.extend(shape_answer(0));
    code
}

/// `select_val x0 fail {nil -> a1, 3 -> a2}`.
fn sel_nil(_entry: Label, a1: Label, a2: Label, fail: Label) -> Vec<Instr> {
    let mut code = vec![Instr::SelectVal {
        value: Operand::X(0),
        fail,
        pairs: vec![(Operand::Nil, a1), (Operand::Integer(3), a2)],
    }];
    code.push(Instr::Label(a1));
    code.extend(shape_answer(10));
    code.push(Instr::Label(a2));
    code.extend(shape_answer(20));
    code.push(Instr::Label(fail));
    code.extend(shape_answer(0));
    code
}

/// `select_val x0 NO_FAIL {1 -> a1, 2 -> a2}` — a `NO_FAIL` switch deopts as a whole.
fn sel_nofail(_entry: Label, a1: Label, a2: Label, _fail: Label) -> Vec<Instr> {
    let mut code = vec![Instr::SelectVal {
        value: Operand::X(0),
        fail: NO_FAIL,
        pairs: vec![(Operand::Integer(1), a1), (Operand::Integer(2), a2)],
    }];
    code.push(Instr::Label(a1));
    code.extend(shape_answer(10));
    code.push(Instr::Label(a2));
    code.extend(shape_answer(20));
    code
}

/// `select_val x0 fail {x1 -> a1}` — a **register** alternative needs a term compare,
/// so the whole instruction deopts.
fn sel_reg_alt(_entry: Label, a1: Label, _a2: Label, fail: Label) -> Vec<Instr> {
    let mut code = vec![Instr::SelectVal {
        value: Operand::X(0),
        fail,
        pairs: vec![(Operand::X(1), a1)],
    }];
    code.push(Instr::Label(a1));
    code.extend(shape_answer(10));
    code.push(Instr::Label(fail));
    code.extend(shape_answer(0));
    code
}

/// The fixture module for every Phase-A shape: the inlinable tuple/list/`select_val`
/// shapes and one deopt shape per rejection clause.
fn shape_fixtures() -> Module {
    shape_module(
        "shape",
        &[
            ("tuple_elem0", 1, tuple_elem0),
            ("tuple_elem1", 1, tuple_elem1),
            ("tuple_elem_oob", 1, tuple_elem_oob),
            ("tuple_elem_const_src", 1, tuple_elem_const_src),
            ("tuple_arity", 1, tuple_arity),
            ("tuple_arity_nofail", 1, tuple_arity_nofail),
            ("list_hd", 1, list_hd),
            ("list_tl", 1, list_tl),
            ("list_split_head", 1, list_split_head),
            ("list_split_tail", 1, list_split_tail),
            ("sel_int", 1, sel_int),
            ("sel_nil", 1, sel_nil),
            ("sel_nofail", 1, sel_nofail),
            ("sel_reg_alt", 2, sel_reg_alt),
        ],
    )
}

/// Assert the interpreter and the Cranelift tier (through a caller-owned compiled
/// artifact) produce the same outcome for `export/arity`, and — when `expected` is
/// given — that the interpreter matches that independently computed oracle too. Returns
/// the shared outcome.
fn agree_shape(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    expected: Option<Eterm>,
) -> Result<Eterm, BeamError> {
    let interpreted = run_module(
        Tier::Interpreter,
        module,
        export,
        arity,
        args,
        &mut MinimalContext::new(),
    );
    let cranelift = run_compiled(backend, compiled, module, export, arity, args);
    assert_same(
        &cranelift,
        &interpreted,
        &format!("cranelift {export}/{arity}"),
    );
    if let Some(expected) = expected {
        match &interpreted {
            Ok(value) => assert_eq!(
                *value, expected,
                "interpreter {export}/{arity} disagrees with the oracle"
            ),
            Err(err) => panic!("interpreter {export}/{arity} failed: {err:?}"),
        }
    }
    interpreted
}

/// Run `export/arity` on the interpreter tier **and** on the Cranelift tier (through a
/// caller-owned compiled artifact) with **caller-owned** contexts, assert the two agree
/// on the outcome, and hand both outcomes to `check` while the contexts that own their
/// reason terms are still alive.
///
/// A raise's reason term is allocated on the run's own heap, so the check must happen
/// here rather than on a returned value: [`agree_shape`]'s contexts are temporaries and
/// die before the caller could read the term.
fn agree_outcome_with<F>(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    mut check: F,
) where
    F: FnMut(&str, &Result<Eterm, BeamError>),
{
    let mut interpreted_ctx = MinimalContext::new();
    let interpreted = run_module(
        Tier::Interpreter,
        module,
        export,
        arity,
        args,
        &mut interpreted_ctx,
    );
    let mut cranelift_ctx = MinimalContext::new();
    let mut machine = Machine::new();
    let cranelift = backend.run_compiled(
        compiled,
        &mut machine,
        module,
        atom(module, export),
        arity,
        args,
        &mut cranelift_ctx,
    );
    assert_same(
        &cranelift,
        &interpreted,
        &format!("cranelift {export}/{arity}"),
    );
    check("interpreter", &interpreted);
    check("cranelift", &cranelift);
}

/// The live reason **term** an `error`-class raise carries out of the machine.
///
/// A family raise ([`Machine::throw_error`]) with no live catch frame leaves the
/// machine as `Raise::new(Error, Structured)` with the reason term in the opaque
/// `live_reason` slot, so `ExitReason::as_atom()` stays `None` and a compound reason is
/// recoverable only as a term.
fn live_reason_term(result: &Result<Eterm, BeamError>, what: &str) -> Eterm {
    let raise = match result {
        Err(BeamError::Raise(raise)) => raise,
        other => panic!("{what}: expected an error-class raise, got {other:?}"),
    };
    assert_eq!(raise.class, ExceptionClass::Error, "{what}: wrong class");
    assert_eq!(
        raise.reason,
        ExitReason::Structured,
        "{what}: a term reason must stay structured"
    );
    assert_eq!(
        raise.reason.as_atom(),
        None,
        "{what}: a term reason must not surface as an atom"
    );
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
        .unwrap_or_else(|| panic!("{what}: the raise carries no live reason term"))
}

/// Build the `{badmatch, Value}` reason the tuple/list families raise, on the caller's
/// arena (which must outlive the comparison — the raise's own reason term lives on the
/// run's heap, so both sides are read while they are alive).
fn badmatch_expected(arena: &mut MinimalContext, module: &Module, value: Eterm) -> Eterm {
    let tag = make_atom(
        module
            .atom_table
            .lookup("badmatch")
            .expect("the shape fixtures intern `badmatch`"),
    );
    terms::make_tuple(arena, &[tag, value]).expect("build {badmatch, Value}")
}

/// Assert **both** tiers reject `export/arity` with the family's exact
/// `{badmatch, Value}` reason — class `error`, tag `badmatch` resolved from the running
/// module's own atom table, and `expected` (the offending value) as the second element.
fn agree_badmatch(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    expected: Eterm,
    what: &str,
) {
    agree_outcome_with(
        backend,
        compiled,
        module,
        export,
        arity,
        args,
        |tier, result| {
            let live = live_reason_term(result, &format!("{tier} {what}"));
            assert!(
                beam_term::eq(live, expected, &module.atom_table),
                "{tier} {what}: the raise reason is not the exact {{badmatch, Value}}"
            );
        },
    );
}

/// Assert **both** tiers reject `export/arity` with the exact typed engine error
/// `unsupported_opcode: <reason>` — an **atom** reason (so no live term), which is the
/// documented degradation of a `NO_FAIL` guard whose `function_clause` atom the running
/// module's own table cannot name.
fn agree_atom_raise(
    backend: &CraneliftJit,
    compiled: &CompiledFunctions,
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
    reason: &str,
    what: &str,
) {
    agree_outcome_with(
        backend,
        compiled,
        module,
        export,
        arity,
        args,
        |tier, result| {
            let raise = match result {
                Err(BeamError::Raise(raise)) => raise,
                other => panic!("{tier} {what}: expected a typed raise, got {other:?}"),
            };
            assert_eq!(
                raise.class,
                ExceptionClass::Error,
                "{tier} {what}: wrong class"
            );
            assert_eq!(
                raise.reason.as_atom(),
                Some(reason),
                "{tier} {what}: wrong typed error"
            );
            assert!(
                raise.live_reason().is_none(),
                "{tier} {what}: an atom reason carries no live term"
            );
        },
    );
}

#[test]
fn the_tier_reports_which_shape_instructions_it_inlined() {
    // Artifact-level evidence that the shape fast paths — not `jit_step_one` — were
    // *compiled*.
    let module = shape_fixtures();
    let compiled = CraneliftJit::new()
        .compile(&module)
        .expect("compile the shape fixtures");

    // Exactly the two immediate-only, fail-labelled switches (`sel_int`, `sel_nil`)
    // inline; `sel_nofail` and `sel_reg_alt` do not.
    assert_eq!(
        compiled.inlined_select_val_count(),
        2,
        "only the immediate-only, fail-labelled switches may inline"
    );

    let tuple_reads = compiled.inlined_tuple_read_kinds();
    assert!(
        tuple_reads.contains(&"get_tuple_element"),
        "get_tuple_element must inline, got {tuple_reads:?}"
    );
    assert!(
        tuple_reads.contains(&"select_tuple_arity"),
        "select_tuple_arity must inline, got {tuple_reads:?}"
    );

    let list_reads = compiled.inlined_list_read_kinds();
    for kind in ["get_list", "get_hd", "get_tl"] {
        assert!(
            list_reads.contains(&kind),
            "{kind} must inline, got {list_reads:?}"
        );
    }

    // A `NO_FAIL` select and a register-alternative select must stay on the helper:
    // compile a module holding only that shape and observe no inlined select.
    for (name, arity, body) in [
        ("nofail", 1u32, sel_nofail as ShapeBody),
        ("reg_alt", 2, sel_reg_alt as ShapeBody),
    ] {
        let only = shape_module("only", &[(name, arity, body)]);
        let compiled = CraneliftJit::new()
            .compile(&only)
            .expect("compile the single-shape module");
        assert_eq!(
            compiled.inlined_select_val_count(),
            0,
            "{name} must not inline its `select_val`"
        );
    }

    // Likewise a `NO_FAIL` `select_tuple_arity` and a constant-source
    // `get_tuple_element` must stay on the helper.
    for (name, body) in [
        ("arity_nofail", tuple_arity_nofail as ShapeBody),
        ("const_src", tuple_elem_const_src as ShapeBody),
    ] {
        let only = shape_module("only", &[(name, 1, body)]);
        let compiled = CraneliftJit::new()
            .compile(&only)
            .expect("compile the single-shape module");
        assert!(
            compiled.inlined_tuple_read_kinds().is_empty(),
            "{name} must not inline its tuple read, got {:?}",
            compiled.inlined_tuple_read_kinds()
        );
    }
}

#[test]
fn the_inlined_tuple_reads_agree_and_deopt_like_the_interpreter() {
    let module = shape_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the shape fixtures");
    assert!(compiled
        .inlined_tuple_read_kinds()
        .contains(&"get_tuple_element"));

    // The boxed inputs live on an arena that outlives every run.
    let mut arena = MinimalContext::new();
    let pair =
        terms::make_tuple(&mut arena, &[make_small(11), make_small(22)]).expect("arena tuple");
    let triple = terms::make_tuple(&mut arena, &[make_small(7), make_small(8), make_small(9)])
        .expect("arena tuple");
    let single = terms::make_tuple(&mut arena, &[make_small(5)]).expect("arena tuple");

    // Fast path: element 0 / element 1, both aliasing the source register.
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_elem0",
        1,
        &[pair],
        Some(make_small(11)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_elem1",
        1,
        &[pair],
        Some(make_small(22)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_elem1",
        1,
        &[triple],
        Some(make_small(8)),
    );

    // The arity switch: a match answers the arity, no match falls to the fail label.
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_arity",
        1,
        &[pair],
        Some(make_small(2)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_arity",
        1,
        &[triple],
        Some(make_small(3)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_arity",
        1,
        &[single],
        Some(make_small(0)),
    );

    // Deopt: `tuple_elem_oob` on a 2-tuple — the element index is out of range, the
    // guard rejects, and the helper raises the family's exact `{badmatch, Tuple}`:
    // `badmatch` resolved from the running module's own atom table and the offending
    // tuple as the reason's second element.
    let expected_reason = badmatch_expected(&mut arena, &module, pair);
    agree_badmatch(
        &backend,
        &compiled,
        &module,
        "tuple_elem_oob",
        1,
        &[pair],
        expected_reason,
        "an out-of-range element",
    );

    // ... and the very same instruction *does* take the fast path for a long enough
    // tuple.
    let twelve: Vec<Eterm> = (0i64..12).map(make_small).collect();
    let long = terms::make_tuple(&mut arena, &twelve).expect("arena tuple");
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_elem_oob",
        1,
        &[long],
        Some(make_small(9)),
    );

    // Deopt: the source is a constant operand — the shape test rejects it at compile
    // time and the helper raises `{badmatch, nil}`: the constant source *is* the
    // offending value, so the reason's second element is `nil`.
    let expected_reason = badmatch_expected(&mut arena, &module, NIL);
    agree_badmatch(
        &backend,
        &compiled,
        &module,
        "tuple_elem_const_src",
        1,
        &[pair],
        expected_reason,
        "a constant source",
    );

    // Deopt: a non-tuple source — the guard rejects at run time. `get_tuple_element`
    // raises the family's exact `{badmatch, Value}`; the arity switch has no arity to
    // match (C's `get_arity` yields `NIL`), so it takes its fail branch and answers
    // the fail arm's `0`.
    for bad in [make_small(1), NIL] {
        let expected_reason = badmatch_expected(&mut arena, &module, bad);
        agree_badmatch(
            &backend,
            &compiled,
            &module,
            "tuple_elem0",
            1,
            &[bad],
            expected_reason,
            &format!("tuple_elem0({bad:?})"),
        );
        let _ = agree_shape(
            &backend,
            &compiled,
            &module,
            "tuple_arity",
            1,
            &[bad],
            Some(make_small(0)),
        );
    }
    // The `NO_FAIL` arity switch deopts as a whole: a 2-tuple still matches through the
    // helper, a 4-tuple raises `{badmatch, Tuple}` carrying the unmatched tuple itself.
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tuple_arity_nofail",
        1,
        &[pair],
        Some(make_small(2)),
    );
    let quad = terms::make_tuple(
        &mut arena,
        &[make_small(1), make_small(2), make_small(3), make_small(4)],
    )
    .expect("arena tuple");
    let expected_reason = badmatch_expected(&mut arena, &module, quad);
    agree_badmatch(
        &backend,
        &compiled,
        &module,
        "tuple_arity_nofail",
        1,
        &[quad],
        expected_reason,
        "a NO_FAIL arity switch with no match",
    );
}
#[test]
fn the_inlined_list_reads_agree_and_deopt_like_the_interpreter() {
    let module = shape_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the shape fixtures");
    let list_reads = compiled.inlined_list_read_kinds();
    for kind in ["get_list", "get_hd", "get_tl"] {
        assert!(
            list_reads.contains(&kind),
            "{kind} must inline, got {list_reads:?}"
        );
    }

    let mut arena = MinimalContext::new();
    let tail = terms::make_cons(&mut arena, make_small(2), NIL).expect("arena cons");
    let list = terms::make_cons(&mut arena, make_small(1), tail).expect("arena cons");
    // `[1 | 9]` — an improper list is still a legal cons cell.
    let improper = terms::make_cons(&mut arena, make_small(1), make_small(9)).expect("arena cons");

    // Fast path: head/tail reads and the split forms.
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "list_hd",
        1,
        &[list],
        Some(make_small(1)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "list_tl",
        1,
        &[list],
        Some(tail),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "list_tl",
        1,
        &[improper],
        Some(make_small(9)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "list_split_head",
        1,
        &[list],
        Some(make_small(1)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "list_split_tail",
        1,
        &[list],
        Some(tail),
    );

    // Deopt: `[]`, an integer and a tuple are not cons cells, so the guard rejects and
    // the helper raises the family's exact `{badmatch, Source}` — the rejected source
    // itself as the reason's second element.
    let a_tuple = terms::make_tuple(&mut arena, &[make_small(1), make_small(2)]).expect("arena tuple");
    for bad in [NIL, make_small(1), a_tuple] {
        for export in ["list_hd", "list_tl", "list_split_head", "list_split_tail"] {
            let expected_reason = badmatch_expected(&mut arena, &module, bad);
            agree_badmatch(
                &backend,
                &compiled,
                &module,
                export,
                1,
                &[bad],
                expected_reason,
                &format!("{export}({bad:?})"),
            );
        }
    }
}
#[test]
fn the_inlined_select_val_agrees_and_deopts_like_the_interpreter() {
    let module = shape_fixtures();
    let mut backend = CraneliftJit::new();
    let compiled = backend
        .compile(&module)
        .expect("compile the shape fixtures");
    assert_eq!(compiled.inlined_select_val_count(), 2);

    // The integer switch: only `1`/`2` answer `10`/`20`, everything else falls to the
    // fail label (`0`).
    for value in [-1i64, 0, 1, 2, 3, MAX_SMALL, MIN_SMALL] {
        let expected = match value {
            1 => 10,
            2 => 20,
            _ => 0,
        };
        let _ = agree_shape(
            &backend,
            &compiled,
            &module,
            "sel_int",
            1,
            &[make_small(value)],
            Some(make_small(expected)),
        );
    }

    // The `nil` alternative is an immediate word compare too.
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_nil",
        1,
        &[NIL],
        Some(make_small(10)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_nil",
        1,
        &[make_small(3)],
        Some(make_small(20)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_nil",
        1,
        &[make_small(4)],
        Some(make_small(0)),
    );

    // A boxed scrutinee's word can never collide with an immediate, so both switches
    // take the fail label.
    let mut arena = MinimalContext::new();
    let float = terms::make_float(&mut arena, 1.0).expect("arena float");
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_nil",
        1,
        &[float],
        Some(make_small(0)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_int",
        1,
        &[float],
        Some(make_small(0)),
    );

    // The inlined switch never consults the BIF floor (the fixture holds no `gc_bif`
    // at all, so the counting floor must stay at zero under the Cranelift tier).
    let calls = Arc::new(AtomicUsize::new(0));
    let got = run_compiled_hooked(
        &backend,
        &compiled,
        &module,
        "sel_int",
        1,
        &[make_small(2)],
        counting_marker_arith(Arc::clone(&calls)),
    )
    .expect("sel_int(2) via the Cranelift tier");
    assert_eq!(got, make_small(20));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the inlined switch must not call the BIF floor"
    );

    // A `NO_FAIL` switch deopts as a whole: a match runs through the helper ...
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_nofail",
        1,
        &[make_small(1)],
        Some(make_small(10)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_nofail",
        1,
        &[make_small(2)],
        Some(make_small(20)),
    );
    // ... and no match raises the interpreter's typed error: a `NO_FAIL` switch raises
    // class `error` with the bare atom `function_clause`, resolved **only** from the
    // running module's own atom table — and `shape_module` interns `badmatch` but never
    // `function_clause`, so the documented degradation applies and the exact typed error
    // is the engine diagnostic below (never a substituted atom).
    agree_atom_raise(
        &backend,
        &compiled,
        &module,
        "sel_nofail",
        1,
        &[make_small(7)],
        "unsupported_opcode: select_val: no 'function_clause' atom in the module",
        "a NO_FAIL switch with no match",
    );
    // A register alternative deopts as a whole; the helper's exact (`cmp`) compare
    // answers `10` for a match and `0` for a mismatch.
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_reg_alt",
        2,
        &[make_small(5), make_small(5)],
        Some(make_small(10)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_reg_alt",
        2,
        &[make_small(5), make_small(6)],
        Some(make_small(0)),
    );
    // A boxed scrutinee is `=:=` to the register's identical word.
    let five_float = terms::make_float(&mut arena, 5.0).expect("arena float");
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "sel_reg_alt",
        2,
        &[five_float, five_float],
        Some(make_small(10)),
    );
}

// ---------------------------------------------------------------------------
// The inlined x-register exchange (`swap`) and the frame-less local tail call
// (`call_only`) (`src/jit/cranelift/{inline,codegen}.rs`)
// ---------------------------------------------------------------------------
//
// Both lower to control flow the tier already emits exactly: `swap` reads both
// x-register slots to SSA values **before** storing them exchanged (so an aliasing
// pair is a no-op, as the interpreter's read-both-then-write-both is), and `call_only`
// is a `DISPATCH_REL` — i.e. exactly the `jump` lowering — with no frame to pop.

/// `swap_pair/2` — `swap x0 x1; return` (returns the exchanged `x0`, i.e. the 2nd arg).
fn swap_pair(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::Swap {
            a: Operand::X(0),
            b: Operand::X(1),
        },
        Instr::Return,
    ]
}

/// `swap_self/1` — `swap x0 x0; return`: the aliasing pair must be a no-op.
fn swap_self(_entry: Label, _a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::Swap {
            a: Operand::X(0),
            b: Operand::X(0),
        },
        Instr::Return,
    ]
}

/// `tail_call/1` — `call_only 1 L; L: x0 = 42; return`: a same-function local tail
/// call, lowered like a `jump` to the resolved label.
fn tail_call(_entry: Label, a1: Label, _a2: Label, _fail: Label) -> Vec<Instr> {
    vec![
        Instr::CallOnly {
            arity: 1,
            target: a1,
        },
        Instr::Label(a1),
        Instr::Move {
            src: Operand::Integer(42),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]
}

#[test]
fn the_inlined_swap_and_tail_call_agree_and_are_recorded() {
    let module = shape_module(
        "exchange",
        &[
            ("swap_pair", 2, swap_pair),
            ("swap_self", 1, swap_self),
            ("tail_call", 1, tail_call),
        ],
    );
    let mut backend = CraneliftJit::new();
    let compiled = backend.compile(&module).expect("compile the exchange fixtures");

    // Artifact-level evidence that the fast paths — not `jit_step_one` — were compiled.
    assert_eq!(
        compiled.inlined_swap_count(),
        2,
        "both swap fixtures must inline their exchange"
    );
    assert_eq!(
        compiled.inlined_tail_call_count(),
        1,
        "the frame-less tail call must inline"
    );

    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "swap_pair",
        2,
        &[make_small(1), make_small(2)],
        Some(make_small(2)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "swap_pair",
        2,
        &[make_small(-7), make_small(9)],
        Some(make_small(9)),
    );
    // The aliasing pair is a no-op, not a lost value.
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "swap_self",
        1,
        &[make_small(9)],
        Some(make_small(9)),
    );
    let _ = agree_shape(
        &backend,
        &compiled,
        &module,
        "tail_call",
        1,
        &[make_small(0)],
        Some(make_small(42)),
    );
}
