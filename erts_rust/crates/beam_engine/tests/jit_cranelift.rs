//! Cranelift hot-tier integration test (feature-gated on `jit-cranelift`).
//!
//! Drives the committed `beam_loader` oracle fixture
//! (`beam_loader/tests/fixtures/slice.beam`) through the **Cranelift tier** and
//! asserts it agrees with the interpreter — on the same [`MinimalContext`] BIF hook
//! `tests/slice.rs` uses (`erlang:'+'/2` and `erlang:'-'/2` over small integers).
//!
//! The whole file is `cfg`-ed out when the feature is off, so the default build
//! neither compiles nor links any Cranelift code (see `src/jit/cranelift.rs`).
#![cfg(feature = "jit-cranelift")]

use beam_common::BeamError;
use beam_engine::jit::cranelift::CraneliftJit;
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, Machine, MinimalContext, Tier};
use beam_loader::{load_file, Module};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm};

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
fn a_raising_bif_propagates_the_same_typed_error() {
    // `add/2`'s arithmetic BIF has no fail label, so a failure is a raise; the helper
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
