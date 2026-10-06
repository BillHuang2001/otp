//! Baseline-JIT integration test: the native tier runs the vertical slice.
//!
//! Drives the committed `beam_loader` oracle fixture
//! (`beam_loader/tests/fixtures/slice.beam`) through **both** execution tiers and
//! asserts they agree, on the same `MinimalContext` BIF hook `tests/slice.rs` uses
//! (`erlang:'+'/2` and `erlang:'-'/2` over small integers). The baseline tier
//! compiles the module's basic blocks to x86-64 and runs them through its driver
//! loop; the interpreter is the reference semantics.
//!
//! The second half of the file is the evidence for the tier's one optimisation —
//! the **guarded small-int arithmetic fast path** for `erlang:'+'/2`/`'-'/2`
//! (`src/jit/baseline.rs`): it proves the fast path produces exactly the
//! interpreter's results across the small-int range (including the boundary cases
//! that must fall back to `jit_step_one`), that it never consults the BIF hook,
//! and that a rejected (non-small) operand really does fall back.

use std::sync::atomic::{AtomicUsize, Ordering};

use beam_common::BeamError;
use beam_engine::jit::baseline::BaselineJit;
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, Machine, MinimalContext, Tier};
use beam_loader::{
    load_file, Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{is_small, make_atom, make_small, signed_value, Atom, Eterm, MAX_SMALL, MIN_SMALL};

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
    // The `gc_bif '+'/2` fast path computes these natively on the native tier.
    assert_eq!(agree("add", 2, &[make_small(0), make_small(0)]), make_small(0));
    assert_eq!(
        agree("add", 2, &[make_small(-7), make_small(-9)]),
        make_small(-16)
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

// ---------------------------------------------------------------------------
// The small-int arithmetic fast path (`src/jit/baseline.rs`)
// ---------------------------------------------------------------------------

/// Counts every call the counting BIF hook serves, so a test can assert *which*
/// path (the native fast path or the `jit_step_one` fallback) a run took.
static ARITH_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);

/// [`arith_hook`] plus a call counter.
fn counting_arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    ARITH_HOOK_CALLS.fetch_add(1, Ordering::SeqCst);
    arith_hook(module, function, arity, args)
}

/// [`arith_hook`], but range-checked: a result outside `MIN_SMALL..=MAX_SMALL` is
/// the typed `BadArith` a real `+`/`-` BIF would raise, instead of the wrong term
/// a small-int-only hook would build (this hook has no bignum arithmetic).
fn checked_arith_hook(
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
    let result = match function {
        "+" => a.checked_add(b),
        "-" => a.checked_sub(b),
        _ => return Err(BeamError::BadArg),
    };
    match result {
        Some(value) if (MIN_SMALL..=MAX_SMALL).contains(&value) => Ok(make_small(value)),
        _ => Err(BeamError::BadArith),
    }
}

/// The in-range result of `a <op> b`, or `None` when it is not a small integer
/// (the range-checked hook then raises, on both tiers).
fn small_result(a: i64, b: i64, op: char) -> Option<i64> {
    let result = match op {
        '+' => a.checked_add(b),
        '-' => a.checked_sub(b),
        _ => return None,
    }?;
    (MIN_SMALL..=MAX_SMALL).contains(&result).then_some(result)
}

/// A hand-built fast-path fixture (the loader is not involved): the exports
/// `plus/2` and `minus/2`, each one basic block
/// `[label, gc_bif '+'/'-'/2 [x(0), x(1)] -> x(0), return]` against the module's
/// own `erlang` import. The two functions use distinct labels, as the loader
/// guarantees a module's labels are unique.
fn arith_module() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("fastpath");
    let erlang = module.atom_table.intern("erlang");
    for (index, (name, function_name)) in [("plus", "+"), ("minus", "-")].iter().enumerate() {
        let function = module.atom_table.intern(name);
        module.imports.push(Import {
            module: erlang,
            function: module.atom_table.intern(function_name),
            arity: 2,
        });
        let label = Label(index as u32 + 1);
        let code = vec![
            Instr::Label(label),
            Instr::GcBif {
                arity: 2,
                bif: ImportRef(index as u32),
                fail: NO_FAIL,
                live: 2,
                args: vec![Operand::X(0), Operand::X(1)],
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let (blocks, labels) = beam_loader::lower::build_blocks(&code);
        module.functions.push(Function {
            func_label: label,
            entry_label: label,
            module: module.name,
            name: function,
            arity: 2,
            code,
            blocks,
            labels,
        });
        module.exports.push(Export {
            function,
            arity: 2,
            label,
        });
    }
    module
}

/// Run `export/arity` of `module` on `tier` with `hook` as the BIF floor.
fn run_module_tier<H>(
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

/// Assert the interpreter and the native baseline tier agree on `module`'s
/// `export/arity` — the same value *or* the same typed error — and return the
/// native tier's result.
fn agree_module(
    module: &Module,
    export: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    let interpreted = run_module_tier(
        Tier::Interpreter,
        module,
        export,
        arity,
        args,
        checked_arith_hook,
    );
    let jitted = run_module_tier(
        Tier::BaselineJit,
        module,
        export,
        arity,
        args,
        checked_arith_hook,
    );
    match (&interpreted, &jitted) {
        (Ok(interpreted), Ok(jitted)) => {
            assert_eq!(jitted, interpreted, "{export}/{arity} differs between tiers")
        }
        (Err(interpreted), Err(jitted)) => assert_eq!(
            format!("{jitted:?}"),
            format!("{interpreted:?}"),
            "tiers disagree on the typed error for {export}/{arity}"
        ),
        _ => panic!("tiers disagree for {export}/{arity}: {interpreted:?} vs {jitted:?}"),
    }
    jitted
}

#[test]
fn arithmetic_fast_path_matches_the_interpreter_across_the_small_range() {
    let module = arith_module();
    // The cases drive every arm of the emitted guard: results the fast path
    // computes natively from the tagged words, tagged sums that overflow an
    // `i64` (so the `jo` guard hands the instruction to the helper), results
    // that are small but whose fast path bailed, and results outside the small
    // range (the helper's typed `BadArith`).
    let cases: [(i64, i64); 14] = [
        (0, 0),
        (2, 3),
        (-4, 10),
        (7, -20),
        (-20, 7),
        (-1, -1),
        (MAX_SMALL - 2, 1),
        (MIN_SMALL + 2, -1),
        (MAX_SMALL, 0),
        (MIN_SMALL, 0),
        (MAX_SMALL, 1),
        (MAX_SMALL, MAX_SMALL),
        (MIN_SMALL, -1),
        (MIN_SMALL, MIN_SMALL),
    ];
    for (a, b) in cases {
        let args = [make_small(a), make_small(b)];
        for (export, op) in [("plus", '+'), ("minus", '-')] {
            let result = agree_module(&module, export, 2, &args);
            match (result, small_result(a, b, op)) {
                (Ok(got), Some(expected)) => {
                    assert_eq!(got, make_small(expected), "{export}({a}, {b})");
                }
                (Err(error), None) => assert_eq!(
                    format!("{error:?}"),
                    format!("{:?}", BeamError::BadArith),
                    "{export}({a}, {b}) must be the out-of-range BadArith"
                ),
                (got, expected) => panic!("{export}({a}, {b}): {got:?} vs {expected:?}"),
            }
        }
    }
}

#[test]
fn arithmetic_fast_path_agrees_over_a_sweep_of_small_integers() {
    // A denser differential sweep than the boundary list above: every pair from a
    // value set clustered at `0`, at the 8-bit/kernel-ish magnitudes and at both
    // ends of the small-int range must agree with the interpreter, and every
    // in-range result must be exactly `make_small(a op b)`.
    let module = arith_module();
    let values = [
        MIN_SMALL,
        MIN_SMALL + 1,
        MIN_SMALL + 2,
        -(1 << 30),
        -1001,
        -16,
        -15,
        -1,
        0,
        1,
        15,
        16,
        1001,
        1 << 30,
        MAX_SMALL - 2,
        MAX_SMALL - 1,
        MAX_SMALL,
    ];
    for a in values {
        for b in values {
            let args = [make_small(a), make_small(b)];
            for (export, op) in [("plus", '+'), ("minus", '-')] {
                let result = agree_module(&module, export, 2, &args);
                match (result, small_result(a, b, op)) {
                    (Ok(got), Some(expected)) => {
                        assert_eq!(got, make_small(expected), "{export}({a}, {b})");
                    }
                    (Err(_), None) => {}
                    (got, expected) => panic!("{export}({a}, {b}): {got:?} vs {expected:?}"),
                }
            }
        }
    }
}

#[test]
fn the_synthetic_fixture_compiles_and_its_small_int_plus_bypasses_the_hook() {
    // Structural evidence: both hand-built functions really do compile to native
    // blocks (so the sweep above ran on the native tier, not the interpreter
    // fallback) ...
    let module = arith_module();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile the synthetic fixture");
    assert_eq!(compiled.block_count(), 2, "one block per function");
    assert!(compiled.entry(0, 0).is_some(), "plus/2's block must be compiled");
    assert!(compiled.entry(1, 0).is_some(), "minus/2's block must be compiled");
    assert!(compiled.code_len() > 0);

    // ... and behavioural evidence: the compiled `gc_bif '+'/2` computes the sum
    // itself, so the BIF hook is never consulted.
    ARITH_HOOK_CALLS.store(0, Ordering::SeqCst);
    let got = run_module_tier(
        Tier::BaselineJit,
        &module,
        "plus",
        2,
        &[make_small(19), make_small(23)],
        counting_arith_hook,
    )
    .expect("plus(19, 23)");
    assert_eq!(got, make_small(42));
    assert_eq!(
        ARITH_HOOK_CALLS.load(Ordering::SeqCst),
        0,
        "the fast path must not call the BIF"
    );
}

#[test]
fn the_native_fast_path_does_not_consult_the_bif_hook() {
    // `slice:add/2` is a single `gc_bif '+'/2` over its two arguments, so the
    // interpreter's hook sees exactly one call. The native fast path computes the
    // addition itself, so the hook must not be reached at all.
    let module = slice_module();
    let args = [make_small(20), make_small(22)];

    ARITH_HOOK_CALLS.store(0, Ordering::SeqCst);
    let interpreted = run_module_tier(
        Tier::Interpreter,
        &module,
        "add",
        2,
        &args,
        counting_arith_hook,
    )
    .expect("add(20, 22) via the interpreter");
    assert_eq!(interpreted, make_small(42));
    assert_eq!(
        ARITH_HOOK_CALLS.load(Ordering::SeqCst),
        1,
        "the interpreter must call the BIF exactly once"
    );

    ARITH_HOOK_CALLS.store(0, Ordering::SeqCst);
    let jitted = run_module_tier(
        Tier::BaselineJit,
        &module,
        "add",
        2,
        &args,
        counting_arith_hook,
    )
    .expect("add(20, 22) via the native tier");
    assert_eq!(jitted, interpreted);
    assert_eq!(
        ARITH_HOOK_CALLS.load(Ordering::SeqCst),
        0,
        "the native fast path must not call the BIF"
    );
}

#[test]
fn a_non_small_operand_falls_back_to_the_interpreter_helper() {
    let module = arith_module();
    let atom_arg = make_atom(atom(&module, "erlang"));
    // `x(0)` is not a small integer, so the guard rejects and the block deopts to
    // `jit_step_one`: the hook raises `BadArith` and both tiers report it alike.
    for bad in [beam_term::NIL, atom_arg] {
        let args = [bad, make_small(1)];

        ARITH_HOOK_CALLS.store(0, Ordering::SeqCst);
        let interpreted = run_module_tier(
            Tier::Interpreter,
            &module,
            "plus",
            2,
            &args,
            counting_arith_hook,
        );
        assert_eq!(ARITH_HOOK_CALLS.load(Ordering::SeqCst), 1);

        ARITH_HOOK_CALLS.store(0, Ordering::SeqCst);
        let jitted = run_module_tier(
            Tier::BaselineJit,
            &module,
            "plus",
            2,
            &args,
            counting_arith_hook,
        );
        // The fallback is the interpreter itself, so the hook ran exactly once...
        assert_eq!(
            ARITH_HOOK_CALLS.load(Ordering::SeqCst),
            1,
            "the rejected guard must fall back to the BIF"
        );
        // ... and the two tiers agree on the result and the typed error.
        assert!(matches!(jitted, Err(BeamError::BadArith)), "got {jitted:?}");
        assert_eq!(format!("{jitted:?}"), format!("{interpreted:?}"));
    }
}
