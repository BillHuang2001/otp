//! Baseline-JIT integration test: the native tier runs the vertical slice.
//!
//! Drives the committed `beam_loader` oracle fixture
//! (`beam_loader/tests/fixtures/slice.beam`) through **both** execution tiers and
//! asserts they agree, on the same `MinimalContext` BIF hook `tests/slice.rs` uses
//! (`erlang:'+'/2` and `erlang:'-'/2` over small integers). The baseline tier
//! compiles the module's basic blocks to x86-64 and runs them through its driver
//! loop; the interpreter is the reference semantics.

use beam_common::BeamError;
use beam_engine::jit::baseline::BaselineJit;
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
    machine.run_with_tier(
        &module,
        atom(&module, export),
        arity,
        args,
        &mut ctx,
        tier,
    )
}

/// Assert the two tiers agree on `export/arity` and return the shared result.
fn agree(export: &str, arity: u32, args: &[Eterm]) -> Eterm {
    let interpreted = run_on_tier(Tier::Interpreter, export, arity, args)
        .unwrap_or_else(|err| panic!("interpreter {export}/{arity} failed: {err:?}"));
    let jitted = run_on_tier(Tier::BaselineJit, export, arity, args)
        .unwrap_or_else(|err| panic!("baseline-jit {export}/{arity} failed: {err:?}"));
    assert_eq!(jitted, interpreted, "{export}/{arity} differs between tiers");
    jitted
}

#[test]
fn available_tiers_lists_the_baseline_backend() {
    assert!(available_tiers().contains(&"baseline-jit"));
    assert!(Tier::BaselineJit.is_available());
    assert_eq!(Tier::from_name("baseline-jit"), Some(Tier::BaselineJit));
    // The backend names the tier it implements.
    assert_eq!(JitBackend::name(&BaselineJit::new()), "baseline-jit");
    assert!(JitBackend::available(&BaselineJit::new()));
}

#[test]
fn compile_emits_a_native_block_for_every_non_empty_block() {
    let module = slice_module();
    let mut backend = BaselineJit::new();
    let compiled = backend.compile(&module).expect("compile slice.beam");

    let expected: usize = module
        .functions
        .iter()
        .map(|function| {
            function
                .blocks
                .iter()
                .filter(|block| block.start < block.end.min(function.code.len()))
                .count()
        })
        .sum();

    assert!(expected > 0, "the fixture must have basic blocks");
    assert_eq!(compiled.block_count(), expected);
    assert!(compiled.code_len() > 0, "no machine code was emitted");

    // Every block's entry is inside the code buffer and unique.
    let (start, end) = compiled.code_address_range();
    let mut entries: Vec<usize> = module
        .functions
        .iter()
        .enumerate()
        .flat_map(|(func, function)| {
            function
                .blocks
                .iter()
                .map(|block| (func as u32, block.start as u32))
                .collect::<Vec<_>>()
        })
        .filter_map(|(func, pc)| compiled.entry(func, pc).map(|entry| entry as usize))
        .collect();
    let total = entries.len();
    entries.sort_unstable();
    entries.dedup();
    assert_eq!(entries.len(), total, "two blocks share a native entry");
    for entry in &entries {
        assert!(
            start <= *entry && *entry < end,
            "entry {entry:#x} is outside the code mapping {start:#x}..{end:#x}"
        );
    }
}

#[test]
fn add_matches_the_interpreter_on_the_native_tier() {
    assert_eq!(agree("add", 2, &[make_small(2), make_small(3)]), make_small(5));
    assert_eq!(
        agree("add", 2, &[make_small(-4), make_small(10)]),
        make_small(6)
    );
}

#[test]
fn fib_matches_the_interpreter_on_the_native_tier() {
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
fn both_tiers_agree_across_register_file_sizes() {
    // The compiled blocks cache x-registers and load them from `machine.x`; a
    // register file that does not cover the cached index must not be entered (the
    // run stays on the interpreter step). Either way the result must match the
    // interpreter's for the same machine.
    let module = slice_module();
    let args = [make_small(2), make_small(3)];
    for max_register in [2u32, 3, 8, beam_engine::MAX_REGISTER] {
        let mut interpreted = Machine::with_max_register(max_register);
        let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
        let expected = interpreted.run_export(
            &module,
            atom(&module, "add"),
            2,
            &args,
            &mut ctx,
        );

        let mut jitted = Machine::with_max_register(max_register);
        let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
        let got = jitted.run_with_tier(
            &module,
            atom(&module, "add"),
            2,
            &args,
            &mut ctx,
            Tier::BaselineJit,
        );

        match (got, expected) {
            (Ok(got), Ok(expected)) => assert_eq!(got, expected),
            (Err(got), Err(expected)) => assert_eq!(
                format!("{got:?}"),
                format!("{expected:?}"),
                "tiers disagree on the typed error for max_register={max_register}"
            ),
            (got, expected) => panic!(
                "tiers disagree for max_register={max_register}: {got:?} vs {expected:?}"
            ),
        }
    }
}

#[test]
fn a_raising_bif_propagates_the_same_typed_error() {
    // `add/2`'s arithmetic BIF has no fail label, so a failure is a raise; with no
    // catch frame it leaves the run as a typed error. The JIT's helper returns the
    // `ERROR` word and the driver must hand back exactly that error.
    let bad = beam_term::NIL;
    let interpreted = run_on_tier(Tier::Interpreter, "add", 2, &[bad, make_small(1)])
        .expect_err("the interpreter must reject a non-integer arith argument");
    let jitted = run_on_tier(Tier::BaselineJit, "add", 2, &[bad, make_small(1)])
        .expect_err("the native tier must reject it too");
    assert_eq!(format!("{jitted:?}"), format!("{interpreted:?}"));
}

#[test]
fn a_missing_export_is_a_typed_error_on_the_native_tier() {
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
            Tier::BaselineJit,
        )
        .expect_err("add/3 is not exported");
    assert!(matches!(err, BeamError::Raise(_)));
}
