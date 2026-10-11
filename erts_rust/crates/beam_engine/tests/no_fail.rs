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

//! Integration coverage for the **defined `NO_FAIL` guard failure**.
//!
//! The four guard-bearing opcodes that carry a `fail` operand (`test`,
//! `select_val`, `get_map_elements`, `has_map_fields`) treat a `fail == NO_FAIL`
//! failure — the compiler's "this cannot fail" marker — as a *defined* raise:
//! class `error` with the **bare atom** `function_clause`, resolved from the
//! running module's own atom table (never a substituted foreign atom). This is
//! the positive counterpart to `tests/gaps.rs`, whose pins are the typed
//! "no such atom in the module" gaps *because* it deliberately does not seed the
//! reason atoms.
//!
//! Every case is driven through the **real public entry** `Machine::run_label`
//! over a synthetic [`Module`] + [`MinimalContext`], exactly as `tests/gaps.rs`
//! does, and each is asserted **both ways**:
//!   * the **uncaught** raise surfaces as the run's `Err(BeamError::Raise(..))`
//!     with `class == ExceptionClass::Error` and the live reason term the bare
//!     atom `function_clause`; and
//!   * the raise **lands in a live `try`/`catch` frame**, whose handler observes
//!     the class `error` (in `x(0)` after `try_case`) and the reason
//!     `function_clause` (in `x(1)`).
//!
//! The raise helpers are the exact bodies of the in-crate unit tests
//! (`src/interpreter/tests.rs::{uncaught_class_and_reason, wrapped_in_catch}` and
//! `src/families/maps/tests.rs::{with_catch, uncaught_raise, live_reason_term}`),
//! re-expressed against the public API.

use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{Atom, Function, Instr, Label, Module, Operand, TestKind, NO_FAIL};
use beam_term::{make_atom, make_small, Eterm};

// ---------------------------------------------------------------------------
// The synthetic-module builder (mirrors `tests/gaps.rs::GapFixture`)
// ---------------------------------------------------------------------------

/// The label→index map the loader would build, over a hand-written `code` vec.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A builder for the synthetic modules this suite drives.
///
/// Unlike `tests/gaps.rs::GapFixture`, [`NoFailFixture::new`] interns the atoms
/// the raise builders resolve **by name** — `function_clause` (the reason) and
/// `error` (the landing class) — plus the map keys `a`/`b`, in a fixed order, so
/// every fixture assigns the same index to the same name. That determinism is
/// what lets a map built from one fixture's atoms be read by another fixture's
/// module (the map cases need a fixture per run).
struct NoFailFixture {
    module: Module,
}

impl NoFailFixture {
    fn new() -> NoFailFixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("no_fail");
        for name in ["function_clause", "error", "a", "b"] {
            module.atom_table.intern(name);
        }
        NoFailFixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function; returns its entry label (the first label in `code`).
    fn add_function(&mut self, name: &str, arity: u32, code: Vec<Instr>) -> Label {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let entry = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label: entry,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        entry
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// Build a `f/1` whose code is `Label(1)` followed by `body`; returns the entry
/// label and the module.
fn function_with_body(body: Vec<Instr>) -> (Label, Module) {
    let mut f = NoFailFixture::new();
    let mut code = vec![Instr::Label(Label(1))];
    code.extend(body);
    let entry = f.add_function("f", 1, code);
    (entry, f.finish())
}

/// The `try … catch` skeleton of `src/families/maps/tests.rs::with_catch`:
/// `Label(1)`, `try Frame Fail=Label(3)`, `body`, `try_end Frame`,
/// `move x(0),x(0)`, `return`, `Label(3)`, `try_case Frame`, `handler`, `return`.
///
/// After `try_case` the **class atom is in `x(0)`** (copied from `x(3)`) while
/// the **reason stays in `x(1)`** (C's `handle_error` register convention), so
/// `handler` selects which of the two the run returns.
fn wrapped_in_catch(body: Vec<Instr>, handler: Instr) -> Vec<Instr> {
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Try {
            frame: Operand::X(9),
            fail: Label(3),
        },
    ];
    code.extend(body);
    code.extend([
        Instr::TryEnd {
            frame: Operand::X(9),
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::TryCase {
            frame: Operand::X(9),
        },
        handler,
        Instr::Return,
    ]);
    code
}

// ---------------------------------------------------------------------------
// The two shared assertions — the uncaught raise, and the caught raise
// ---------------------------------------------------------------------------

/// The `(class, live reason term)` of an uncaught raise that surfaced as a run's
/// `Err`. Panics unless the error is a raise carrying a live reason term, so a
/// test can never pass by observing a different outcome than the one it pins.
fn uncaught_class_and_reason(err: BeamError) -> (ExceptionClass, Eterm) {
    match err {
        BeamError::Raise(raise) => {
            let live = raise
                .live_reason()
                .expect("an uncaught reason carries its term");
            (
                raise.class,
                *live.downcast_ref::<Eterm>().expect("the reason term"),
            )
        }
        other => panic!("expected an uncaught raise, got {other:?}"),
    }
}

/// Drive `entry` with no live catch frame and assert the run's `Err` is a
/// class-`error` raise whose live reason term is the bare `function_clause`, and
/// that the machine records the same pending class + reason.
fn assert_uncaught_function_clause(module: &Module, entry: Label, args: &[Eterm], reason_atom: Atom) {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let err = machine
        .run_label(module, entry, args.len() as u32, args, &mut ctx)
        .expect_err("the NO_FAIL guard failure raises with no live catch frame");
    let (class, reason) = uncaught_class_and_reason(err);
    assert_eq!(class, ExceptionClass::Error);
    assert_eq!(reason, make_atom(reason_atom));
    // The machine records exactly the same pending class + reason term.
    assert_eq!(machine.pending_class(), Some(ExceptionClass::Error));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, make_atom(reason_atom)))
    );
}

/// Wrap `body` in a live `try` frame, run it, and assert the handler observes
/// class `error` (in `x(0)` after `try_case`) and the bare reason (in `x(1)`).
/// Two runs of the same body, differing only in which register the handler
/// returns, so BOTH the class and the reason are pinned. `args` may be built from
/// any [`NoFailFixture`]'s atoms (their indices are deterministic; see the struct
/// docs).
fn assert_raise_lands_in_a_live_catch_frame(body: Vec<Instr>, args: &[Eterm]) {
    // Run 1: the handler returns the landed **reason** (`x(1)`).
    let mut f = NoFailFixture::new();
    let function_clause = f.atom("function_clause");
    let code = wrapped_in_catch(
        body.clone(),
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
    );
    let entry = f.add_function("f", 1, code);
    let module = f.finish();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let reason = machine
        .run_label(&module, entry, args.len() as u32, args, &mut ctx)
        .expect("the live catch frame handles the raise");
    assert_eq!(
        reason,
        make_atom(function_clause),
        "the landed reason is the bare atom function_clause"
    );
    assert_eq!(machine.catch_frame_depth(), 0, "try_case consumed the frame");

    // Run 2: the handler returns the landed **class** (`x(0)`, the `try_case`
    // copy of `x(3)`).
    let mut f = NoFailFixture::new();
    let error = f.atom("error");
    let code = wrapped_in_catch(
        body,
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::X(0),
        },
    );
    let entry = f.add_function("f", 1, code);
    let module = f.finish();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let class = machine
        .run_label(&module, entry, args.len() as u32, args, &mut ctx)
        .expect("the live catch frame handles the raise");
    assert_eq!(
        class,
        make_atom(error),
        "the landed class is the atom error"
    );
    assert_eq!(machine.catch_frame_depth(), 0, "try_case consumed the frame");
}

// ---------------------------------------------------------------------------
// 1. `test` — `is_tuple` on a small int
// ---------------------------------------------------------------------------

/// The failing body: a tuple test whose operand is a small integer, with
/// `fail == NO_FAIL`.
fn test_body() -> Vec<Instr> {
    vec![Instr::Test {
        kind: TestKind::IsTuple,
        fail: NO_FAIL,
        args: vec![Operand::X(0)],
    }]
}

#[test]
fn test_no_fail_raises_function_clause_uncaught() {
    let (entry, module) = function_with_body(test_body());
    let function_clause = module.atom_table.lookup("function_clause").expect("seeded");
    assert_uncaught_function_clause(&module, entry, &[make_small(1)], function_clause);
}

#[test]
fn test_no_fail_raise_lands_in_a_live_catch_frame() {
    assert_raise_lands_in_a_live_catch_frame(test_body(), &[make_small(1)]);
}

// ---------------------------------------------------------------------------
// 2. `select_val` — a scrutinee matching no alternative
// ---------------------------------------------------------------------------

/// The failing body: a `select_val` with a single, non-matching alternative and
/// `fail == NO_FAIL`, plus a `Label(2)`/`Return` tail so the instruction's
/// (never-taken) target label exists.
fn select_val_body() -> Vec<Instr> {
    vec![
        Instr::SelectVal {
            value: Operand::X(0),
            fail: NO_FAIL,
            pairs: vec![(Operand::Integer(99), Label(2))],
        },
        Instr::Label(Label(2)),
        Instr::Return,
    ]
}

#[test]
fn select_val_no_fail_raises_function_clause_uncaught() {
    let (entry, module) = function_with_body(select_val_body());
    let function_clause = module.atom_table.lookup("function_clause").expect("seeded");
    // `0` matches no alternative (only `99` is offered).
    assert_uncaught_function_clause(&module, entry, &[make_small(0)], function_clause);
}

#[test]
fn select_val_no_fail_raise_lands_in_a_live_catch_frame() {
    assert_raise_lands_in_a_live_catch_frame(select_val_body(), &[make_small(0)]);
}

// ---------------------------------------------------------------------------
// 3. `get_map_elements` — an absent key
// ---------------------------------------------------------------------------

/// A flat map holding only key `b`, which makes a read of key `a` the absent-key
/// path. The map lives in `builder`'s arena, so the caller must keep `builder`
/// alive for the duration of the run (mirrors `tests/gaps.rs`).
fn b_only_map(builder: &mut MinimalContext, fixture: &mut NoFailFixture) -> Eterm {
    let b = fixture.atom("b");
    terms::make_flatmap(builder, &[(make_atom(b), make_small(1))]).expect("a flat map")
}

/// The failing body: a `get_map_elements` reading key `a`, with `fail == NO_FAIL`.
fn get_map_elements_body(a: Atom) -> Vec<Instr> {
    vec![Instr::GetMapElements {
        fail: NO_FAIL,
        source: Operand::X(0),
        pairs: vec![(Operand::Atom(a), Operand::X(1))],
    }]
}

#[test]
fn get_map_elements_no_fail_raises_function_clause_uncaught() {
    let mut f = NoFailFixture::new();
    let a = f.atom("a");
    let mut builder = MinimalContext::new();
    let map = b_only_map(&mut builder, &mut f);
    let function_clause = f.atom("function_clause");
    let code = {
        let mut code = vec![Instr::Label(Label(1))];
        code.extend(get_map_elements_body(a));
        code
    };
    let entry = f.add_function("f", 1, code);
    let module = f.finish();
    // `builder` (and therefore `map`) stays alive across the run.
    assert_uncaught_function_clause(&module, entry, &[map], function_clause);
}

#[test]
fn get_map_elements_no_fail_raise_lands_in_a_live_catch_frame() {
    let mut f = NoFailFixture::new();
    let a = f.atom("a");
    let mut builder = MinimalContext::new();
    let map = b_only_map(&mut builder, &mut f);
    assert_raise_lands_in_a_live_catch_frame(get_map_elements_body(a), &[map]);
}

// ---------------------------------------------------------------------------
// 4. `has_map_fields` — an absent key
// ---------------------------------------------------------------------------

/// The failing body: a `has_map_fields` requiring key `a`, with `fail == NO_FAIL`.
fn has_map_fields_body(a: Atom) -> Vec<Instr> {
    vec![Instr::HasMapFields {
        fail: NO_FAIL,
        source: Operand::X(0),
        keys: vec![Operand::Atom(a)],
    }]
}

#[test]
fn has_map_fields_no_fail_raises_function_clause_uncaught() {
    let mut f = NoFailFixture::new();
    let a = f.atom("a");
    let mut builder = MinimalContext::new();
    let map = b_only_map(&mut builder, &mut f);
    let function_clause = f.atom("function_clause");
    let code = {
        let mut code = vec![Instr::Label(Label(1))];
        code.extend(has_map_fields_body(a));
        code
    };
    let entry = f.add_function("f", 1, code);
    let module = f.finish();
    assert_uncaught_function_clause(&module, entry, &[map], function_clause);
}

#[test]
fn has_map_fields_no_fail_raise_lands_in_a_live_catch_frame() {
    let mut f = NoFailFixture::new();
    let a = f.atom("a");
    let mut builder = MinimalContext::new();
    let map = b_only_map(&mut builder, &mut f);
    assert_raise_lands_in_a_live_catch_frame(has_map_fields_body(a), &[map]);
}
