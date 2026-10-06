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
//! The whole file is `cfg`-ed out when the feature is off, so the default build
//! neither compiles nor links any Cranelift code (see `src/jit/cranelift.rs`).
#![cfg(feature = "jit-cranelift")]

use beam_common::BeamError;
use beam_engine::jit::cranelift::{CompiledFunctions, CraneliftJit};
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, terms, Machine, MinimalContext, Tier};
use beam_loader::{
    load_file, Export, Function, Instr, Label, Module, Operand, TestKind, NO_FAIL,
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
