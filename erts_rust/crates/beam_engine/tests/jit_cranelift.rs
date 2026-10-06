//! Cranelift hot-tier integration test (feature-gated on `jit-cranelift`).
//!
//! Drives the committed `beam_loader` oracle fixture
//! (`beam_loader/tests/fixtures/slice.beam`) through the **Cranelift tier** and
//! asserts it agrees with the interpreter — on the same [`MinimalContext`] BIF hook
//! `tests/slice.rs` uses (`erlang:'+'/2` and `erlang:'-'/2` over small integers).
//!
//! The second half is the evidence for the tier's inlined `Instr::Test` subset
//! (`src/jit/cranelift.rs`): every newly inlined `TestKind` is pinned on **both** its
//! success and its failure side against the interpreter and an independently
//! computed oracle, the *inline* path is proven to have been compiled (through the
//! artifact's `inlined_test_kinds()` introspection — a test that silently fell
//! through to `jit_step_one` fails), and the run-time guard's deopt (a float
//! operand) is exercised where only the shared helper can produce the answer.
//!
//! The third part is the evidence for the tier's other inlined instruction — the
//! guarded small-int `erlang:'+'/2`/`'-'/2` `gc_bif` fast path — pinned the same way
//! and, in addition, **behaviourally**: the fixtures run against BIF floors that
//! count their calls, so "the fast path computed this" (no call) and "the guard
//! rejected this and the helper computed it" (exactly one call) are observable
//! rather than inferred.
//!
//! The whole file is `cfg`-ed out when the feature is off, so the default build
//! neither compiles nor links any Cranelift code (see `src/jit/cranelift.rs`).
#![cfg(feature = "jit-cranelift")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_common::BeamError;
use beam_engine::jit::cranelift::{CompiledFunctions, CraneliftJit, SmallArith};
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, terms, Machine, MinimalContext, Tier};
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
    // `erlang:'+'/2`/`'-'/2` fast path, and routes everything else through the shared
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
// The inlined `test` subset (`src/jit/cranelift.rs`)
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
    // A boxed-term/structural test and the *exact* kinds over two x-registers (whose
    // words may differ for `=:=` terms) are outside the subset: the artifact must
    // name no inlined kind for this module, and the shapes must still run — through
    // one interpreter step each.
    let module = test_module(
        "helper",
        &[
            TestFn::new("is_integer", 1, TestKind::IsInteger, &[Spec::X(0)]),
            TestFn::new("is_tuple", 1, TestKind::IsTuple, &[Spec::X(0)]),
            TestFn::new(
                "eq_exact_regs",
                2,
                TestKind::IsEqExact,
                &[Spec::X(0), Spec::X(1)],
            ),
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
    // The exact compare over two registers answers `=:=`'s value through the helper.
    assert_eq!(
        run("eq_exact_regs", 2, &[make_small(7), make_small(7)]),
        flag(true)
    );
    assert_eq!(
        run("eq_exact_regs", 2, &[make_small(7), make_small(8)]),
        flag(false)
    );
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
// The inlined small-int `erlang:'+'/2` / `'-'/2` fast path (`src/jit/cranelift.rs`)
// ---------------------------------------------------------------------------
//
// A `gc_bif erlang:'+'/2`/`'-'/2` is the hot instruction of the compute-bound
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
    if function != "+" && function != "-" {
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
    /// A `Y` (stack slot) operand — neither an x-register nor a constant.
    Y(u32),
}

impl ArithSpec {
    /// The loader operand this spec stands for.
    fn operand(self) -> Operand {
        match self {
            ArithSpec::X(n) => Operand::X(n),
            ArithSpec::Int(value) => Operand::Integer(value),
            ArithSpec::FloatReg(n) => Operand::FloatReg(n),
            ArithSpec::Literal(index) => Operand::Literal(index),
            ArithSpec::Y(n) => Operand::Y(n),
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
                args: fixture.args.iter().map(|spec| spec.operand()).collect(),
                dst: fixture.dst.operand(),
            },
        ];
        if fixture.tail_move {
            code.push(Instr::Move {
                src: fixture.dst.operand(),
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

/// The fast-path fixtures: every shape the tier inlines — the two BIFs, an
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
                "times",
                2,
                "erlang",
                "*",
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
    for op in [SmallArith::Add, SmallArith::Sub] {
        assert!(
            ops.contains(&op),
            "{} must take the inline path, got {ops:?}",
            op.name()
        );
    }
    assert_eq!(ops.len(), 2, "only the two `+`/`-` ops are inlined: {ops:?}");
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
        "times",
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
