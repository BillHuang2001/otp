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
//! The `func_info` raise's **stacktrace frames**, end to end.
//!
//! A function's callable entry label is the label *after* its `func_info`
//! instruction, so a normal call never executes `func_info`; the only way to
//! reach it is a JUMP from a failed head-pattern/guard `fail` label. C
//! (`i_func_info_IaaI`, `erts/emulator/beam/emu/beam_emu.c:578-583`) then raises
//! class `error` with the **bare atom** `function_clause` — the call arguments
//! ride in the stacktrace, never in the reason.
//!
//! That reason names neither the module nor the function, so an *uncaught*
//! `function_clause` — the boot path's failure mode, e.g.
//! `application_controller`'s child dying during startup — is unlocalizable unless
//! the raise carries the frames it travelled through. C captures them as the raw
//! `StackTrace` object hung off the exception (`c_p->ftrace`); the engine derives
//! them from its live call frames at raise time
//! ([`Machine::raise_stacktrace`](beam_engine::Machine::raise_stacktrace)) and
//! hands them to the uncaught `Raise`.
//!
//! The oracle cross-check at the bottom pins the semantic contract the frames
//! implement: real ERTS names the **raising** function in the innermost frame of a
//! `function_clause` stacktrace. It skips loudly when no `erl` is on `PATH`.
//!
//! The synthetic modules below are hand-built (the loader is not involved) — the
//! smallest bytecode shapes that reproduce a failed head clause and a non-tail
//! call into it.
use std::collections::BTreeMap;
use std::process::Command;

use beam_common::ExceptionClass;
use beam_engine::{Machine, MinimalContext};
use beam_loader::{Export, Function, HeapNeed, Instr, Label, Module, Operand, TestKind};
use beam_term::{make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// Module-building scaffolding
// ---------------------------------------------------------------------------

/// Map every `Instr::Label` in `code` to its index.
fn labels_of(code: &[Instr]) -> BTreeMap<Label, usize> {
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    labels
}

/// A module builder with its own atom table (the loader is not involved).
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    /// Append a function. `entry` is its **callable entry label** — the label
    /// *after* `func_info`, exactly what the loader records (the `func_info` label
    /// itself is reachable only from a failed clause), and what a caller jumps to.
    fn add_function(&mut self, name: &str, arity: u32, entry: Label, code: Vec<Instr>) {
        let name = self.atom(name);
        let module = self.module.name;
        let labels = labels_of(&code);
        let func_label = labels.keys().next().copied().unwrap_or(Label(0));
        self.module.functions.push(Function {
            func_label,
            entry_label: entry,
            module,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
    }

    fn add_export(&mut self, name: &str, arity: u32, label: Label) {
        let function = self.atom(name);
        self.module.exports.push(Export {
            function,
            arity,
            label,
        });
    }

    fn finish(self) -> Module {
        self.module
    }
}

/// `m:f/1` — one guard clause whose `fail` label is the function's `func_info`
/// label, i.e. the compiled shape of a failed head clause:
///
/// ```text
/// label 1
/// func_info m f 1
/// label 2                 ; the callable entry
///     test is_atom x(0) fail 1
///     move 1 -> x(0)
///     return
/// ```
///
/// The `function_clause`/`error` atoms are interned because a real module that can
/// raise `function_clause` carries them (and the landing installs the `error` class
/// atom from the module's own table).
fn func_info_module() -> Module {
    let mut fixture = Fixture::new("m");
    fixture.atom("function_clause");
    fixture.atom("error");
    fixture.atom("a");
    let module_atom = fixture.atom("m");
    let function_atom = fixture.atom("f");
    fixture.add_function(
        "f",
        1,
        Label(2),
        vec![
            Instr::Label(Label(1)),
            Instr::FuncInfo {
                module: module_atom,
                function: function_atom,
                arity: 1,
            },
            Instr::Label(Label(2)),
            Instr::Test {
                kind: TestKind::IsAtom,
                fail: Label(1),
                args: vec![Operand::X(0)],
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("f", 1, Label(2));
    fixture.finish()
}

/// The same `f/1`, called **non-tail** by `g/0`, so the raise has a live caller
/// frame:
///
/// ```text
/// f/1: (as above)
/// g/0: label 10
///          allocate 0
///          move 7 -> x(0)
///          call f/1            ; stores the continuation in g's frame
///          deallocate 0
///          return
/// ```
fn nested_module() -> Module {
    let mut fixture = Fixture::new("m");
    fixture.atom("function_clause");
    fixture.atom("error");
    let module_atom = fixture.atom("m");
    let function_atom = fixture.atom("f");
    fixture.add_function(
        "f",
        1,
        Label(2),
        vec![
            Instr::Label(Label(1)),
            Instr::FuncInfo {
                module: module_atom,
                function: function_atom,
                arity: 1,
            },
            Instr::Label(Label(2)),
            Instr::Test {
                kind: TestKind::IsAtom,
                fail: Label(1),
                args: vec![Operand::X(0)],
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    fixture.add_export("f", 1, Label(2));
    fixture.add_function(
        "g",
        0,
        Label(10),
        vec![
            Instr::Label(Label(10)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(0),
                live: 0,
            },
            Instr::Move {
                src: Operand::Integer(7),
                dst: Operand::X(0),
            },
            Instr::Call {
                arity: 1,
                target: Label(2),
            },
            Instr::Deallocate(0),
            Instr::Return,
        ],
    );
    fixture.add_export("g", 0, Label(10));
    fixture.finish()
}

/// Run `module:f/arity` with `args` and require an uncaught raise.
fn run_uncaught(module: &Module, function: &str, args: &[Eterm]) -> beam_common::Raise {
    let function = module
        .atom_table
        .lookup(function)
        .expect("the function atom is interned");
    let arity = args.len() as u32;
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let err = machine
        .run_export_outcome(module, function, arity, args, &mut ctx)
        .expect_err("the head clause matches no argument and must raise");
    err.into_raise().expect("a raise, not a capability gap")
}

/// The raise's **live reason term** — what a live catch frame lands in `x(1)`.
///
/// The engine's uncaught raise is `ExitReason::Structured` with the reason term in
/// the `live_reason` slot, so [`Raise::reason::as_atom`] is `None` here; the atom
/// name is recovered by resolving the term against the module that raised it (the
/// runtime does the same through its shared atom table).
fn live_reason_term(raise: &beam_common::Raise) -> Option<Eterm> {
    raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
}

/// Resolve the raise's reason term to its atom name in `module`'s own atom table.
fn reason_atom_name(raise: &beam_common::Raise, module: &Module) -> Option<String> {
    let reason = live_reason_term(raise)?;
    if !beam_term::is_atom(reason) {
        return None;
    }
    module
        .atom_table
        .name(beam_term::atom_val(reason))
        .map(str::to_owned)
}

// ---------------------------------------------------------------------------
// The frames of an uncaught `func_info` raise
// ---------------------------------------------------------------------------

#[test]
fn an_uncaught_func_info_raise_names_the_raising_function() {
    // `f(7)`: the guard failure jumps to `func_info`, which raises. The reason is
    // the bare atom `function_clause` — it names no function — so the *frames* are
    // the only thing that localizes the failure.
    let module = func_info_module();
    let raise = run_uncaught(&module, "f", &[make_small(7)]);

    assert_eq!(raise.class, ExceptionClass::Error);
    assert_eq!(
        reason_atom_name(&raise, &module).as_deref(),
        Some("function_clause"),
        "the reason is the bare atom `function_clause`"
    );
    assert!(
        !raise.stacktrace.is_empty(),
        "an uncaught `function_clause` must carry the frames it was raised in"
    );
    let head = &raise.stacktrace.frames[0];
    assert_eq!(head.module.as_str(), "m");
    assert_eq!(head.function.as_str(), "f");
    assert_eq!(head.arity, 1);
}

#[test]
fn the_raise_names_its_live_callers_innermost_first() {
    // `g/0` calls `f/1` non-tail, so the raise has a live caller frame: the frames
    // run the raising function first, then each caller — C's
    // `build_stacktrace` order.
    let module = nested_module();
    let raise = run_uncaught(&module, "g", &[]);

    assert_eq!(
        reason_atom_name(&raise, &module).as_deref(),
        Some("function_clause")
    );
    let frames = &raise.stacktrace.frames;
    assert_eq!(frames.len(), 2, "the raising function then its caller");
    assert_eq!(frames[0].function.as_str(), "f");
    assert_eq!(frames[0].arity, 1);
    assert_eq!(frames[1].module.as_str(), "m");
    assert_eq!(frames[1].function.as_str(), "g");
    assert_eq!(frames[1].arity, 0);
}

#[test]
fn a_matching_head_clause_never_raises_and_the_reason_is_unchanged() {
    // The `func_info` label is reachable only from a failed clause, so a matching
    // call returns normally (the raise is not a new failure mode of the arm), and
    // the raise itself still carries the bare atom reason.
    let module = func_info_module();
    let function = module.atom_table.lookup("f").expect("interned");
    let atom = module.atom_table.lookup("a").expect("interned");
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let outcome = machine
        .run_export_outcome(&module, function, 1, &[beam_term::make_atom(atom)], &mut ctx)
        .expect("an atom argument matches the guard");
    assert_eq!(
        outcome,
        beam_engine::RunOutcome::Returned(make_small(1)),
        "the matching clause returns 1"
    );

    let raise = run_uncaught(&module, "f", &[make_small(7)]);
    assert_eq!(
        reason_atom_name(&raise, &module).as_deref(),
        Some("function_clause")
    );
    assert!(
        raise.live_reason().is_some(),
        "the reason term still rides out of the machine"
    );
}

// ---------------------------------------------------------------------------
// The oracle cross-check (real ERTS), skipped loudly without `erl`
// ---------------------------------------------------------------------------

/// Run `expr` under the system `erl` and return its stdout, or `None` when no
/// `erl` is available (or the expression failed).
fn oracle(expr: &str) -> Option<String> {
    let output = Command::new("erl")
        .arg("-noshell")
        .arg("-eval")
        .arg(expr)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[test]
fn the_oracle_names_the_raising_function_in_its_innermost_frame() {
    // `proplists:get_value/2` has no clause for a non-list argument, so it raises
    // `function_clause` from inside a real system module. The oracle's raw
    // stacktrace (`catch C:R:S`) must name **that** function in its head item —
    // exactly the contract the engine's frames implement above.
    let expr = "S = (try proplists:get_value(a, undefined) \
                catch error:_:St -> St end), \
                [{M,F,Args,_}|_] = S, io:format(\"~p\", [{M,F,length(Args)}]), halt().";
    match oracle(expr) {
        Some(actual) => assert_eq!(
            actual, "{proplists,get_value,3}",
            "the oracle's innermost stacktrace frame names the raising function"
        ),
        None => eprintln!(
            "SKIP func_info_stacktrace: no system `erl` on PATH — the oracle \
             cross-check was skipped"
        ),
    }
}
