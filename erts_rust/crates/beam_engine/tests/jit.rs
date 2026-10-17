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
//! the **guarded small-int arithmetic fast path** for `erlang:'+'/2`/`'-'/2`/`'*'/2`
//! (`src/jit/baseline/`): it proves the fast path produces exactly the
//! interpreter's results across the small-int range (including the boundary cases
//! that must fall back to `jit_step_one`), that it never consults the BIF hook,
//! and that a rejected (non-small) operand really does fall back.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_common::BeamError;
use beam_engine::jit::baseline::BaselineJit;
use beam_engine::jit::JitBackend;
use beam_engine::{available_tiers, Machine, MinimalContext, Tier};
use beam_loader::{
    load_file, Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{
    is_small, make_atom, make_small, signed_value, Atom, Eterm, Heap, MAX_SMALL, MIN_SMALL, NIL,
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

/// The slice BIF hook: `erlang:'+'/2`, `erlang:'-'/2` and `erlang:'*'/2` over
/// small integers.
///
/// It has no bignum arithmetic, so it is used only where the operands and the
/// product stay in the small range; the multiplication wraps rather than panicking
/// (the out-of-range cases are driven through [`checked_arith_hook`] or the
/// sentinel hook in `the_times_fast_path_deopts_...`).
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
        "*" => Ok(make_small(a.wrapping_mul(b))),
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
// The small-int arithmetic fast path (`src/jit/baseline/`)
// ---------------------------------------------------------------------------

/// [`arith_hook`] plus a call counter, so a test can assert *which* path (the
/// native fast path or the `jit_step_one` fallback) a run took.
///
/// The counter is **per-call-site** (each test owns its own `Arc`) rather than a
/// process-global `static`: the tests in this file run in parallel in one binary,
/// so a shared counter would interleave across runs and flake.
fn counting_arith_hook(
    counter: Arc<AtomicUsize>,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static {
    move |module, function, arity, args| {
        counter.fetch_add(1, Ordering::SeqCst);
        arith_hook(module, function, arity, args)
    }
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
        "*" => a.checked_mul(b),
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
        '*' => a.checked_mul(b),
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

/// The `erlang:'*'/2` counterpart of [`arith_module`]: the single export `times/2`
/// as one basic block `[label, gc_bif '*'/2 [x(0), x(1)] -> x(0), return]`.
///
/// A separate fixture rather than a third function in [`arith_module`], so the
/// existing plus/minus tests keep their exact two-block shape.
fn mul_module() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("fastpath_mul");
    let erlang = module.atom_table.intern("erlang");
    let function = module.atom_table.intern("times");
    module.imports.push(Import {
        module: erlang,
        function: module.atom_table.intern("*"),
        arity: 2,
    });
    let label = Label(1);
    let code = vec![
        Instr::Label(label),
        Instr::GcBif {
            arity: 2,
            bif: ImportRef(0),
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
                    // Out of the small range: the range-checked hook's `BadArith` is the
                    // only error this fixture can produce (`raised` through the `NO_FAIL`
                    // `gc_bif`, which has no catch frame here), so pin it exactly — never
                    // "any error".
                    (Err(error), None) => assert!(
                        matches!(&error, BeamError::BadArith),
                        "{export}({a}, {b}) must be the out-of-range BadArith, got {error:?}"
                    ),
                    (got, expected) => panic!("{export}({a}, {b}): {got:?} vs {expected:?}"),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The small-int `erlang:'*'/2` fast path
// ---------------------------------------------------------------------------

/// The `'*'` deopt oracle: exactly the small-int product when it fits, else `NIL`
/// as a sentinel. No multiplication of two small integers can legitimately produce
/// `NIL`, so a run that returns it *proves* the guard rejected the instruction and
/// the helper really consulted the BIF.
fn sentinel_mul_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || function != "*" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !is_small(args[0]) || !is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    let (a, b) = (signed_value(args[0]), signed_value(args[1]));
    match a.checked_mul(b) {
        Some(product) if (MIN_SMALL..=MAX_SMALL).contains(&product) => Ok(make_small(product)),
        _ => Ok(NIL),
    }
}

/// [`sentinel_mul_hook`] plus a call counter (see [`counting_arith_hook`] for why
/// the counter is per-call-site rather than a process-global `static`).
fn counting_sentinel_mul_hook(
    counter: Arc<AtomicUsize>,
) -> impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static {
    move |module, function, arity, args| {
        counter.fetch_add(1, Ordering::SeqCst);
        sentinel_mul_hook(module, function, arity, args)
    }
}

#[test]
fn arithmetic_fast_path_times_matches_the_interpreter_across_the_small_range() {
    let module = mul_module();
    // The cases drive every arm of the emitted `'*'` guard: products the fast path
    // computes natively, products exactly at each end of the small range, and
    // products just past each end (some still fitting the `i64` the untagged
    // multiply works in, so only the **untagged** range check rejects them; others
    // overflowing it, so the `imul` `jo` guard does).
    let cases: [(i64, i64); 22] = [
        (0, 0),
        (2, 3),
        (3, 2),
        (-4, 10),
        (10, -4),
        (7, -20),
        (-1, -1),
        (15, 16),
        (-65536, 65536),
        (MAX_SMALL, 0),
        (MIN_SMALL, 0),
        (MAX_SMALL, 1), // exactly the upper boundary
        (1, MAX_SMALL),
        (MIN_SMALL, 1), // exactly the lower boundary
        (MAX_SMALL, -1), // -(2^59 - 1) == MIN_SMALL + 1: still in range
        (MAX_SMALL, 2),  // one past the upper boundary (fits i64)
        (MIN_SMALL, 2),  // one past the lower boundary (fits i64)
        (MIN_SMALL, -1), // 2^59 == MAX_SMALL + 1 (fits i64)
        (1 << 30, 1 << 30), // 2^60 (fits i64)
        (MAX_SMALL, MAX_SMALL), // overflows i64: the `imul` overflow guard
        (MIN_SMALL, MIN_SMALL), // overflows i64
        (MIN_SMALL, MAX_SMALL), // overflows i64
    ];
    for (a, b) in cases {
        let args = [make_small(a), make_small(b)];
        let result = agree_module(&module, "times", 2, &args);
        match (result, small_result(a, b, '*')) {
            (Ok(got), Some(expected)) => {
                assert_eq!(got, make_small(expected), "times({a}, {b})");
            }
            (Err(error), None) => assert_eq!(
                format!("{error:?}"),
                format!("{:?}", BeamError::BadArith),
                "times({a}, {b}) must be the out-of-range BadArith"
            ),
            (got, expected) => panic!("times({a}, {b}): {got:?} vs {expected:?}"),
        }
    }
}

#[test]
fn arithmetic_fast_path_times_agrees_over_a_sweep_of_small_integers() {
    // A denser differential sweep than the boundary list above: every pair from a
    // value set clustered at `0`, at the 8-bit/kernel-ish magnitudes and at both
    // ends of the small-int range must agree with the interpreter, and every
    // in-range product must be exactly `make_small(a * b)`.
    let module = mul_module();
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
            let result = agree_module(&module, "times", 2, &args);
            match (result, small_result(a, b, '*')) {
                (Ok(got), Some(expected)) => {
                    assert_eq!(got, make_small(expected), "times({a}, {b})");
                }
                // Out of the small range: `checked_arith_hook`'s `BadArith` is the only
                // error this fixture can produce (no catch frame, so the `NO_FAIL`
                // `gc_bif` error propagates unchanged), so pin it exactly.
                (Err(error), None) => assert!(
                    matches!(&error, BeamError::BadArith),
                    "times({a}, {b}) must be the out-of-range BadArith, got {error:?}"
                ),
                (got, expected) => panic!("times({a}, {b}): {got:?} vs {expected:?}"),
            }
        }
    }
}

#[test]
fn the_synthetic_times_fixture_compiles_and_bypasses_the_hook() {
    // Structural evidence: `times/2` really does compile to a native block ...
    let module = mul_module();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile the synthetic fixture");
    assert_eq!(compiled.block_count(), 1, "one block for times/2");
    assert!(
        compiled.entry(0, 0).is_some(),
        "times/2's block must be compiled"
    );
    assert!(compiled.code_len() > 0);

    // ... and behavioural evidence: the compiled `gc_bif '*'/2` multiplies itself,
    // so the BIF hook is never consulted.
    let calls = Arc::new(AtomicUsize::new(0));
    let got = run_module_tier(
        Tier::BaselineJit,
        &module,
        "times",
        2,
        &[make_small(6), make_small(7)],
        counting_arith_hook(Arc::clone(&calls)),
    )
    .expect("times(6, 7)");
    assert_eq!(got, make_small(42));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the fast path must not call the BIF"
    );
}

#[test]
fn the_times_fast_path_deopts_to_the_bif_when_the_product_is_out_of_range() {
    let module = mul_module();

    // In range: the native fast path computes the product and never consults the BIF.
    let fast_calls = Arc::new(AtomicUsize::new(0));
    let got = run_module_tier(
        Tier::BaselineJit,
        &module,
        "times",
        2,
        &[make_small(6), make_small(7)],
        counting_sentinel_mul_hook(Arc::clone(&fast_calls)),
    )
    .expect("times(6, 7)");
    assert_eq!(got, make_small(42));
    assert_eq!(
        fast_calls.load(Ordering::SeqCst),
        0,
        "the in-range fast path must not call the BIF"
    );

    // Out of range: the guard must reject and the helper hand the instruction to the
    // BIF, whose sentinel answer (`NIL`) neither the fast path nor a wrongly widened
    // result could have produced. `MAX_SMALL * 2` is the discriminating case: its
    // *tagged* word wraps to `-17`, which is inside `[MIN_SMALL_WORD, MAX_SMALL_WORD]`,
    // so a tagged-word range check would have accepted it and returned a wrong small.
    let cases = [
        (MAX_SMALL, 2),         // 2^60 - 2: fits i64, out of the small range
        (MIN_SMALL, 2),         // -2^60: the same shape at the other end
        (MIN_SMALL, -1),        // 2^59 == MAX_SMALL + 1
        (1 << 30, 1 << 30),     // 2^60
        (1 << 30, -(1 << 30)),  // -2^60
        (MAX_SMALL, MAX_SMALL), // overflows i64: the `imul` overflow guard
        (MIN_SMALL, MIN_SMALL), // overflows i64
        (MIN_SMALL, MAX_SMALL), // overflows i64
    ];
    for (a, b) in cases {
        let args = [make_small(a), make_small(b)];
        for tier in [Tier::Interpreter, Tier::BaselineJit] {
            let calls = Arc::new(AtomicUsize::new(0));
            let got = run_module_tier(
                tier,
                &module,
                "times",
                2,
                &args,
                counting_sentinel_mul_hook(Arc::clone(&calls)),
            )
            .unwrap_or_else(|err| panic!("{tier:?} times({a}, {b}): {err:?}"));
            assert_eq!(
                got, NIL,
                "{tier:?} times({a}, {b}) must be the BIF's sentinel answer"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "{tier:?} times({a}, {b}) must consult the BIF exactly once"
            );
        }
    }
}

#[test]
fn a_non_small_times_operand_falls_back_to_the_interpreter_helper() {
    let module = mul_module();
    let mut heap = Heap::new(4);
    // SAFETY: `alloc_words(2)` hands out exactly the two-word `[HEADER_FLONUM, bits]`
    // block `write_float` writes, and `heap` outlives every use of the term.
    let float = unsafe {
        let hp = heap.alloc_words(2).expect("a 2-word float block");
        beam_term::write_float(hp, 6.0)
    };
    let atom_arg = make_atom(atom(&module, "erlang"));

    // None of these is a small integer, so the guard rejects and the block deopts to
    // `jit_step_one`: the hook raises `BadArith` and both tiers report it alike.
    for bad in [NIL, atom_arg, float] {
        let args = [bad, make_small(2)];

        let interpreted_calls = Arc::new(AtomicUsize::new(0));
        let interpreted = run_module_tier(
            Tier::Interpreter,
            &module,
            "times",
            2,
            &args,
            counting_arith_hook(Arc::clone(&interpreted_calls)),
        );
        assert_eq!(interpreted_calls.load(Ordering::SeqCst), 1);

        let jitted_calls = Arc::new(AtomicUsize::new(0));
        let jitted = run_module_tier(
            Tier::BaselineJit,
            &module,
            "times",
            2,
            &args,
            counting_arith_hook(Arc::clone(&jitted_calls)),
        );
        assert_eq!(
            jitted_calls.load(Ordering::SeqCst),
            1,
            "the rejected guard must fall back to the BIF"
        );
        assert!(matches!(jitted, Err(BeamError::BadArith)), "got {jitted:?}");
        assert_eq!(format!("{jitted:?}"), format!("{interpreted:?}"));
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
    let calls = Arc::new(AtomicUsize::new(0));
    let got = run_module_tier(
        Tier::BaselineJit,
        &module,
        "plus",
        2,
        &[make_small(19), make_small(23)],
        counting_arith_hook(Arc::clone(&calls)),
    )
    .expect("plus(19, 23)");
    assert_eq!(got, make_small(42));
    assert_eq!(
        calls.load(Ordering::SeqCst),
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

    let interpreted_calls = Arc::new(AtomicUsize::new(0));
    let interpreted = run_module_tier(
        Tier::Interpreter,
        &module,
        "add",
        2,
        &args,
        counting_arith_hook(Arc::clone(&interpreted_calls)),
    )
    .expect("add(20, 22) via the interpreter");
    assert_eq!(interpreted, make_small(42));
    assert_eq!(
        interpreted_calls.load(Ordering::SeqCst),
        1,
        "the interpreter must call the BIF exactly once"
    );

    let jitted_calls = Arc::new(AtomicUsize::new(0));
    let jitted = run_module_tier(
        Tier::BaselineJit,
        &module,
        "add",
        2,
        &args,
        counting_arith_hook(Arc::clone(&jitted_calls)),
    )
    .expect("add(20, 22) via the native tier");
    assert_eq!(jitted, interpreted);
    assert_eq!(
        jitted_calls.load(Ordering::SeqCst),
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

        let interpreted_calls = Arc::new(AtomicUsize::new(0));
        let interpreted = run_module_tier(
            Tier::Interpreter,
            &module,
            "plus",
            2,
            &args,
            counting_arith_hook(Arc::clone(&interpreted_calls)),
        );
        assert_eq!(interpreted_calls.load(Ordering::SeqCst), 1);

        let jitted_calls = Arc::new(AtomicUsize::new(0));
        let jitted = run_module_tier(
            Tier::BaselineJit,
            &module,
            "plus",
            2,
            &args,
            counting_arith_hook(Arc::clone(&jitted_calls)),
        );
        // The fallback is the interpreter itself, so the hook ran exactly once...
        assert_eq!(
            jitted_calls.load(Ordering::SeqCst),
            1,
            "the rejected guard must fall back to the BIF"
        );
        // ... and the two tiers agree on the result and the typed error.
        assert!(matches!(jitted, Err(BeamError::BadArith)), "got {jitted:?}");
        assert_eq!(format!("{jitted:?}"), format!("{interpreted:?}"));
    }
}

/// Compile `module` once, then run `function/arity` on **fresh machines** twice via
/// [`JitBackend::run_compiled`] (no recompilation) and assert both runs yield
/// `expected`; finally assert the provided one-shot [`JitBackend::run_export`]
/// agrees. Generic over the backend so both native tiers are exercised the same way.
fn compile_once_run_many<B: JitBackend>(
    backend: &mut B,
    module: &Module,
    function: Atom,
    arity: u32,
    args: &[Eterm],
    expected: Eterm,
) {
    let compiled = backend.compile(module).expect("compile the module");

    for run in 0..2 {
        let mut machine = Machine::new();
        let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
        let got = backend
            .run_compiled(&compiled, &mut machine, module, function, arity, args, &mut ctx)
            .unwrap_or_else(|err| panic!("run_compiled, run {run}: {err:?}"));
        assert_eq!(got, expected, "run {run} of the same artifact");
    }

    // A missing export on the compile-once path is a typed error, never a panic.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    assert!(
        backend
            .run_compiled(&compiled, &mut machine, module, Atom::MAX, 0, &[], &mut ctx)
            .is_err(),
        "a missing export must be a typed error on the compile-once path"
    );

    // The one-shot entry is `compile` + `run_compiled`: same observable result.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let one_shot = backend
        .run_export(&mut machine, module, function, arity, args, &mut ctx)
        .expect("run_export");
    assert_eq!(one_shot, expected, "run_export must agree with run_compiled");
}

#[test]
fn a_compiled_artifact_can_be_run_more_than_once() {
    let module = slice_module();
    let add = atom(&module, "add");
    let fib = atom(&module, "fib");
    let add_args = [make_small(2), make_small(3)];
    let fib_args = [make_small(10)];

    // The interpreter is the reference semantics.
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let add_expected = machine
        .run_export(&module, add, 2, &add_args, &mut ctx)
        .expect("interpreter add/2");
    assert_eq!(add_expected, make_small(5));
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let fib_expected = machine
        .run_export(&module, fib, 1, &fib_args, &mut ctx)
        .expect("interpreter fib/1");
    assert_eq!(fib_expected, make_small(55));

    compile_once_run_many(&mut BaselineJit::new(), &module, add, 2, &add_args, add_expected);
    compile_once_run_many(&mut BaselineJit::new(), &module, fib, 1, &fib_args, fib_expected);

    #[cfg(feature = "jit-cranelift")]
    compile_once_run_many(
        &mut beam_engine::jit::cranelift::CraneliftJit::new(),
        &module,
        add,
        2,
        &add_args,
        add_expected,
    );
}

// ---------------------------------------------------------------------------
// Registers beyond the cache budget: direct `machine.x` addressing
// ---------------------------------------------------------------------------

/// The number of x-registers the baseline block emitter holds in machine registers
/// (`CACHE_REGISTERS`). Stated here as a literal so the tests below say which budget
/// they are exceeding; the structural assertions verify the allocator agrees.
///
/// The allocator keeps the `CACHE_BUDGET` x-registers with the **longest live intervals** —
/// `[first touch, last touch]` over the block's inlinable instructions, ties broken by
/// ascending register index — and addresses every other register it names directly in
/// `machine.x`.
const CACHE_BUDGET: u32 = 5;

/// A hand-built module with a single exported `export/arity` whose whole body is one
/// basic block (the loader is not involved). `imports` are the module's
/// `{module,function,arity}` imports, in `ImportRef` order; `build_code` runs once the
/// module name and imports are in place, so it can intern the atoms its instructions
/// reference.
fn arity_module<F>(
    name: &str,
    export: &str,
    arity: u32,
    imports: &[(&str, &str, u32)],
    build_code: F,
) -> Module
where
    F: FnOnce(&mut Module) -> Vec<Instr>,
{
    let mut module = Module::default();
    module.name = module.atom_table.intern(name);
    for (module_name, function_name, import_arity) in imports {
        let import_module = module.atom_table.intern(module_name);
        let import_function = module.atom_table.intern(function_name);
        module.imports.push(Import {
            module: import_module,
            function: import_function,
            arity: *import_arity,
        });
    }
    let function = module.atom_table.intern(export);
    let code = build_code(&mut module);
    let label = Label(1);
    let (blocks, labels) = beam_loader::lower::build_blocks(&code);
    module.functions.push(Function {
        func_label: label,
        entry_label: label,
        module: module.name,
        name: function,
        arity,
        code,
        blocks,
        labels,
    });
    module.exports.push(Export {
        function,
        arity,
        label,
    });
    module
}

/// [`arity_module`] with an arity-0 export — the shape most fixtures below take.
fn block_module(
    name: &str,
    export: &str,
    imports: &[(&str, &str, u32)],
    code: Vec<Instr>,
) -> Module {
    arity_module(name, export, 0, imports, |_| code)
}

/// `spread/0` — one straight-line block that reads and writes **eight** x-registers,
/// well past the five the emitter caches, so the shortest-lived ones live only in
/// `machine.x`:
///
/// ```text
///   x0=1 x1=2 x2=3 x3=4 x4=10     ; x4 is touched again and again below
///   x5 = x4                       ; `machine.x` → `machine.x`
///   x6 = x0 + x1          (= 3)   ; fast path with a `machine.x` destination
///   x4 = x6 + x3          (= 7)   ; fast path with `machine.x` operands and dst
///   swap x4 x5                    ; x4=10, x5=7 (both `machine.x`)
///   x7 = x4 + x5          (= 17)
///   x0 = x7                       ; `machine.x` → cached register
/// ```
///
/// Returns `17`.
fn spread_module() -> Module {
    let mv = |value: i64, x: u32| Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(x),
    };
    let add = |a: u32, b: u32, dst: u32| Instr::GcBif {
        arity: 2,
        bif: ImportRef(0),
        fail: NO_FAIL,
        live: dst.max(a).max(b) + 1,
        args: vec![Operand::X(a), Operand::X(b)],
        dst: Operand::X(dst),
    };
    block_module(
        "spread",
        "spread",
        &[("erlang", "+", 2)],
        vec![
            Instr::Label(Label(1)),
            mv(1, 0),
            mv(2, 1),
            mv(3, 2),
            mv(4, 3),
            mv(10, 4),
            Instr::Move {
                src: Operand::X(4),
                dst: Operand::X(5),
            },
            add(0, 1, 6),
            add(6, 3, 4),
            Instr::Swap {
                a: Operand::X(4),
                b: Operand::X(5),
            },
            add(4, 5, 7),
            Instr::Move {
                src: Operand::X(7),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    )
}

/// `swaps/0` — one block exercising `swap` over six x-registers:
///
/// ```text
///   x0=1 x1=2 x2=3 x3=4 x4=5 x5=6
///   swap x0 x4
///   swap x4 x5
///   swap x0 x1
///   swap x4 x3
///   x0 = x4
/// ```
///
/// Returns `4` (the value `x(3)` started with, carried through the last swap).
///
/// The live-interval allocator caches `{x0, x1, x3, x4, x5}` for this block, so every
/// operand here lands in a machine register; the `machine.x` operand combinations of
/// `swap` are covered by [`swaps_memory_module`].
fn swaps_module() -> Module {
    let mv = |value: i64, x: u32| Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(x),
    };
    let swap = |a: u32, b: u32| Instr::Swap {
        a: Operand::X(a),
        b: Operand::X(b),
    };
    block_module(
        "swaps",
        "swaps",
        &[],
        vec![
            Instr::Label(Label(1)),
            mv(1, 0),
            mv(2, 1),
            mv(3, 2),
            mv(4, 3),
            mv(5, 4),
            mv(6, 5),
            swap(0, 4),
            swap(4, 5),
            swap(0, 1),
            swap(4, 3),
            Instr::Move {
                src: Operand::X(4),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    )
}

/// `wide/0` — six x-registers whose live intervals make the allocator's choice **differ**
/// from first-appearance order, so `x(5)` — a register a four-slot first-appearance
/// allocator could never hold — becomes resident:
///
/// ```text
///   x0=1 x1=2 x2=3 x3=4 x4=5      ; five initial moves (one touch each)
///   x5 = x0 + x1                  ; x5 is first touched only here
///   x5 = x0 + x2
///   x5 = x0 + x3
///   x0 = x5                       ; x0's interval reaches the end of the block
/// ```
///
/// Live intervals, as `[first, last]` (length): `x0=[1,9]` (9), `x1=[2,6]` (5), `x2=[3,7]`
/// (5), `x3=[4,8]` (5), `x5=[6,9]` (4), `x4=[5,5]` (1). The allocator keeps the five longest
/// — `{x0, x1, x2, x3, x5}` — and leaves `x4` (the shortest-lived register, index 4) in
/// `machine.x`, even though its index is lower than the cached `x5`. Returns `5`.
fn wide_module() -> Module {
    let mv = |value: i64, x: u32| Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(x),
    };
    let add = |a: u32, b: u32, dst: u32| Instr::GcBif {
        arity: 2,
        bif: ImportRef(0),
        fail: NO_FAIL,
        live: dst.max(a).max(b) + 1,
        args: vec![Operand::X(a), Operand::X(b)],
        dst: Operand::X(dst),
    };
    block_module(
        "wide",
        "wide",
        &[("erlang", "+", 2)],
        vec![
            Instr::Label(Label(1)),
            mv(1, 0),
            mv(2, 1),
            mv(3, 2),
            mv(4, 3),
            mv(5, 4),
            add(0, 1, 5),
            add(0, 2, 5),
            add(0, 3, 5),
            Instr::Move {
                src: Operand::X(5),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    )
}

/// `swaps_memory/0` — one block that exercises **every** cached/`machine.x` operand
/// combination of `swap`, by leaving `x(5)`/`x(6)` — the block's two shortest live
/// intervals — outside the register cache:
///
/// ```text
///   x0..x4 = 1..5, x5 = 6, x6 = 7
///   <15 `+`/2 padding instructions>   ; broaden the x0..x4 live intervals
///   swap x0 x5   ; cached       <-> machine.x
///   swap x5 x6   ; machine.x    <-> machine.x
///   swap x0 x1   ; cached       <-> cached
///   swap x5 x3   ; machine.x    <-> cached
///   x0 = x5
///   x4 = x2 + x0 ; the last touch of x4, keeping its interval in the top five
/// ```
///
/// `x5` is live over `[6, 27]` (length 22) and `x6` over `[7, 24]` (length 18) — the two
/// shortest intervals of the seven registers the block names — so the allocator's five
/// residents are `{x0, x1, x2, x3, x4}` and both swap operands that must be read/written
/// through `machine.x` really are. (The exact residents are pinned by
/// `the_multi_register_blocks_cache_the_longest_lived_registers`.)
fn swaps_memory_module() -> Module {
    let mv = |value: i64, x: u32| Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(x),
    };
    let add = |a: u32, b: u32, dst: u32| Instr::GcBif {
        arity: 2,
        bif: ImportRef(0),
        fail: NO_FAIL,
        live: dst.max(a).max(b) + 1,
        args: vec![Operand::X(a), Operand::X(b)],
        dst: Operand::X(dst),
    };
    let swap = |a: u32, b: u32| Instr::Swap {
        a: Operand::X(a),
        b: Operand::X(b),
    };

    let mut code = vec![Instr::Label(Label(1))];
    for x in 0..5 {
        code.push(mv(i64::from(x) + 1, x));
    }
    code.push(mv(6, 5));
    code.push(mv(7, 6));
    // Pad x0..x4 so that the swap operands x5/x6 keep the two shortest live intervals:
    // each of the five `+`/2 instructions touches three of x0..x4 once, so three rounds
    // spread x0..x4's touches across the block interior while x5/x6 are named only near
    // the ends.
    for _ in 0..3 {
        for (a, b, dst) in [(0, 1, 2), (2, 3, 4), (4, 0, 1), (1, 2, 3), (3, 4, 0)] {
            code.push(add(a, b, dst));
        }
    }
    code.extend([
        swap(0, 5),
        swap(5, 6),
        swap(0, 1),
        swap(5, 3),
        Instr::Move {
            src: Operand::X(5),
            dst: Operand::X(0),
        },
        // The last touch of x4, which lengthens its live interval past x5's (`[6, 27]`,
        // length 22) and x6's (`[7, 24]`, length 18) — so the five residents are
        // `{x0..x4}` and both swap operands stay addressable through `machine.x`.
        add(2, 0, 4),
        Instr::Return,
    ]);
    block_module(
        "swaps_memory",
        "swaps_memory",
        &[("erlang", "+", 2)],
        code,
    )
}

#[test]
fn the_multi_register_blocks_cache_the_longest_lived_registers() {
    // Structural evidence for the live-interval allocator. The residents are the
    // `CACHE_BUDGET` registers with the **longest live intervals** `[first touch, last
    // touch]` over the block (ties by ascending index, slots ordered by first appearance),
    // so a register the budget cannot hold is addressed directly in `machine.x` — no
    // matter how low its index.
    //
    // `spread/0` live intervals, as `[first, last]` (length): x0=[1,11] (11), x1=[2,7] (6),
    // x4=[5,10] (6), x3=[4,8] (5), x5=[6,10] (5), x6=[7,8] (2), x7=[10,11] (2), x2=[3,3]
    // (1) — the five longest are {x0, x1, x3, x4, x5}; x2 stays in `machine.x`.
    let compiled = BaselineJit::new()
        .compile(&spread_module())
        .expect("compile the spread fixture");
    assert_eq!(compiled.block_count(), 1);
    assert_eq!(compiled.cached_registers(), &[0u32, 1, 3, 4, 5]);
    assert_eq!(compiled.highest_cached_register(), Some(5));
    assert_eq!(compiled.highest_native_register(), Some(7));
    assert!(
        compiled.highest_native_register() > compiled.highest_cached_register(),
        "the block must touch registers the cache budget could not hold"
    );

    // `swaps/0` live intervals (length): x0=[1,11] (11), x1=[2,9] (8), x3=[4,10] (7),
    // x4=[5,11] (7), x5=[6,8] (3), x2=[3,3] (1) — residents {x0, x1, x3, x4, x5};
    // x2 stays in `machine.x`.
    let compiled = BaselineJit::new()
        .compile(&swaps_module())
        .expect("compile the swaps fixture");
    assert_eq!(compiled.cached_registers(), &[0u32, 1, 3, 4, 5]);
    assert_eq!(compiled.highest_cached_register(), Some(5));
    assert_eq!(compiled.highest_native_register(), Some(5));

    // `swaps_memory/0` broadens x0..x4's live intervals (the padding plus the trailing
    // `x4 = x2 + x0`), so the residents are exactly {x0, x1, x2, x3, x4} and x5
    // ([6,27], length 22) / x6 ([7,24], length 18) — the two shortest — are read and
    // written through `machine.x`, which is what makes the swap combinations above real.
    let compiled = BaselineJit::new()
        .compile(&swaps_memory_module())
        .expect("compile the swaps_memory fixture");
    assert_eq!(compiled.cached_registers(), &[0u32, 1, 2, 3, 4]);
    assert_eq!(compiled.highest_cached_register(), Some(4));
    assert_eq!(compiled.highest_native_register(), Some(6));
    assert!(
        !compiled.cached_registers().contains(&5) && !compiled.cached_registers().contains(&6),
        "x5/x6 must stay in machine.x for the swap combinations to be exercised"
    );
}

#[test]
fn the_widened_pool_caches_a_register_the_old_budget_could_not() {
    // A concrete widening proof: `wide/0` names six x-registers and the allocator keeps
    // five of them, including x5 — which the previous four-slot **first-appearance**
    // allocator (cache x0..x3 as they appear, full after four) could never hold. It is
    // precisely the fifth slot (the released `rbp`) that makes x5 resident.
    let compiled = BaselineJit::new()
        .compile(&wide_module())
        .expect("compile the wide fixture");
    assert_eq!(
        compiled.cached_registers(),
        &[0u32, 1, 2, 3, 5],
        "the five longest-lived registers, x4 excluded"
    );
    assert_eq!(
        compiled.cached_registers().len() as u32,
        CACHE_BUDGET,
        "the allocator fills the widened pool exactly"
    );
    assert_eq!(compiled.highest_cached_register(), Some(5));
    assert!(
        !compiled.cached_registers().contains(&4),
        "x4 is the least-touched register and stays in machine.x"
    );
    assert_eq!(compiled.highest_native_register(), Some(5));
}

#[test]
fn multi_register_moves_and_arithmetic_agree_with_the_interpreter() {
    let spread = spread_module();
    assert_eq!(
        agree_module(&spread, "spread", 0, &[]).expect("spread/0"),
        make_small(17)
    );
    let swaps = swaps_module();
    assert_eq!(
        agree_module(&swaps, "swaps", 0, &[]).expect("swaps/0"),
        make_small(4)
    );
    let wide = wide_module();
    assert_eq!(
        agree_module(&wide, "wide", 0, &[]).expect("wide/0"),
        make_small(5)
    );
    // `swaps_memory/0`'s padding produces a value that depends on the arithmetic the
    // fixture performs; the point of the case is the agreement of the two tiers over
    // the `machine.x` swap combinations, so the exact value is not pinned.
    let swaps_memory = swaps_memory_module();
    agree_module(&swaps_memory, "swaps_memory", 0, &[]).expect("swaps_memory/0");
}

/// `handoff/0` — a block that **deopts mid-block** while cached x-registers hold values
/// the inline prefix wrote. The tier's whole residency model is that the interpreter
/// helper sees `machine.x` at every instruction boundary, so the `spill` before the
/// helper and the `fill` after it are what carry a resident's value across a deopt:
///
/// ```text
///   x0 = 7                  ; inlined `move` — x0 becomes a resident, machine.x[x0] stale
///   put_tuple2 x1 [x0, x0]  ; DEOPT — the helper builds {7, 7} and writes machine.x[x1]
///   x0 = x1                 ; inlined `move` — reads resident x1
/// ```
///
/// Both registers fit the five-slot budget, so the block is entered natively. A missing
/// `fill` after the helper would leave the x1 resident holding its **pre-helper** word,
/// and the answer would be that stale word instead of `{7, 7}` — exactly the divergence
/// the interpreter-agreement check below catches.
fn handoff_module() -> Module {
    block_module(
        "handoff",
        "handoff",
        &[],
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::PutTuple2 {
                dst: Operand::X(1),
                elements: vec![Operand::X(0), Operand::X(0)],
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    )
}

#[test]
fn an_inlined_resident_survives_a_mid_block_deopt() {
    let module = handoff_module();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile the handoff fixture");
    assert_eq!(compiled.block_count(), 1);
    assert_eq!(
        compiled.cached_registers(),
        &[0u32, 1],
        "both registers must be resident for the cross-deopt handoff to matter"
    );

    // The produced tuple lives in the run context's arena, so the context must outlive
    // the comparison; the expected tuple is built in an independent arena.
    let mut expected_heap = Heap::new(64);
    let expected = expected_heap
        .tuple(&[make_small(7), make_small(7)])
        .expect("the expected {7, 7}");

    let mut interpreted = Machine::new();
    let mut interp_ctx = MinimalContext::new();
    let interpreted_value = interpreted
        .run_with_tier(
            &module,
            atom(&module, "handoff"),
            0,
            &[],
            &mut interp_ctx,
            Tier::Interpreter,
        )
        .expect("handoff/0 on the interpreter");

    let mut jitted = Machine::new();
    let mut jit_ctx = MinimalContext::new();
    let jitted_value = jitted
        .run_with_tier(
            &module,
            atom(&module, "handoff"),
            0,
            &[],
            &mut jit_ctx,
            Tier::BaselineJit,
        )
        .expect("handoff/0 on the native tier");

    assert!(
        beam_term::eq(jitted_value, expected, &module.atom_table),
        "the x1 resident must be refreshed from machine.x after the mid-block deopt"
    );
    assert!(
        beam_term::eq(interpreted_value, jitted_value, &module.atom_table),
        "the interpreter and the native tier must agree across the deopt"
    );
}

#[test]
fn the_multi_register_fast_path_does_not_consult_the_bif_hook() {
    // All three `gc_bif '+'/2`s read their operands (and write the result) through
    // `machine.x` — they are past the cache budget — and are still lowered
    // natively: the BIF hook the interpreter would call is never reached. A deopt
    // to `jit_step_one` would call it.
    let module = spread_module();
    let calls = Arc::new(AtomicUsize::new(0));
    let got = run_module_tier(
        Tier::BaselineJit,
        &module,
        "spread",
        0,
        &[],
        counting_arith_hook(Arc::clone(&calls)),
    )
    .expect("spread/0 on the native tier");
    assert_eq!(got, make_small(17));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "the multi-register fast path must not call the BIF"
    );

    // The interpreter is the control: it calls the BIF once per `gc_bif`.
    let interpreted_calls = Arc::new(AtomicUsize::new(0));
    let interpreted = run_module_tier(
        Tier::Interpreter,
        &module,
        "spread",
        0,
        &[],
        counting_arith_hook(Arc::clone(&interpreted_calls)),
    )
    .expect("spread/0 on the interpreter");
    assert_eq!(interpreted, got);
    assert_eq!(interpreted_calls.load(Ordering::SeqCst), 3);
}

#[test]
fn multi_register_blocks_agree_across_register_file_sizes() {
    // `with_max_register(8)` covers x(0)..x(7), so the block runs natively with
    // x(4)..x(7) addressed in `machine.x`; a shorter file than the highest
    // natively-touched register must not be entered at all (the run stays on the
    // interpreter step and reports the interpreter's exact `bad_register`). Either
    // way the two tiers must agree.
    for module in [spread_module(), swaps_module()] {
        for max_register in [2u32, CACHE_BUDGET, 8, beam_engine::MAX_REGISTER] {
            let export = module.atom_name(module.exports[0].function).unwrap().to_string();

            let mut interpreted = Machine::with_max_register(max_register);
            let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
            let expected = interpreted.run_export(
                &module,
                atom(&module, &export),
                0,
                &[],
                &mut ctx,
            );

            let mut jitted = Machine::with_max_register(max_register);
            let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
            let got = jitted.run_with_tier(
                &module,
                atom(&module, &export),
                0,
                &[],
                &mut ctx,
                Tier::BaselineJit,
            );

            match (got, expected) {
                (Ok(got), Ok(expected)) => assert_eq!(got, expected),
                (Err(got), Err(expected)) => assert_eq!(
                    format!("{got:?}"),
                    format!("{expected:?}"),
                    "tiers disagree on the typed error for {export} with max_register={max_register}"
                ),
                (got, expected) => panic!(
                    "tiers disagree for {export} with max_register={max_register}: \
                     {got:?} vs {expected:?}"
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The exact-value switch (`select_val`)
// ---------------------------------------------------------------------------

/// `select/1` — a `select_val` on the argument with one case per immediate the tier
/// inlines (a small int, another small int, an atom, and `nil`), plus a fail arm:
///
/// ```text
///   select_val x0 L10 {1 -> L2; 2 -> L3; case_a -> L4; nil -> L5}
///   L2: x0 = 100   L3: x0 = 200   L4: x0 = 300   L5: x0 = 400   L10: x0 = -1
/// ```
///
/// (The case order matters: the interpreter takes the first match, exactly as the
/// emitted `cmp`/`jcc` chain does.)
fn select_module() -> Module {
    let mv = |value: i64, x: u32| Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(x),
    };
    arity_module("select", "select", 1, &[], |module| {
        let case_a = module.atom_table.intern("case_a");
        vec![
            Instr::Label(Label(1)),
            Instr::SelectVal {
                value: Operand::X(0),
                fail: Label(10),
                pairs: vec![
                    (Operand::Integer(1), Label(2)),
                    (Operand::Integer(2), Label(3)),
                    (Operand::Atom(case_a), Label(4)),
                    (Operand::Nil, Label(5)),
                ],
            },
            Instr::Label(Label(2)),
            mv(100, 0),
            Instr::Return,
            Instr::Label(Label(3)),
            mv(200, 0),
            Instr::Return,
            Instr::Label(Label(4)),
            mv(300, 0),
            Instr::Return,
            Instr::Label(Label(5)),
            mv(400, 0),
            Instr::Return,
            Instr::Label(Label(10)),
            mv(-1, 0),
            Instr::Return,
        ]
    })
}

/// `select_big/1` — a `select_val` whose **first** case is a bignum constant (above
/// the 60-bit immediate range), so a plain word compare cannot reproduce `=:=` and
/// the whole switch must deopt to `jit_step_one`:
///
/// ```text
///   select_val x0 L10 {<bignum MAX_SMALL+1> -> L2; 2 -> L3}
///   L2: x0 = 100   L3: x0 = 200   L10: x0 = -1
/// ```
fn select_bignum_module() -> Module {
    let mv = |value: i64, x: u32| Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(x),
    };
    arity_module("select_big", "select_big", 1, &[], |_| {
        vec![
            Instr::Label(Label(1)),
            Instr::SelectVal {
                value: Operand::X(0),
                fail: Label(10),
                pairs: vec![
                    (Operand::Integer(MAX_SMALL + 1), Label(2)),
                    (Operand::Integer(2), Label(3)),
                ],
            },
            Instr::Label(Label(2)),
            mv(100, 0),
            Instr::Return,
            Instr::Label(Label(3)),
            mv(200, 0),
            Instr::Return,
            Instr::Label(Label(10)),
            mv(-1, 0),
            Instr::Return,
        ]
    })
}

/// `scrutinee/0` — the block's **only** register reference is a `select_val`'s
/// scrutinee, so the allocator caches `x(5)` exactly when the switch is inlined (a
/// deopted `select_val` contributes no native touch at all).
fn scrutinee_module() -> Module {
    arity_module("scrutinee", "scrutinee", 0, &[], |_| {
        vec![
            Instr::Label(Label(1)),
            Instr::SelectVal {
                value: Operand::X(5),
                fail: Label(10),
                pairs: vec![(Operand::Integer(1), Label(2))],
            },
            Instr::Label(Label(2)),
            Instr::Return,
            Instr::Label(Label(10)),
            Instr::Return,
        ]
    })
}

#[test]
fn a_select_val_inlines_and_caches_its_scrutinee() {
    // Structural evidence that the switch is lowered natively: `x(5)` is named by no
    // other inlinable instruction, so it appears in the allocator's resident set — and
    // the block's native-register bound covers it — only because the `select_val` was
    // inlined (a deopted instruction contributes no native touch).
    let compiled = BaselineJit::new()
        .compile(&scrutinee_module())
        .expect("compile the select_val scrutinee fixture");
    assert_eq!(compiled.cached_registers(), &[5u32]);
    assert_eq!(compiled.highest_cached_register(), Some(5));
    assert_eq!(compiled.highest_native_register(), Some(5));
}

#[test]
fn select_val_matches_the_interpreter_on_every_case_and_the_fail_path() {
    let module = select_module();

    // Each inlined alternative, then the no-match path (which falls through to the fail
    // label's target). `agree_module` asserts the two tiers produce the same value.
    for (arg, expected) in [
        (make_small(1), make_small(100)),
        (make_small(2), make_small(200)),
        (NIL, make_small(400)),
        (make_small(99), make_small(-1)),
        (make_small(-5), make_small(-1)),
    ] {
        assert_eq!(
            agree_module(&module, "select", 1, &[arg]).expect("select/1"),
            expected,
            "select/1 on {arg:?}"
        );
    }

    // The atom case: the argument is the very atom the case names (an atom is `=:=`
    // only to the identical immediate word).
    let case_a = module.atom_table.lookup("case_a").expect("case_a interned");
    assert_eq!(
        agree_module(&module, "select", 1, &[make_atom(case_a)]).expect("select/1"),
        make_small(300)
    );
    // A different atom is not `=:=` to `case_a` — it falls through to the fail arm.
    let other = module.atom_table.lookup("select").expect("select interned");
    assert_eq!(
        agree_module(&module, "select", 1, &[make_atom(other)]).expect("select/1"),
        make_small(-1)
    );
}

#[test]
fn select_val_with_a_bignum_case_deopts_and_still_agrees() {
    // A non-immediate case value keeps the whole switch on the interpreter step; the
    // tiers must still agree (and the interpreter's result is the reference).
    let module = select_bignum_module();
    // `1` matches neither the bignum nor `2` -> the fail arm.
    assert_eq!(
        agree_module(&module, "select_big", 1, &[make_small(1)]).expect("select_big/1"),
        make_small(-1)
    );
    // `2` matches the second (immediate) alternative even though the first cannot be
    // compared by word.
    assert_eq!(
        agree_module(&module, "select_big", 1, &[make_small(2)]).expect("select_big/1"),
        make_small(200)
    );
}

#[test]
fn the_inlined_select_val_fail_path_agrees_with_the_interpreter() {
    // `scrutinee/0` takes its scrutinee from the (NIL-initialised) `x(5)`, matches no
    // case, and returns through the fail arm — exercising the inlined switch's no-match
    // path directly.
    let module = scrutinee_module();
    assert_eq!(
        agree_module(&module, "scrutinee", 0, &[]).expect("scrutinee/0"),
        NIL
    );
}

// ---------------------------------------------------------------------------
// The guarded cons-cell readers (`get_list`/`get_hd`/`get_tl`)
// ---------------------------------------------------------------------------

/// The one-block list-read fixtures' shared shape: `read/1` reads the cons cell in
/// `x(0)` and returns one of its halves in `x(0)`.
///
/// Every fixture interns `badmatch` so the deopt path raises the compound
/// `{badmatch, Source}` reason (`crate::terms::badmatch_reason`) rather than the bare
/// `BeamError::BadMatch`.
fn split_head_module() -> Module {
    arity_module("split_head", "read", 1, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
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
    })
}

/// `read/1` — `get_list x0 x1 x2` with the **CDR** returned.
fn split_tail_module() -> Module {
    arity_module("split_tail", "read", 1, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
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
    })
}

/// `read/1` — `get_hd x0 x1` (the CAR).
fn head_module() -> Module {
    arity_module("list_head", "read", 1, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::GetHd {
                source: Operand::X(0),
                head: Operand::X(1),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    })
}

/// `read/1` — `get_tl x0 x1` (the CDR).
fn tail_module() -> Module {
    arity_module("list_tail", "read", 1, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::GetTl {
                source: Operand::X(0),
                tail: Operand::X(1),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    })
}

/// `read/1` — `get_list x0 x0 x0`: the source and both destinations are the same
/// register, so the return value pins the write order. The CAR is written first and
/// the CDR second, exactly as the interpreter's `write(head)` / `write(tail)` pair
/// does, so **the CDR** survives:
///
/// ```text
///   get_list x0 x0 x0     ; x0 = CAR, then x0 = CDR
///   return                ; the CDR
/// ```
fn alias_module() -> Module {
    arity_module("list_alias", "read", 1, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::GetList {
                source: Operand::X(0),
                head: Operand::X(0),
                tail: Operand::X(0),
            },
            Instr::Return,
        ]
    })
}

/// `read/1` — a list read whose source index lies **past `MAX_REGISTER`**: the tier
/// refuses the shape (the emitted code may not read out of `machine.x`), so the whole
/// instruction stays on the interpreter step, whose `read_operand` reports the
/// faithful `bad_register`:
///
/// ```text
///   get_list x<MAX_REGISTER> x1 x2   ; no addressable source -> deopt
///   return
/// ```
fn out_of_bound_module() -> Module {
    arity_module("list_out_of_bound", "read", 1, &[], |_| {
        vec![
            Instr::Label(Label(1)),
            Instr::GetList {
                source: Operand::X(beam_engine::MAX_REGISTER),
                head: Operand::X(1),
                tail: Operand::X(2),
            },
            Instr::Return,
        ]
    })
}

/// `read/6` — a list read whose **source and one destination live in `machine.x`**:
/// the sixth argument carries the cons cell in `x(5)`, and the `move` chain spreads
/// `x0..x4` across the block (`x0` is live `[1,10]`, `x1..x4` all start in its first half)
/// while `x5` is live only over the final two instructions — so the allocator's five
/// residents are `{x0..x4}` and `x5` is addressed directly in `machine.x`:
///
/// ```text
///   x5 = <the cons cell>      ; the sixth argument
///   move chain over x0..x4    ; the padding that keeps x0..x4 the longest-lived
///   get_list x5 x0 x5         ; CAR → x0 (cached), CDR → x5 (`machine.x`)
///   x0 = x5                   ; the CDR, read back out of `machine.x`
/// ```
///
/// Returns the CDR.
fn list_memory_module() -> Module {
    let mv = |src: u32, dst: u32| Instr::Move {
        src: Operand::X(src),
        dst: Operand::X(dst),
    };
    arity_module("list_memory", "read", 6, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            mv(0, 1),
            mv(1, 2),
            mv(2, 3),
            mv(3, 4),
            mv(4, 0),
            mv(0, 1),
            mv(2, 3),
            mv(4, 0),
            Instr::GetList {
                source: Operand::X(5),
                head: Operand::X(0),
                tail: Operand::X(5),
            },
            mv(5, 0),
            Instr::Return,
        ]
    })
}

#[test]
fn list_reads_match_the_interpreter_on_a_real_cons_cell() {
    // A `[CAR | CDR]` cell of each interesting shape: a proper pair, a one-element
    // list (whose CDR is `NIL`, not a cons cell — legal, because only the *source* is
    // gated), and an improper list.
    let mut heap = Heap::new(16);
    let tail = heap.cons(make_small(2), NIL).expect("a cons cell");
    let pair = heap.cons(make_small(1), tail).expect("a cons cell");
    let single = heap.cons(make_small(7), NIL).expect("a cons cell");
    let improper = heap
        .cons(make_small(3), make_small(4))
        .expect("a cons cell");

    let cases = [
        (split_head_module(), "get_list -> head", true),
        (split_tail_module(), "get_list -> tail", false),
        (head_module(), "get_hd", true),
        (tail_module(), "get_tl", false),
        (alias_module(), "get_list x0 x0 x0", false),
    ];
    for (module, what, returns_head) in cases {
        for (list, car, cdr) in [
            (pair, make_small(1), tail),
            (single, make_small(7), NIL),
            (improper, make_small(3), make_small(4)),
        ] {
            let expected = if returns_head { car } else { cdr };
            assert_eq!(
                agree_module(&module, "read", 1, &[list])
                    .unwrap_or_else(|err| panic!("{what} on {list:?}: {err:?}")),
                expected,
                "{what} on {list:?} (CAR {car:?}, CDR {cdr:?})"
            );
        }
    }
}

#[test]
fn the_list_reads_are_inlined_and_cache_their_registers() {
    // Structural evidence that the cons-cell reads are lowered natively: `x(2)` — the
    // `get_list` tail destination — is named by no other inlinable instruction of
    // `split_head/1`, so it appears in the allocator's resident set (and the block's
    // native-register bound covers it) only because the read was inlined. A deopted
    // instruction contributes no native touch at all.
    let compiled = BaselineJit::new()
        .compile(&split_head_module())
        .expect("compile the get_list fixture");
    assert_eq!(compiled.cached_registers(), &[0u32, 1, 2]);
    assert_eq!(compiled.highest_cached_register(), Some(2));
    assert_eq!(compiled.highest_native_register(), Some(2));
}

#[test]
fn a_list_read_addressing_machine_x_agrees_with_the_interpreter() {
    // `list_memory/6`'s residents are the five longest-lived registers `{x0..x4}` (x5 is
    // live only over its final two instructions), so both `x5` operands — the source
    // **and** the tail destination — are read and written directly in `machine.x`.
    let module = list_memory_module();
    let compiled = BaselineJit::new()
        .compile(&module)
        .expect("compile the machine.x list-read fixture");
    assert_eq!(
        compiled.cached_registers(),
        &[0u32, 1, 2, 3, 4],
        "x5 must stay in machine.x for the direct-addressing path to be exercised"
    );
    assert_eq!(compiled.highest_cached_register(), Some(4));
    assert_eq!(compiled.highest_native_register(), Some(5));

    let mut heap = Heap::new(16);
    let tail = heap.cons(make_small(2), NIL).expect("a cons cell");
    let list = heap.cons(make_small(1), tail).expect("a cons cell");
    let args = [
        make_small(0),
        make_small(0),
        make_small(0),
        make_small(0),
        make_small(0),
        list,
    ];
    assert_eq!(
        agree_module(&module, "read", 6, &args).expect("list_memory/6"),
        tail,
        "the CDR stored through machine.x must be the tail"
    );
}

#[test]
fn a_non_cons_source_deopts_and_agrees_with_the_interpreter() {
    // The guard can only *reject*: `[]`, a small integer, an atom and a boxed term (a
    // tuple) are not cons cells, so the block deopts to `jit_step_one`, whose
    // interpreter step raises the exact `{badmatch, Source}`.
    let mut heap = Heap::new(256);
    let tuple = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a tuple term");
    for (module, what) in [
        (split_head_module(), "get_list -> head"),
        (split_tail_module(), "get_list -> tail"),
        (head_module(), "get_hd"),
        (tail_module(), "get_tl"),
    ] {
        let badmatch = atom(&module, "badmatch");
        for source in [NIL, make_small(7), make_atom(atom(&module, "read")), tuple] {
            // The two tiers agree, and the native tier's error is a raise — not the
            // bare `BeamError::BadMatch` a missing `badmatch` atom would produce.
            let jitted = agree_module(&module, "read", 1, &[source]);
            assert!(
                matches!(jitted, Err(BeamError::Raise(_))),
                "{what} on {source:?}: expected the {{badmatch, Source}} raise, got {jitted:?}"
            );

            // ... and the raise carries the exact reason term, with the offending
            // source in it. The context is kept alive here so the reason term's arena
            // outlives the check.
            let mut machine = Machine::new();
            let mut ctx = MinimalContext::with_bif_hook(Box::new(checked_arith_hook));
            let error = machine
                .run_with_tier(
                    &module,
                    atom(&module, "read"),
                    1,
                    &[source],
                    &mut ctx,
                    Tier::BaselineJit,
                )
                .expect_err(&format!("{what} on {source:?} must raise"));
            let reason = match &error {
                BeamError::Raise(raise) => raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<Eterm>().copied()),
                _ => None,
            }
            .unwrap_or_else(|| panic!("{what} on {source:?}: no live reason term: {error:?}"));
            let expected = heap
                .tuple(&[make_atom(badmatch), source])
                .expect("the expected {badmatch, Source} reason");
            assert!(
                beam_term::eq(reason, expected, &module.atom_table),
                "{what} on {source:?}: the raise reason is not {{badmatch, Source}}"
            );
        }
    }
}

#[test]
fn a_list_read_with_an_out_of_bound_register_deopts_and_agrees_with_the_interpreter() {
    // A source register past `MAX_REGISTER` is not an addressable shape: the tier
    // refuses it **without emitting a byte**, so the block emits the interpreter step
    // for the whole instruction. The two tiers must agree on the faithful
    // `bad_register` the interpreter's `read_operand` reports — never a read past the
    // end of `machine.x`.
    let module = out_of_bound_module();
    let jitted = agree_module(&module, "read", 1, &[NIL]);
    assert!(
        matches!(jitted, Err(BeamError::BadArg)),
        "expected the interpreter's `bad_register` for an out-of-bound source, got {jitted:?}"
    );
}

// ---------------------------------------------------------------------------
// The guarded tuple readers (`get_tuple_element`/`select_tuple_arity`)
// ---------------------------------------------------------------------------

/// `read/2` — `get_tuple_element x1 Element x0`: element `Element` of the tuple in
/// `x(1)` is returned in `x(0)`. The second argument carries the tuple so that `x(1)`
/// is named by no other inlinable instruction of the fixture.
///
/// Every fixture interns `badmatch` so the deopt path raises the compound
/// `{badmatch, Source}` reason (`crate::terms::badmatch_reason`) rather than the bare
/// `BeamError::BadMatch`.
fn tuple_read_module(element: u32) -> Module {
    arity_module("tuple_read", "read", 2, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::GetTupleElement {
                source: Operand::X(1),
                element,
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    })
}

/// `read/1` — a tuple read whose **destination index lies past `MAX_REGISTER`**: the
/// tier refuses the shape (the emitted code may not write out of `machine.x`), so the
/// whole instruction stays on the interpreter step, whose `write_operand` reports the
/// faithful `bad_register`:
///
/// ```text
///   get_tuple_element x0 0 x<MAX_REGISTER>   ; no addressable destination -> deopt
///   return
/// ```
fn tuple_out_of_bound_module() -> Module {
    arity_module("tuple_out_of_bound", "read", 1, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::GetTupleElement {
                source: Operand::X(0),
                element: 0,
                dst: Operand::X(beam_engine::MAX_REGISTER),
            },
            Instr::Return,
        ]
    })
}

/// `read/2` — `select_tuple_arity x1 Fail {2 -> L2; 3 -> L3}`, the tuple's arity
/// deciding which arm runs:
///
/// ```text
///   select_tuple_arity x1 Fail {2 -> L2; 3 -> L3}
///   L2: x0 = 100   L3: x0 = 200   L10: x0 = -1
/// ```
///
/// (The alternative order matters: the interpreter takes the first match, exactly as
/// the emitted `cmp`/`jcc` chain does.) `fail` is the no-match label — `NO_FAIL` gives
/// the exhaustive switch, which *raises* `{badmatch, Tuple}` when no arity matches.
fn arity_switch_module(name: &str, fail: Label) -> Module {
    let mv = |value: i64, x: u32| Instr::Move {
        src: Operand::Integer(value),
        dst: Operand::X(x),
    };
    arity_module(name, "read", 2, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::SelectTupleArity {
                tuple: Operand::X(1),
                fail,
                destinations: vec![
                    (Operand::Integer(2), Label(2)),
                    (Operand::Integer(3), Label(3)),
                ],
            },
            Instr::Label(Label(2)),
            mv(100, 0),
            Instr::Return,
            Instr::Label(Label(3)),
            mv(200, 0),
            Instr::Return,
            Instr::Label(Label(10)),
            mv(-1, 0),
            Instr::Return,
        ]
    })
}

/// Assert the native tier's `read/2` on `source` raises the compound
/// `{badmatch, Source}` reason the interpreter's deopt step builds, `expected` being
/// that reason built in the caller's own heap (the raise's reason term lives in the
/// run's context, so the comparison must happen while it is still alive). Also asserts
/// the interpreter tier agrees — the same value *or* the same typed error.
fn assert_badmatch_raise(module: &Module, what: &str, source: Eterm, expected: Eterm) {
    let agreed = agree_module(module, "read", 2, &[NIL, source]);
    assert!(
        matches!(agreed, Err(BeamError::Raise(_))),
        "{what} on {source:?}: both tiers must raise {{badmatch, Source}}, got {agreed:?}"
    );

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(checked_arith_hook));
    let error = machine
        .run_with_tier(
            module,
            atom(module, "read"),
            2,
            &[NIL, source],
            &mut ctx,
            Tier::BaselineJit,
        )
        .expect_err("a rejected tuple read must raise");
    let reason = match &error {
        BeamError::Raise(raise) => raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>().copied()),
        _ => None,
    }
    .unwrap_or_else(|| panic!("{what} on {source:?}: no live reason term: {error:?}"));
    assert!(
        beam_term::eq(reason, expected, &module.atom_table),
        "{what} on {source:?}: the raise reason is not {{badmatch, Source}}"
    );
}

#[test]
fn tuple_reads_match_the_interpreter_on_real_tuples() {
    // Every in-range element index of a 3-tuple, on the native tier and on the
    // interpreter (the reference).
    let mut heap = Heap::new(64);
    let triple = heap
        .tuple(&[make_small(10), make_small(20), make_small(30)])
        .expect("a tuple term");
    for (element, expected) in [
        (0u32, make_small(10)),
        (1, make_small(20)),
        (2, make_small(30)),
    ] {
        let module = tuple_read_module(element);
        assert_eq!(
            agree_module(&module, "read", 2, &[NIL, triple])
                .unwrap_or_else(|err| panic!("get_tuple_element x1 {element} x0: {err:?}")),
            expected,
            "element {element} of a 3-tuple"
        );
    }
}

#[test]
fn an_out_of_range_tuple_element_deopts_and_agrees_with_the_interpreter() {
    // The arity bound is the second half of the guard: an element index at/past the
    // tuple's arity is rejected, and the helper raises the exact `{badmatch, Source}`.
    let mut heap = Heap::new(64);
    let empty = heap.tuple(&[]).expect("an arity-0 tuple");
    let pair = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a tuple term");

    for (what, element, source) in [
        ("element 0 of a 0-tuple", 0u32, empty),
        ("element 2 of a 2-tuple", 2, pair),
        ("element 5 of a 2-tuple", 5, pair),
    ] {
        let module = tuple_read_module(element);
        let expected = heap
            .tuple(&[make_atom(atom(&module, "badmatch")), source])
            .expect("the expected {badmatch, Source} reason");
        assert_badmatch_raise(&module, what, source, expected);
    }
}

#[test]
fn a_non_tuple_tuple_read_source_deopts_and_agrees_with_the_interpreter() {
    // The other guard half: `NIL`, a small integer, an atom and a cons cell are not
    // boxed arityvals, so the block deopts and the helper raises the exact
    // `{badmatch, Source}`.
    let mut heap = Heap::new(64);
    let cons = heap.cons(make_small(1), NIL).expect("a cons cell");
    let module = tuple_read_module(0);
    let read = make_atom(atom(&module, "read"));

    for source in [NIL, make_small(7), read, cons] {
        let expected = heap
            .tuple(&[make_atom(atom(&module, "badmatch")), source])
            .expect("the expected {badmatch, Source} reason");
        assert_badmatch_raise(&module, "non-tuple source", source, expected);
    }
}

#[test]
fn a_tuple_read_with_an_out_of_bound_destination_deopts_and_agrees_with_the_interpreter() {
    // A destination past `MAX_REGISTER` is not an addressable shape: the tier refuses
    // it **without emitting a byte**, so the block emits the interpreter step for the
    // whole instruction — whose `write_operand` reports the faithful `bad_register`,
    // never a write past the end of `machine.x`.
    let mut heap = Heap::new(64);
    let pair = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a tuple term");
    let module = tuple_out_of_bound_module();
    let jitted = agree_module(&module, "read", 1, &[pair]);
    assert!(
        matches!(jitted, Err(BeamError::BadArg)),
        "expected the interpreter's `bad_register` for an out-of-bound destination, got {jitted:?}"
    );
}

#[test]
fn the_tuple_read_is_inlined_and_caches_its_registers() {
    // Structural evidence that the tuple read is lowered natively: `x(1)` — the
    // source — is named by no other inlinable instruction of `tuple_read/2`, so it is
    // in the allocator's resident set only because the read was inlined. A deopted
    // instruction contributes no native touch at all.
    let compiled = BaselineJit::new()
        .compile(&tuple_read_module(1))
        .expect("compile the get_tuple_element fixture");
    assert_eq!(compiled.cached_registers(), &[0u32, 1]);
    assert_eq!(compiled.highest_cached_register(), Some(1));
    assert_eq!(compiled.highest_native_register(), Some(1));
}

#[test]
fn arity_switches_match_the_interpreter_on_real_tuples() {
    // Each alternative, and the fall-through to the fail label when no arity matches.
    let mut heap = Heap::new(64);
    let pair = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a tuple term");
    let triple = heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("a tuple term");
    let quad = heap
        .tuple(&[make_small(1), make_small(2), make_small(3), make_small(4)])
        .expect("a tuple term");

    let module = arity_switch_module("select_arity", Label(10));
    for (what, source, expected) in [
        ("arity 2", pair, make_small(100)),
        ("arity 3", triple, make_small(200)),
        ("arity 4 falls through to fail", quad, make_small(-1)),
    ] {
        assert_eq!(
            agree_module(&module, "read", 2, &[NIL, source])
                .unwrap_or_else(|err| panic!("{what}: {err:?}")),
            expected,
            "{what}"
        );
    }
}

#[test]
fn a_non_tuple_arity_switch_source_deopts_and_agrees_with_the_interpreter() {
    // A non-tuple source has no arity at all (C's `get_arity` sets the switch value to
    // `NIL`), so it matches no alternative and takes the ordinary fail branch; the
    // tier's guard can only reject the shape, so both tiers report the fail arm's
    // answer.
    let mut heap = Heap::new(64);
    let cons = heap.cons(make_small(1), NIL).expect("a cons cell");
    let module = arity_switch_module("select_arity", Label(10));
    let scrutinee = make_atom(atom(&module, "read"));

    for source in [NIL, make_small(7), scrutinee, cons] {
        let agreed = agree_module(&module, "read", 2, &[NIL, source]);
        assert_eq!(
            agreed.unwrap_or_else(|err| panic!("non-tuple source {source:?}: {err:?}")),
            make_small(-1),
            "a non-tuple source ({source:?}) must take the arity switch's fail branch"
        );
    }
}

#[test]
fn a_no_fail_arity_switch_deopts_and_agrees_with_the_interpreter() {
    // A `NO_FAIL` switch is refused by the plan (its no-match path *raises*
    // `{badmatch, Tuple}`), so the whole instruction stays on the interpreter step —
    // whose matching path must still agree, and whose no-match path must raise.
    let mut heap = Heap::new(64);
    let pair = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a tuple term");
    let quad = heap
        .tuple(&[make_small(1), make_small(2), make_small(3), make_small(4)])
        .expect("a tuple term");

    let module = arity_switch_module("select_arity_no_fail", NO_FAIL);
    assert_eq!(
        agree_module(&module, "read", 2, &[NIL, pair]).expect("arity 2"),
        make_small(100)
    );
    let raised = agree_module(&module, "read", 2, &[NIL, quad]);
    assert!(
        matches!(raised, Err(BeamError::Raise(_))),
        "expected the exhaustive switch to raise {{badmatch, Tuple}}, got {raised:?}"
    );
}

#[test]
fn the_arity_switch_is_inlined_and_caches_its_registers() {
    // As for the tuple read: `x(1)` is named by no other inlinable instruction, so it
    // is resident only because the switch was lowered natively (its scrutinee is its
    // one native touch).
    let compiled = BaselineJit::new()
        .compile(&arity_switch_module("select_arity", Label(10)))
        .expect("compile the select_tuple_arity fixture");
    assert_eq!(compiled.cached_registers(), &[0u32, 1]);
    assert_eq!(compiled.highest_native_register(), Some(1));
}

// ---------------------------------------------------------------------------
// The tuple writer (`set_tuple_element`)
// ---------------------------------------------------------------------------
//
// `set_tuple_element Element Tuple Offset` overwrites one element word of a tuple **in
// place**. The tier inlines it behind the same `is_tuple` header guard the reads use
// plus a range and literal-area guard; a target that is not a uniquely-owned tuple, an
// out-of-range `offset` or a shared-literal-area term branches to the helper, whose
// interpreter step raises the exact `{badmatch, Source}` (or reports the typed
// `unsupported`). Every fixture interns `badmatch` for the same reason the read fixtures
// do.

/// `read/1` — build `{1, 1, 1}`, destructively overwrite one element, then read the
/// overwritten element back and return it:
///
/// ```text
///   put_tuple2 x1 {1, 1, 1}
///   set_tuple_element <element> x1 <offset>
///   get_tuple_element x1 <offset> x0
///   return
/// ```
///
/// The tuple is built **inside** the module (never passed in) so each tier run mutates
/// its own copy, and the returned element pins the store.
fn tuple_write_module(element: Operand, offset: u32) -> Module {
    arity_module("tuple_write", "read", 1, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::PutTuple2 {
                dst: Operand::X(1),
                elements: vec![Operand::Integer(1), Operand::Integer(1), Operand::Integer(1)],
            },
            Instr::SetTupleElement {
                element,
                tuple: Operand::X(1),
                offset,
            },
            Instr::GetTupleElement {
                source: Operand::X(1),
                element: offset,
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    })
}

/// `read/2` — `set_tuple_element <element> x1 <offset>`, the target tuple arriving as
/// the **second argument** (so a rejected target is the caller's own term, and the
/// `{badmatch, Source}` reason can be built in the caller's heap for comparison):
///
/// ```text
///   set_tuple_element <element> x1 <offset>
///   get_tuple_element x1 <offset> x0
///   return
/// ```
fn tuple_write_arg_module(element: Operand, offset: u32) -> Module {
    arity_module("tuple_write_arg", "read", 2, &[], |module| {
        module.atom_table.intern("badmatch");
        vec![
            Instr::Label(Label(1)),
            Instr::SetTupleElement {
                element,
                tuple: Operand::X(1),
                offset,
            },
            Instr::GetTupleElement {
                source: Operand::X(1),
                element: offset,
                dst: Operand::X(0),
            },
            Instr::Return,
        ]
    })
}

/// `read/2` — a lone `set_tuple_element x0 x1 0`: the only instruction of the block the
/// tier can lower natively, so the registers it caches are evidence the write was
/// inlined (a deopted instruction contributes no native touch at all).
///
/// ```text
///   set_tuple_element x0 x1 0
///   return
/// ```
fn tuple_write_only_module() -> Module {
    arity_module("tuple_write_only", "read", 2, &[], |_| {
        vec![
            Instr::Label(Label(1)),
            Instr::SetTupleElement {
                element: Operand::X(0),
                tuple: Operand::X(1),
                offset: 0,
            },
            Instr::Return,
        ]
    })
}

#[test]
fn tuple_writes_match_the_interpreter_on_real_tuples() {
    // Every offset of the built 3-tuple, with an **immediate** element operand (the
    // inlined path stores the constant word directly).
    for offset in 0u32..3 {
        let module = tuple_write_module(Operand::Integer(42), offset);
        assert_eq!(
            agree_module(&module, "read", 1, &[NIL])
                .unwrap_or_else(|err| panic!("set_tuple_element 42 x1 {offset}: {err:?}")),
            make_small(42),
            "offset {offset} of a built 3-tuple"
        );
    }
    // An **x-register** element operand — the argument itself.
    let module = tuple_write_module(Operand::X(0), 1);
    assert_eq!(
        agree_module(&module, "read", 1, &[make_small(99)]).expect("register element"),
        make_small(99)
    );
    // `NIL` is a constant immediate word too (not a pointer into a tuple).
    let module = tuple_write_module(Operand::Nil, 2);
    assert_eq!(
        agree_module(&module, "read", 1, &[NIL]).expect("nil element"),
        NIL
    );
}

#[test]
fn an_out_of_range_tuple_write_offset_deopts_and_agrees_with_the_interpreter() {
    // The offset bound is the second half of the guard: an offset at/past the tuple's
    // arity is rejected, and the helper raises the exact `{badmatch, Source}`.
    let mut heap = Heap::new(64);
    let empty = heap.tuple(&[]).expect("an arity-0 tuple");
    let pair = heap
        .tuple(&[make_small(1), make_small(2)])
        .expect("a tuple term");

    for (what, offset, source) in [
        ("offset 0 of a 0-tuple", 0u32, empty),
        ("offset 2 of a 2-tuple", 2, pair),
        ("offset 9 of a 2-tuple", 9, pair),
    ] {
        let module = tuple_write_arg_module(Operand::Integer(7), offset);
        let expected = heap
            .tuple(&[make_atom(atom(&module, "badmatch")), source])
            .expect("the expected {badmatch, Source} reason");
        assert_badmatch_raise(&module, what, source, expected);
    }
}

#[test]
fn a_non_tuple_tuple_write_target_deopts_and_agrees_with_the_interpreter() {
    // The other guard half: `NIL`, a small integer, an atom and a cons cell are not
    // boxed arityvals, so the block deopts and the helper raises the exact
    // `{badmatch, Source}`.
    let mut heap = Heap::new(64);
    let cons = heap.cons(make_small(1), NIL).expect("a cons cell");
    let module = tuple_write_arg_module(Operand::Integer(7), 0);
    let read = make_atom(atom(&module, "read"));

    for source in [NIL, make_small(7), read, cons] {
        let expected = heap
            .tuple(&[make_atom(atom(&module, "badmatch")), source])
            .expect("the expected {badmatch, Source} reason");
        assert_badmatch_raise(&module, "non-tuple target", source, expected);
    }
}

#[test]
fn a_literal_area_tuple_write_target_deopts_and_agrees_with_the_interpreter() {
    // A tuple in a shared literal area (the `TAG_LITERAL_PTR` bit on its boxed word) is
    // **not** writable: the interpreter refuses it with the typed `unsupported` rather
    // than corrupting a module's constants, and the tier's literal-area guard must
    // reject the same term — else it would write shared storage and diverge.
    let mut heap = Heap::new(64);
    let triple = heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("a tuple term");
    let literal = beam_term::make_literal(triple);
    assert!(beam_term::is_literal(literal), "the literal-area bit is set");

    let module = tuple_write_arg_module(Operand::Integer(7), 1);
    let agreed = agree_module(&module, "read", 2, &[NIL, literal]);
    assert!(
        matches!(agreed, Err(BeamError::Raise(_))),
        "a literal-area target must be refused by both tiers, got {agreed:?}"
    );
    // Neither tier wrote: the tuple is byte-identical to a freshly built `{1, 2, 3}`.
    let unchanged = heap
        .tuple(&[make_small(1), make_small(2), make_small(3)])
        .expect("the expected tuple");
    assert!(
        beam_term::eq(triple, unchanged, &module.atom_table),
        "a refused write must leave the tuple untouched"
    );
}

#[test]
fn the_tuple_write_is_inlined_and_caches_its_registers() {
    // Structural evidence that the tuple write is lowered natively: `x0` (the value) and
    // `x1` (the target) are named by no other inlinable instruction of
    // `tuple_write_only/2`, so they are resident only because the write was inlined. A
    // deopted instruction contributes no native touch, which would leave the set empty.
    let compiled = BaselineJit::new()
        .compile(&tuple_write_only_module())
        .expect("compile the set_tuple_element fixture");
    assert_eq!(compiled.cached_registers(), &[0u32, 1]);
    assert_eq!(compiled.highest_cached_register(), Some(1));
    assert_eq!(compiled.highest_native_register(), Some(1));
}
