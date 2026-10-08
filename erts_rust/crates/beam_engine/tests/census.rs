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

//! The interpreter's **execution census** over the frozen typed IR.
//!
//! `beam_loader::Instr` is a closed vocabulary of **106** variants (plus the
//! separate 25-kind `TestKind`). This file turns "is every variant reachable on a
//! real execution path?" into an executable claim instead of an untested assertion:
//!
//! 1. [`interpreter_execution_census_over_every_instr_variant`] builds a synthetic
//!    one-function module `[Label(1), <variant>, Label(2), return, Label(3),
//!    func_info, return]` for **every** variant, drives it through the real public
//!    entry point ([`Machine::run_label`]) with a [`MinimalContext`], and classifies
//!    the outcome:
//!    * **(a) EXECUTED** — the instruction produced a `Step` / a returned term
//!      (the module reached `Label(2)` and its `return`);
//!    * **(b) CLAIMED-BLOCKED** — a typed [`beam_common::BeamError`] that is **not**
//!      the unclaimed sentinel (a core arm's or a family's own capability error:
//!      `bad_register`, `badarg`, a `unsupported_opcode: <seam method>`, …) ⇒ some
//!      core arm or instruction family owns the variant;
//!    * **(c) UNCLAIMED** — the reason carries the sentinel
//!      [`UNCLAIMED_PREFIX`] that `families::dispatch` returns for an instruction
//!      **no** core arm and **no** family claims.
//!
//!    The census asserts (c) is **empty**: every one of the 106 variants is owned.
//!    At this revision the measured dynamic split is **49 EXECUTED + 57
//!    CLAIMED-BLOCKED + 0 UNCLAIMED** (pinned below as `EXECUTED_VARIANTS` /
//!    `CLAIMED_BLOCKED_VARIANTS`). Many CLAIMED-BLOCKED rows are **probe artifacts**,
//!    not real gaps: the roster drives every atom-ish operand as the degenerate
//!    `Operand::Nil` (a real operand the family then rejects with a typed error), so
//!    e.g. `IsNativeRecord`/`IsRecordAccessible` report `… : module`/`… : scope`
//!    because the family correctly requires an atom the probe never supplies, and
//!    `PutRecord`'s `id: nil()` probe is neither an atom nor a `{Module,Name}` pair.
//!    Those variants have zero corpus occurrences (the installed toolchain does not
//!    enable the `native_record` feature); the real capability gaps are named in
//!    `tests/gaps.rs`.
//!
//! 2. [`interpreter_execution_census_over_every_test_kind`] does the same for all
//!    25 [`TestKind`]s packed into one `Instr::Test` each; all **25/25** execute
//!    (0 claimed-blocked, 0 unclaimed).
//!
//! 3. [`corpus_reachability_census`] loads **every** `.beam` of the whole installed
//!    OTP (the generic `support::all_otp_beams` walk — every application, its `ebin`
//!    modules plus any compiled examples it ships), walks every lowered instruction
//!    of every function, and counts occurrences per variant — the quantitative
//!    evidence for which variants real compiler output actually emits. It asserts
//!    that every variant with a non-zero corpus count is class (a) or (b) (nothing
//!    the corpus reaches may be unclaimed) and names the zero-occurrence variants
//!    explicitly. (This is the same file set `tests/stdlib_sweep.rs` proves loads in
//!    full — currently ≈1366 `.beam` over ≈34 applications.)
//!
//! # Synthetic-module mechanics
//!
//! `Machine::step_instruction` is `pub(crate)`, so an integration test cannot drive
//! one instruction directly. Instead each variant is placed as the **first**
//! instruction after its entry label and the run is bounded by two sinks:
//!
//! * `Label(2)` — the success sink, whose body is a bare `return` (the bottom frame
//!   pushed by `run_label` has the `Halt` continuation, so this finishes the run);
//! * `Label(3)` — the failure sink, whose body is `func_info` (which raises), used
//!   for the `call`-family targets so a call can never re-enter its own
//!   continuation slot and spin.
//!
//! Every operand that is a label therefore points at one of those two sinks, so a
//! variant that branches, calls, suspends or raises always terminates within a
//! bounded number of steps. Nothing here panics or hangs: a typed error is a
//! *result*, not a failure.
//!
//! Family assignment (static, from the crate sources): **30** variants are executed
//! by core arms of `src/interpreter.rs` (`Label, Line, IntCodeEnd, ExecutableLine,
//! DebugLine, NifStart, OnLoad, FuncInfo, Move, Swap, Test, SelectVal, Jump, GcBif,
//! Bif0, Bif, Call, CallLast, CallOnly, CallExt, CallExtOnly, CallExtLast, Allocate,
//! AllocateHeap, TestHeap, Deallocate, Trim, Return, InitYregs, Generic`); the other
//! **76** are claimed by exactly one of the nine families under `src/families/**`.
//! `Instr::Generic` is the residual-opcode sentinel: it is handled by
//! `src/generic.rs` (`error::unsupported(op.name())`), which is core, so it is
//! classed EXECUTED here with its typed error recorded in the detail column.
//!
//! # Why one long file
//!
//! The rosters ([`all_instr_variants`], [`the_test_kinds`]) are a hand-maintained
//! one-to-one mirror of a closed enum — 106 `Instr` constructions and 25 `TestKind`s
//! whose value *is* that they are exhaustive. They must live beside the census that
//! consumes them, and they cannot be factored into `tests/support/` (a shared module
//! owned by another test file); keeping every census artifact in this one file is
//! what makes the "106 rows / 106 distinct names" assertion a real completeness
//! check rather than a claim split across modules.

mod support;

use std::collections::{BTreeMap, BTreeSet};

use beam_common::Result;
use beam_engine::{Machine, MinimalContext};
use beam_loader::{
    load_file, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Opcode, Operand,
    TestKind,
};
use beam_term::{Atom, Eterm};
use support::{all_otp_beams, skip_reason};

/// The sentinel `beam_engine::families::dispatch` returns for an instruction that
/// **no** core arm and **no** instruction family claims.
///
/// It is `families::UNCLAIMED_PREFIX`, which is `pub(crate)` to the engine and so
/// cannot be imported here; the literal is reproduced verbatim (it is a stable,
/// documented diagnostic — see `src/families/mod.rs`).
const UNCLAIMED_PREFIX: &str = "widened opcode not yet executed";

/// The number of `beam_loader::Instr` variants this census must drive (the enum's
/// own roster; a mismatch means the hand-written roster below drifted).
const INSTR_VARIANTS: usize = 106;

/// The number of [`TestKind`]s (the enum's own roster).
const TEST_KINDS: usize = 25;

/// The measured **dynamic** classification of the [`INSTR_VARIANTS`] variants at
/// this census's revision: the probe reached a `Step`/return for **49** of them
/// (class (a) EXECUTED) and observed a typed, owned capability error for **57**
/// (class (b) CLAIMED-BLOCKED); **0** are unclaimed.
///
/// These are the *dynamic* counts, not the *static* family assignment (30 core arms
/// + 76 family-owned): a family-owned variant whose degenerate probe happens to
/// execute classifies as EXECUTED — `SelectTupleArity` on a non-tuple takes the
/// `fail` branch, exactly like an unmatched arity, and `GetRecordElements`/
/// `GetRecordElementsId`/`GetRecordField`/`UpdateRecordId` branch to `fail` on a
/// non-record source. The counts are pinned so a change of classification (a real
/// regression, or a newly executed variant) fails the build.
const EXECUTED_VARIANTS: usize = 49;
/// See [`EXECUTED_VARIANTS`]; class (b) CLAIMED-BLOCKED.
const CLAIMED_BLOCKED_VARIANTS: usize = 57;

// ---------------------------------------------------------------------------
// Classification
// ---------------------------------------------------------------------------

/// The three census buckets.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Class {
    /// (a) The instruction produced a `Step` / the module returned a term.
    Executed,
    /// (b) A typed error that is **not** the unclaimed sentinel ⇒ someone owns it.
    ClaimedBlocked,
    /// (c) The `families::dispatch` "no core arm, no family" sentinel.
    Unclaimed,
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Class::Executed => "EXECUTED",
            Class::ClaimedBlocked => "CLAIMED-BLOCKED",
            Class::Unclaimed => "UNCLAIMED",
        }
    }
}

/// One census row: a variant name, its bucket and the observed outcome.
struct Row {
    name: String,
    class: Class,
    detail: String,
}

/// Classify one run outcome.
///
/// `Ok(term)` is (a). An error is (c) when its text carries [`UNCLAIMED_PREFIX`],
/// else (b). The text is the error's `Debug` plus its BEAM atom name (when it has
/// one), so the sentinel — an `unsupported_opcode: widened opcode not yet executed:
/// <name>` atom — is always visible.
fn classify(result: Result<Eterm>) -> (Class, String) {
    match result {
        Ok(term) => (Class::Executed, format!("returned {term:?}")),
        Err(error) => {
            let mut text = format!("{error:?}");
            if let Some(atom) = error.as_atom() {
                text.push_str(" => ");
                text.push_str(atom);
            }
            let class = if text.contains(UNCLAIMED_PREFIX) {
                Class::Unclaimed
            } else {
                Class::ClaimedBlocked
            };
            (class, text)
        }
    }
}

// ---------------------------------------------------------------------------
// Naming (derived from the derived `Debug`, so it can never drift)
// ---------------------------------------------------------------------------

/// The leading identifier of a derived `Debug` rendering — i.e. the variant name.
fn debug_head(debug: &str) -> String {
    let end = debug
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .unwrap_or(debug.len());
    debug[..end].to_string()
}

/// The variant name of an [`Instr`] (`Instr::Move { .. }` ⇒ `"Move"`).
fn variant_name(instr: &Instr) -> String {
    debug_head(&format!("{instr:?}"))
}

/// The name of a [`TestKind`].
///
/// An **exhaustive** match (not `Debug`): a future `TestKind` variant fails to
/// compile here, forcing the census to be extended rather than silently shrinking.
fn test_kind_name(kind: TestKind) -> &'static str {
    match kind {
        TestKind::IsLt => "IsLt",
        TestKind::IsGe => "IsGe",
        TestKind::IsEq => "IsEq",
        TestKind::IsNe => "IsNe",
        TestKind::IsEqExact => "IsEqExact",
        TestKind::IsNeExact => "IsNeExact",
        TestKind::IsInteger => "IsInteger",
        TestKind::IsFloat => "IsFloat",
        TestKind::IsNumber => "IsNumber",
        TestKind::IsAtom => "IsAtom",
        TestKind::IsPid => "IsPid",
        TestKind::IsReference => "IsReference",
        TestKind::IsPort => "IsPort",
        TestKind::IsNil => "IsNil",
        TestKind::IsBinary => "IsBinary",
        TestKind::IsBitstr => "IsBitstr",
        TestKind::IsList => "IsList",
        TestKind::IsNonemptyList => "IsNonemptyList",
        TestKind::IsTuple => "IsTuple",
        TestKind::IsMap => "IsMap",
        TestKind::IsBoolean => "IsBoolean",
        TestKind::IsFunction => "IsFunction",
        TestKind::IsFunction2 => "IsFunction2",
        TestKind::IsTaggedTuple => "IsTaggedTuple",
        TestKind::TestArity => "TestArity",
    }
}

// ---------------------------------------------------------------------------
// The synthetic census module + runner
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index (the loader's contract).
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// Build a one-function module whose code is
/// `[Label(1), <variant>, Label(2), return, Label(3), func_info, return]`.
///
/// The entry label is `Label(1)`; `Label(2)` is the success sink (a bare `return`
/// on the bottom frame — the `Halt` continuation — which finishes the run) and
/// `Label(3)` is the failure sink (`func_info`, which raises). One import
/// (`erlang:self/0`) exists so an `ImportRef(0)` operand of the call/BIF variants
/// resolves; the `MinimalContext` default hook rejects it with a typed error.
fn census_module(variant: &Instr) -> Module {
    let mut module = Module::default();
    let name = module.atom_table.intern("census");
    let function = module.atom_table.intern("f");
    // Atoms the failure sink and a few raise paths resolve.
    for atom in ["function_clause", "error", "badarg"] {
        module.atom_table.intern(atom);
    }
    let bif_module = module.atom_table.intern("erlang");
    let bif_function = module.atom_table.intern("self");
    module.name = name;
    module.imports.push(Import {
        module: bif_module,
        function: bif_function,
        arity: 0,
    });

    let code = vec![
        Instr::Label(Label(1)),
        variant.clone(),
        Instr::Label(Label(2)),
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::FuncInfo {
            module: name,
            function,
            arity: 0,
        },
        Instr::Return,
    ];
    let labels = labels_of(&code);
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: name,
        name: function,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    });
    module
}

/// Drive `variant` through the real public execution entry and classify the
/// outcome.
///
/// The `MinimalContext` (no BIF hook beyond the default rejecting one, no mailbox)
/// is alive for the whole call, including the formatting of the outcome.
fn run_row(name: String, variant: Instr) -> Row {
    let is_generic = matches!(variant, Instr::Generic { .. });
    let module = census_module(&variant);

    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let result = machine.run_label(&module, Label(1), 0, &[], &mut ctx);

    let (mut class, mut detail) = classify(result);
    if is_generic {
        // `src/generic.rs`'s sentinel is core (the documented `unsupported(op.name())`
        // for a residual opcode, never the family-dispatch sentinel): the interpreter
        // arm is the one that executes, so this is an executed variant.
        class = Class::Executed;
        detail = format!("core sentinel — {detail}");
    }
    Row {
        name,
        class,
        detail,
    }
}

/// The census over every [`Instr`] variant, in the enum's own order.
fn variant_census() -> Vec<Row> {
    all_instr_variants()
        .into_iter()
        .map(|variant| {
            let name = variant_name(&variant);
            run_row(name, variant)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Test 1 — the per-variant execution census
// ---------------------------------------------------------------------------

#[test]
fn interpreter_execution_census_over_every_instr_variant() {
    let rows = variant_census();

    let mut executed = 0usize;
    let mut blocked = 0usize;
    let mut unclaimed = 0usize;
    for row in &rows {
        match row.class {
            Class::Executed => executed += 1,
            Class::ClaimedBlocked => blocked += 1,
            Class::Unclaimed => unclaimed += 1,
        }
        println!(
            "CENSUS-INSTR {:<22} {:<16} {}",
            row.name,
            row.class.label(),
            row.detail
        );
    }

    println!(
        "CENSUS: variants={} executed={} claimed-blocked={} unclaimed={}",
        rows.len(),
        executed,
        blocked,
        unclaimed
    );

    // The roster must be complete *and* duplicate-free: every construction is a real
    // variant, so 106 distinct names out of 106 variants is exhaustive coverage.
    let distinct: BTreeSet<&str> = rows.iter().map(|row| row.name.as_str()).collect();
    assert_eq!(
        rows.len(),
        INSTR_VARIANTS,
        "the census drove {} variants, expected {INSTR_VARIANTS}",
        rows.len()
    );
    assert_eq!(
        distinct.len(),
        INSTR_VARIANTS,
        "the census roster has {} distinct names for {} rows — a variant is missing \
         or duplicated",
        distinct.len(),
        rows.len()
    );

    let unclaimed_names: Vec<&str> = rows
        .iter()
        .filter(|row| row.class == Class::Unclaimed)
        .map(|row| row.name.as_str())
        .collect();
    assert!(
        unclaimed_names.is_empty(),
        "these {INSTR_VARIANTS} variant(s) have NO core arm and NO claiming family \
         (the `{UNCLAIMED_PREFIX}` sentinel): {unclaimed_names:?}"
    );

    // The dynamic split is pinned (see `EXECUTED_VARIANTS`): a newly executed
    // variant — or a family/core regression into a typed error — changes it and
    // must be an intentional, re-measured update.
    assert_eq!(
        executed, EXECUTED_VARIANTS,
        "the census executed {executed} variants, expected {EXECUTED_VARIANTS}"
    );
    assert_eq!(
        blocked, CLAIMED_BLOCKED_VARIANTS,
        "the census found {blocked} CLAIMED-BLOCKED variants, expected \
         {CLAIMED_BLOCKED_VARIANTS}"
    );
    assert_eq!(
        executed + blocked + unclaimed,
        INSTR_VARIANTS,
        "the census classes must partition all {INSTR_VARIANTS} variants"
    );
}

// ---------------------------------------------------------------------------
// Test 2 — the per-TestKind execution census
// ---------------------------------------------------------------------------

#[test]
fn interpreter_execution_census_over_every_test_kind() {
    let cases = test_kind_cases();
    let distinct: BTreeSet<&str> = cases.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        cases.len(),
        distinct.len(),
        "the TestKind roster repeats a kind"
    );

    let rows: Vec<Row> = cases
        .into_iter()
        .map(|(name, args)| {
            run_row(
                name.to_string(),
                Instr::Test {
                    kind: kind_of(name),
                    fail: Label(2),
                    args,
                },
            )
        })
        .collect();

    let mut executed = 0usize;
    let mut blocked = 0usize;
    let mut unclaimed = 0usize;
    for row in &rows {
        match row.class {
            Class::Executed => executed += 1,
            Class::ClaimedBlocked => blocked += 1,
            Class::Unclaimed => unclaimed += 1,
        }
        println!(
            "CENSUS-KIND {:<22} {:<16} {}",
            row.name,
            row.class.label(),
            row.detail
        );
    }
    println!(
        "CENSUS-TESTKIND: kinds={} executed={} claimed-blocked={} unclaimed={}",
        rows.len(),
        executed,
        blocked,
        unclaimed
    );

    let unclaimed_names: Vec<&str> = rows
        .iter()
        .filter(|row| row.class == Class::Unclaimed)
        .map(|row| row.name.as_str())
        .collect();
    assert!(
        unclaimed_names.is_empty(),
        "these TestKind(s) are not handled by the interpreter's test evaluation: \
         {unclaimed_names:?}"
    );

    // The roster must cover every kind, and all 25/25 must execute (0 claimed-blocked,
    // 0 unclaimed): the interpreter's test evaluation owns every `TestKind`.
    assert_eq!(
        rows.len(),
        TEST_KINDS,
        "the census drove {} TestKind(s), expected {TEST_KINDS}",
        rows.len()
    );
    assert_eq!(
        executed, TEST_KINDS,
        "only {executed} of {TEST_KINDS} TestKind(s) executed — every kind must"
    );
    assert_eq!(blocked, 0, "{blocked} TestKind(s) are CLAIMED-BLOCKED");
}

/// The `(name, operands)` pair per [`TestKind`], with a correct operand count so the
/// test evaluation reaches its predicate rather than a "missing argument" error.
fn test_kind_cases() -> Vec<(&'static str, Vec<Operand>)> {
    let one = || vec![Operand::Integer(1)];
    vec![
        ("IsLt", vec![Operand::Integer(1), Operand::Integer(2)]),
        ("IsGe", vec![Operand::Integer(1), Operand::Integer(2)]),
        ("IsEq", vec![Operand::Integer(1), Operand::Integer(2)]),
        ("IsNe", vec![Operand::Integer(1), Operand::Integer(2)]),
        ("IsEqExact", vec![Operand::Integer(1), Operand::Integer(2)]),
        ("IsNeExact", vec![Operand::Integer(1), Operand::Integer(2)]),
        ("IsInteger", one()),
        ("IsFloat", one()),
        ("IsNumber", one()),
        ("IsAtom", one()),
        ("IsPid", one()),
        ("IsReference", one()),
        ("IsPort", one()),
        ("IsNil", one()),
        ("IsBinary", one()),
        ("IsBitstr", one()),
        ("IsList", one()),
        ("IsNonemptyList", one()),
        ("IsTuple", one()),
        ("IsMap", one()),
        ("IsBoolean", one()),
        ("IsFunction", one()),
        ("IsFunction2", vec![Operand::Integer(1), Operand::Integer(0)]),
        (
            "IsTaggedTuple",
            vec![Operand::Integer(1), Operand::Integer(0), Operand::Nil],
        ),
        ("TestArity", vec![Operand::Integer(1), Operand::Integer(0)]),
    ]
}

/// The [`TestKind`] a roster name denotes (the inverse of [`test_kind_name`]).
fn kind_of(name: &str) -> TestKind {
    the_test_kinds()
        .into_iter()
        .find(|kind| test_kind_name(*kind) == name)
        .unwrap_or_else(|| panic!("unknown TestKind roster name {name:?}"))
}

// ---------------------------------------------------------------------------
// Test 3 — the corpus reachability census
// ---------------------------------------------------------------------------

/// Walk **every** `.beam` of the whole installed OTP (the generic
/// [`support::all_otp_beams`] walk — every application, its `ebin` modules plus any
/// compiled examples it ships), count the occurrences of every [`Instr`] variant
/// (and every [`TestKind`]), and assert that nothing the corpus reaches is unclaimed.
///
/// This is the same file set `tests/stdlib_sweep.rs::sweep_loads_every_installed_beam`
/// proves loads in full (currently ≈1366 `.beam` over ≈34 applications).
#[test]
fn corpus_reachability_census() {
    if let Some(reason) = skip_reason() {
        println!("{reason}");
        return;
    }

    let beams = all_otp_beams().unwrap_or_default();
    assert!(!beams.is_empty(), "no OTP `.beam` file was found to sweep");

    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut kind_counts: BTreeMap<&str, usize> = BTreeMap::new();
    let mut modules_total = 0usize;
    let mut modules_loaded = 0usize;
    let mut instr_total = 0usize;
    let mut per_app: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();

    for beam in &beams {
        modules_total += 1;
        *per_app.entry(beam.app.clone()).or_default() += 1;
        match load_file(&beam.path) {
            Ok(module) => {
                modules_loaded += 1;
                for function in &module.functions {
                    for instr in &function.code {
                        instr_total += 1;
                        *counts.entry(variant_name(instr)).or_default() += 1;
                        if let Instr::Test { kind, .. } = instr {
                            *kind_counts.entry(test_kind_name(*kind)).or_default() += 1;
                        }
                    }
                }
            }
            Err(error) => {
                println!("CENSUS-LOAD-FAIL {}/{}: {error:?}", beam.app, beam.module);
                failures.push(format!("{}/{}", beam.app, beam.module));
            }
        }
    }

    println!(
        "CENSUS corpus: modules={modules_loaded}/{modules_total} apps={} \
         instructions={instr_total}",
        per_app.len()
    );

    let rows = variant_census();
    let classes: BTreeMap<&str, Class> = rows
        .iter()
        .map(|row| (row.name.as_str(), row.class))
        .collect();

    // Every name the corpus produced must be one of the census rows: a name outside
    // the roster means the census roster is incomplete.
    let unknown: Vec<&str> = counts
        .keys()
        .map(String::as_str)
        .filter(|name| !classes.contains_key(name))
        .collect();
    assert!(
        unknown.is_empty(),
        "the corpus lowered variants absent from the census roster: {unknown:?}"
    );

    let mut zero: Vec<&str> = Vec::new();
    for row in &rows {
        let count = counts.get(&row.name).copied().unwrap_or(0);
        println!(
            "CENSUS-CORPUS {count:>8}  {:<22} [{}]",
            row.name,
            row.class.label()
        );
        if count == 0 {
            zero.push(row.name.as_str());
        }
    }

    let zero_kinds: Vec<&str> = the_test_kinds()
        .into_iter()
        .map(test_kind_name)
        .filter(|name| !kind_counts.contains_key(name))
        .collect();
    println!(
        "CENSUS-CORPUS-KINDS: {} of {} TestKind(s) present in the corpus ({} zero)",
        kind_counts.len(),
        the_test_kinds().len(),
        zero_kinds.len()
    );
    for (name, count) in &kind_counts {
        println!("CENSUS-CORPUS-KIND {count:>8}  {name}");
    }
    println!(
        "CENSUS-CORPUS-ZERO-VARIANTS ({}): {}",
        zero.len(),
        zero.join(", ")
    );
    println!(
        "CENSUS-CORPUS-ZERO-KINDS ({}): {}",
        zero_kinds.len(),
        zero_kinds.join(", ")
    );

    // Nothing the corpus reaches may be unclaimed.
    let unclaimed_reachable: Vec<&str> = counts
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(name, _)| name.as_str())
        .filter(|name| classes.get(name) == Some(&Class::Unclaimed))
        .collect();
    assert!(
        unclaimed_reachable.is_empty(),
        "corpus-reachable variants that NO core arm and NO family claims: \
         {unclaimed_reachable:?}"
    );

    assert!(modules_loaded > 0, "the corpus census loaded no modules");
    assert!(
        modules_total >= 1000,
        "the corpus census only found {modules_total} `.beam` files — expected the \
         whole installed OTP (≈1366 over ≈34 applications)"
    );
    assert!(
        per_app.len() >= 20,
        "the corpus census only covered {} applications — expected the whole \
         installed OTP (≈34)",
        per_app.len()
    );
    assert!(
        failures.is_empty(),
        "{} corpus module(s) failed to LOAD (the census would be incomplete): {failures:?}",
        failures.len()
    );
}

// ---------------------------------------------------------------------------
// The rosters
// ---------------------------------------------------------------------------

/// Every one of the 25 [`TestKind`]s, in the enum's own order.
///
/// Kept in step with the exhaustive [`test_kind_name`] match; a kind added to the
/// enum forces that match to be extended (compile error), and the roster assertion
/// in [`interpreter_execution_census_over_every_test_kind`] then catches an omission
/// here.
fn the_test_kinds() -> Vec<TestKind> {
    vec![
        TestKind::IsLt,
        TestKind::IsGe,
        TestKind::IsEq,
        TestKind::IsNe,
        TestKind::IsEqExact,
        TestKind::IsNeExact,
        TestKind::IsInteger,
        TestKind::IsFloat,
        TestKind::IsNumber,
        TestKind::IsAtom,
        TestKind::IsPid,
        TestKind::IsReference,
        TestKind::IsPort,
        TestKind::IsNil,
        TestKind::IsBinary,
        TestKind::IsBitstr,
        TestKind::IsList,
        TestKind::IsNonemptyList,
        TestKind::IsTuple,
        TestKind::IsMap,
        TestKind::IsBoolean,
        TestKind::IsFunction,
        TestKind::IsFunction2,
        TestKind::IsTaggedTuple,
        TestKind::TestArity,
    ]
}

/// Every one of the 106 [`Instr`] variants — one construction each, in the enum's
/// own order.
///
/// Operand choices are the *minimal well-formed* ones that let the variant be
/// reached: label operands point at `Label(2)` (the success sink) or `Label(3)`
/// (the `func_info` failure sink), atomish operands are `Operand::Nil` (a real
/// operand the family then rejects with a typed error, never a fabricated atom),
/// and the `call`-family targets use `Label(3)` so a call can never re-enter its own
/// continuation and spin.
fn all_instr_variants() -> Vec<Instr> {
    let x0 = || Operand::X(0);
    let x1 = || Operand::X(1);
    let fail = Label(2);
    let sink = Label(3);
    let nil = || Operand::Nil;
    let words = || HeapNeed::Words(0);

    vec![
        Instr::Label(Label(77)),
        Instr::Line(0),
        Instr::FuncInfo {
            module: Atom::MAX,
            function: Atom::MAX,
            arity: 0,
        },
        Instr::IntCodeEnd,
        Instr::Move {
            src: Operand::Integer(1),
            dst: x0(),
        },
        Instr::Swap {
            a: x0(),
            b: x1(),
        },
        Instr::Call {
            arity: 0,
            target: sink,
        },
        Instr::CallLast {
            arity: 0,
            target: fail,
            deallocate: 0,
        },
        Instr::CallOnly {
            arity: 0,
            target: fail,
        },
        Instr::CallExt {
            arity: 0,
            target: ImportRef(0),
        },
        Instr::CallExtOnly {
            arity: 0,
            target: ImportRef(0),
        },
        Instr::CallExtLast {
            arity: 0,
            target: ImportRef(0),
            deallocate: 0,
        },
        Instr::Bif0 {
            bif: ImportRef(0),
            dst: x0(),
        },
        Instr::Bif {
            arity: 0,
            bif: ImportRef(0),
            fail,
            args: Vec::new(),
            dst: x0(),
        },
        Instr::GcBif {
            arity: 0,
            bif: ImportRef(0),
            fail,
            live: 0,
            args: Vec::new(),
            dst: x0(),
        },
        Instr::CallFun { arity: 0 },
        Instr::CallFun2 {
            tag: nil(),
            arity: 0,
            func: x0(),
        },
        Instr::Apply { arity: 0 },
        Instr::ApplyLast {
            arity: 0,
            deallocate: 0,
        },
        Instr::MakeFun3 {
            index: 0,
            dst: x0(),
            env: Vec::new(),
        },
        Instr::Test {
            kind: TestKind::IsInteger,
            fail,
            args: vec![Operand::Integer(1)],
        },
        Instr::SelectVal {
            value: Operand::Integer(1),
            fail,
            pairs: vec![(Operand::Integer(1), fail)],
        },
        Instr::SelectTupleArity {
            tuple: x0(),
            fail,
            destinations: vec![(Operand::Integer(0), fail)],
        },
        Instr::Jump { target: fail },
        Instr::Allocate {
            stack_need: words(),
            live: 0,
        },
        Instr::AllocateHeap {
            stack_need: words(),
            heap_need: words(),
            live: 0,
        },
        Instr::TestHeap {
            heap_need: words(),
            live: 0,
        },
        Instr::InitYregs {
            registers: vec![Operand::Y(0)],
        },
        Instr::Deallocate(0),
        Instr::Trim { n: 0, remaining: 0 },
        Instr::Return,
        Instr::PutTuple2 {
            dst: x0(),
            elements: vec![Operand::Integer(1)],
        },
        Instr::GetTupleElement {
            source: x0(),
            element: 0,
            dst: x1(),
        },
        Instr::PutList {
            head: x0(),
            tail: nil(),
            dst: x1(),
        },
        Instr::GetList {
            source: x0(),
            head: x1(),
            tail: Operand::X(2),
        },
        Instr::GetHd {
            source: x0(),
            head: x1(),
        },
        Instr::GetTl {
            source: x0(),
            tail: x1(),
        },
        Instr::GetMapElements {
            fail,
            source: x0(),
            pairs: vec![(Operand::Integer(1), x1())],
        },
        Instr::HasMapFields {
            fail,
            source: x0(),
            keys: vec![Operand::Integer(1)],
        },
        Instr::PutMapAssoc {
            fail,
            source: x0(),
            dst: x1(),
            live: 0,
            pairs: vec![(Operand::Integer(1), Operand::Integer(2))],
        },
        Instr::PutMapExact {
            fail,
            source: x0(),
            dst: x1(),
            live: 0,
            pairs: vec![(Operand::Integer(1), Operand::Integer(2))],
        },
        Instr::UpdateRecord {
            hint: nil(),
            size: 1,
            src: x0(),
            dst: x1(),
            updates: vec![(Operand::Integer(1), Operand::Integer(2))],
        },
        Instr::IsAnyNativeRecord { fail, src: x0() },
        Instr::IsNativeRecord {
            fail,
            src: x0(),
            module: nil(),
            name: nil(),
        },
        Instr::GetRecordElements {
            fail,
            src: x0(),
            fields: vec![(nil(), x1())],
        },
        Instr::PutRecord {
            fail,
            id: nil(),
            src: nil(),
            dst: x0(),
            live: 0,
            updates: vec![(nil(), Operand::Integer(1))],
        },
        Instr::IsRecordAccessible {
            fail,
            src: x0(),
            scope: nil(),
        },
        Instr::GetRecordField {
            fail,
            src: x0(),
            id: nil(),
            field: nil(),
            dst: x1(),
        },
        Instr::GetRecordElementsId {
            fail,
            id: nil(),
            src: x0(),
            fields: vec![(nil(), x1())],
        },
        Instr::UpdateRecordId {
            fail,
            hint: nil(),
            id: nil(),
            src: x0(),
            dst: x1(),
            live: 0,
            updates: vec![(nil(), Operand::Integer(1))],
        },
        Instr::Badmatch { value: x0() },
        Instr::CaseEnd { value: x0() },
        Instr::IfEnd,
        Instr::Try {
            frame: x0(),
            fail,
        },
        Instr::Catch {
            frame: x0(),
            fail,
        },
        Instr::TryCase { frame: x0() },
        Instr::TryEnd { frame: x0() },
        Instr::TryCaseEnd { value: x0() },
        Instr::CatchEnd { frame: x0() },
        Instr::Raise {
            trace: nil(),
            value: x0(),
        },
        Instr::Badrecord { value: x0() },
        Instr::BuildStacktrace,
        Instr::RawRaise,
        Instr::Fmove {
            src: Operand::FloatReg(0),
            dst: Operand::FloatReg(1),
        },
        Instr::Fconv {
            src: Operand::Integer(1),
            dst: Operand::FloatReg(0),
        },
        Instr::Fadd {
            fail,
            lhs: Operand::FloatReg(0),
            rhs: Operand::FloatReg(1),
            dst: Operand::FloatReg(2),
        },
        Instr::Fsub {
            fail,
            lhs: Operand::FloatReg(0),
            rhs: Operand::FloatReg(1),
            dst: Operand::FloatReg(2),
        },
        Instr::Fmul {
            fail,
            lhs: Operand::FloatReg(0),
            rhs: Operand::FloatReg(1),
            dst: Operand::FloatReg(2),
        },
        Instr::Fdiv {
            fail,
            lhs: Operand::FloatReg(0),
            rhs: Operand::FloatReg(1),
            dst: Operand::FloatReg(2),
        },
        Instr::Fnegate {
            fail,
            src: Operand::FloatReg(0),
            dst: Operand::FloatReg(1),
        },
        Instr::Send,
        Instr::LoopRec { fail, dst: x0() },
        Instr::LoopRecEnd { target: fail },
        Instr::Wait { target: fail },
        Instr::WaitTimeout {
            target: fail,
            time: Operand::Integer(0),
        },
        Instr::Timeout,
        Instr::RemoveMessage,
        Instr::RecvMarkerReserve { marker: x0() },
        Instr::RecvMarkerBind {
            marker: x0(),
            reference: x1(),
        },
        Instr::RecvMarkerClear { reference: x0() },
        Instr::RecvMarkerUse { reference: x0() },
        Instr::BsStartMatch3 {
            fail,
            bin: x0(),
            live: 0,
            dst: x1(),
        },
        Instr::BsStartMatch4 {
            fail: Operand::Label(fail),
            live: 0,
            src: x0(),
            dst: x1(),
        },
        Instr::BsMatch {
            fail,
            ctx: x0(),
            commands: Vec::new(),
        },
        Instr::BsMatchString {
            fail,
            ctx: x0(),
            bits: 0,
            offset: 0,
        },
        Instr::BsGetTail {
            ctx: x0(),
            dst: x1(),
            live: 0,
        },
        Instr::BsGetPosition {
            ctx: x0(),
            dst: x1(),
            live: 0,
        },
        Instr::BsSetPosition {
            ctx: x0(),
            pos: x1(),
        },
        Instr::BsGetInteger2 {
            fail,
            ctx: x0(),
            live: 0,
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
            dst: x1(),
        },
        Instr::BsGetFloat2 {
            fail,
            ctx: x0(),
            live: 0,
            size: Operand::Integer(64),
            unit: 1,
            flags: 0,
            dst: x1(),
        },
        Instr::BsGetBinary2 {
            fail,
            ctx: x0(),
            live: 0,
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
            dst: x1(),
        },
        Instr::BsSkipBits2 {
            fail,
            ctx: x0(),
            size: Operand::Integer(8),
            unit: 1,
            flags: 0,
        },
        Instr::BsTestTail2 {
            fail,
            ctx: x0(),
            bits: 0,
        },
        Instr::BsGetUtf8 {
            fail,
            ctx: x0(),
            live: 0,
            flags: 0,
            dst: x1(),
        },
        Instr::BsGetUtf16 {
            fail,
            ctx: x0(),
            live: 0,
            flags: 0,
            dst: x1(),
        },
        Instr::BsGetUtf32 {
            fail,
            ctx: x0(),
            live: 0,
            flags: 0,
            dst: x1(),
        },
        Instr::BsSkipUtf8 {
            fail,
            ctx: x0(),
            live: 0,
            flags: 0,
        },
        Instr::BsSkipUtf16 {
            fail,
            ctx: x0(),
            live: 0,
            flags: 0,
        },
        Instr::BsSkipUtf32 {
            fail,
            ctx: x0(),
            live: 0,
            flags: 0,
        },
        Instr::BsInitWritable,
        Instr::BsCreateBin {
            fail,
            alloc: 0,
            live: 0,
            unit: 1,
            dst: x0(),
            segments: Vec::new(),
        },
        Instr::ExecutableLine {
            location: 0,
            index: 0,
        },
        Instr::DebugLine {
            kind: nil(),
            location: 0,
            index: 0,
            live: 0,
        },
        Instr::NifStart,
        Instr::OnLoad,
        Instr::Generic {
            op: Opcode::send,
            operands: Vec::new(),
            offset: 0,
        },
    ]
}
