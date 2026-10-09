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

//! Regression: an internal `catch` must contain a raise whose reason is a **bare
//! atom** — including one that originates in a cross-module callee.
//!
//! # The defect this pins
//!
//! `Machine::raise_bif_error` (the one landing path for a `gc_bif`/`bif`/`call_ext*`
//! failure on the no-fail-label route) used to short-circuit on
//! `matches!(&error, BeamError::Raise(_))`: **every** `BeamError::Raise` was
//! returned uncaught, whether or not a live `try`/`catch` frame existed. A raise
//! with a *compound* reason was fine (it carries a `live_reason` term and lands
//! through `throw_with_trace`), but a raise whose reason is a **bare atom** —
//! exactly what `erlang:throw/1`, `erlang:exit/1`, `erlang:error(Atom)`, and every
//! `beam_bifs`-backed no-clause failure (`lists:nth/2`, …) produce — has no live
//! term slot, so it escaped the `catch`:
//!
//! ```text
//! f() -> try lists:nth(5, [1]) catch C:R -> {C, R} end.   %% escaped uncaught
//! g() -> try erlang:error(boom) catch C:R -> {C, R} end.  %% escaped uncaught
//! ```
//!
//! The oracle catches all of them (`{error,function_clause}` / `{error,boom}`).
//!
//! # What the tests cover
//!
//! * an atom-reason raise is caught — both in the caller's own context (a BIF
//!   failure) and inside a cross-module caller;
//! * a **cross-module** callee's atom-reason raise lands in the *caller's* catch
//!   frame — the handler lives in a different **module context** than the raising
//!   code, so `Machine::throw_with_trace` reports the crossing as a
//!   `Step::Enter { context: caller, .. }` and `Machine::drive` must switch back;
//! * a term-reason raise (with a live reason slot) still lands — the behaviour that
//!   already worked;
//! * the **class** of a landed raise is preserved (`throw`/`exit`/`error`);
//! * a raise from the current module's own native opcode (`case_end`) stays caught;
//! * an **uncaught** raise (no live catch frame) still propagates the caller's own
//!   typed `BeamError`, with its reason intact.

use std::collections::BTreeMap;
use std::sync::Arc;

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise, Result};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext, RunOutcome};
use beam_loader::{
    Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand,
};
use beam_term::{cmp, make_atom, make_small, Atom, Eterm};

// ---------------------------------------------------------------------------
// Minimal synthetic-module scaffolding
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

struct Fixture {
    module: Module,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern(name);
        // Every module names the atoms a landing may resolve: the three class
        // names, the `catch_end` tag, the `func_info` reason, and the atom-reason
        // fixtures.
        for name in [
            "error",
            "exit",
            "throw",
            "EXIT",
            "function_clause",
            "boom",
            "case_clause",
            "sync_boom",
        ] {
            module.atom_table.intern(name);
        }
        Fixture { module }
    }

    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    fn import(&mut self, module_name: &str, function_name: &str, arity: u32) -> ImportRef {
        let module = self.atom(module_name);
        let function = self.atom(function_name);
        self.module.imports.push(Import {
            module,
            function,
            arity,
        });
        ImportRef((self.module.imports.len() - 1) as u32)
    }

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

// ---------------------------------------------------------------------------
// The caller `m`: `run/0` wraps a cross-module `call_ext` in a `try`/`catch`
// ---------------------------------------------------------------------------

/// What the handler returns: the `x(1)` **reason** (the `{C,R}` reason component)
/// or the `x(0)` **class** atom (what `try_case` unpacks from `x(3)`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Handler {
    Reason,
    Class,
}

/// ```text
/// run() -> try <callee>(Arg) catch C:R -> {C, R} end.
/// ```
///
/// Bytecode (hand-built; the shapes mirror what `beam_ssa_codegen` emits — the try
/// frame slot is a Y register and `try_end` pops it on the normal path):
///
/// ```text
/// run/0:
///     allocate 1
///     try y0 {handler}
///     [move <arg> x0]           ; only when the callee takes an argument
///     call_ext <arity> <callee>
///     try_end y0
///     deallocate 1
///     return
///   {handler}:
///     try_case y0               ; x0 = class atom, x1 = reason, x2 = trace
///     move <Reason|Class> -> x0
///     deallocate 1
///     return
/// ```
fn caller_module(
    callee_module: &str,
    callee_function: &str,
    callee_arity: u32,
    handler: Handler,
) -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let target = f.import(callee_module, callee_function, callee_arity);
    let boom = f.atom("boom");
    let ret = match handler {
        Handler::Reason => Operand::X(1),
        Handler::Class => Operand::X(0),
    };
    let mut code = vec![
        Instr::Label(Label(100)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(103),
        },
    ];
    if callee_arity == 1 {
        code.push(Instr::Move {
            src: Operand::Atom(boom),
            dst: Operand::X(0),
        });
    }
    code.extend([
        Instr::CallExt {
            target,
            arity: callee_arity,
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(103)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        Instr::Move { src: ret, dst: Operand::X(0) },
        Instr::Deallocate(1),
        Instr::Return,
    ]);
    let run = f.add_function("run", 0, code);
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

/// The **same-module** variant of [`caller_module`]: `case_end` raises
/// `{case_clause, 1}` from a native opcode in the caller's own module (no
/// cross-module call, no BIF) — the same-context raise that always worked.
fn native_raise_module() -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let run = f.add_function(
        "run",
        0,
        vec![
            Instr::Label(Label(100)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(103),
            },
            Instr::CaseEnd {
                value: Operand::Integer(1),
            },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(103)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

// ---------------------------------------------------------------------------
// The callee `c`: `boom/0` raises via `erlang:error/1`; `thrown/0` via
// `erlang:throw/1`; `fc/0` raises a `function_clause`.
// ---------------------------------------------------------------------------

fn callee_module() -> Arc<Module> {
    let mut f = Fixture::new("c");

    let ok = f.add_function(
        "ok",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Integer(5),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    f.add_export("ok", 0, ok);

    // `boom/0`: `erlang:error(boom)` — the raise is a BIF failure inside the callee.
    let error = f.import("erlang", "error", 1);
    let boom_atom = f.atom("boom");
    let boom = f.add_function(
        "boom",
        0,
        vec![
            Instr::Label(Label(10)),
            Instr::Move {
                src: Operand::Atom(boom_atom),
                dst: Operand::X(0),
            },
            Instr::CallExt { target: error, arity: 1 },
            Instr::Return,
        ],
    );
    f.add_export("boom", 0, boom);

    // `thrown/0`: `throw(boom)`.
    let thrown_import = f.import("erlang", "throw", 1);
    let thrown_atom = f.atom("boom");
    let thrown = f.add_function(
        "thrown",
        0,
        vec![
            Instr::Label(Label(15)),
            Instr::Move {
                src: Operand::Atom(thrown_atom),
                dst: Operand::X(0),
            },
            Instr::CallExt {
                target: thrown_import,
                arity: 1,
            },
            Instr::Return,
        ],
    );
    f.add_export("thrown", 0, thrown);

    // `fc/0`: a failed head clause — the `func_info` raise (`function_clause`).
    let module_atom = f.module.name;
    let fc_atom = f.atom("fc");
    let fc = f.add_function(
        "fc",
        0,
        vec![
            Instr::Label(Label(30)),
            Instr::FuncInfo {
                module: module_atom,
                function: fc_atom,
                arity: 0,
            },
            // Unreachable: `func_info` always raises.
            Instr::Return,
        ],
    );
    f.add_export("fc", 0, fc);

    Arc::new(f.finish())
}

// ---------------------------------------------------------------------------
// The host: the cross-module seam + a BIF floor that raises
// ---------------------------------------------------------------------------

type BifFloor = Box<dyn FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm>>;

struct CatchCtx {
    callees: Vec<(String, Arc<Module>)>,
    seam: bool,
    bif: BifFloor,
}

impl CatchCtx {
    fn new(callees: Vec<(String, Arc<Module>)>, bif: BifFloor) -> CatchCtx {
        CatchCtx {
            callees,
            seam: true,
            bif,
        }
    }

    fn without_seam(mut self) -> CatchCtx {
        self.seam = false;
        self
    }
}

impl ExecutionContext for CatchCtx {
    fn call_bif(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
        args: &[Eterm],
    ) -> Result<Eterm> {
        (self.bif)(module, function, arity, args)
    }

    fn ensure_heap(
        &mut self,
        _n: u32,
        _live: u32,
        _x: &mut [Eterm],
        _s: &mut [Eterm],
    ) -> Result<()> {
        Ok(())
    }

    fn resolve_external_call(
        &mut self,
        module: &str,
        function: &str,
        arity: u32,
    ) -> Result<Option<(Arc<Module>, Label)>> {
        if !self.seam {
            return Ok(None);
        }
        for (name, callee) in &self.callees {
            if name != module {
                continue;
            }
            let Some(function) = callee.atom_table.lookup(function) else {
                continue;
            };
            if let Some(label) = callee.export(function, arity) {
                return Ok(Some((Arc::clone(callee), label)));
            }
        }
        Ok(None)
    }
}

/// A BIF floor modelling the raising standard BIFs. `erlang:error/1` /
/// `throw/1` / `exit/1` raise a [`BeamError::Raise`] whose reason is the **bare
/// atom** `boom` (no live term slot) — exactly what `beam_bifs` produces for an
/// atom reason, and the shape the defect dropped. `c:boom/0` models an external
/// module reached through the *synchronous* `call_bif` path (used when the seam is
/// off) with the same atom-reason raise.
fn raising_bif() -> BifFloor {
    Box::new(|module, function, _arity, _args| {
        let (class, reason) = match (module, function) {
            ("erlang", "error") => (ExceptionClass::Error, "boom"),
            ("erlang", "throw") => (ExceptionClass::Throw, "boom"),
            ("erlang", "exit") => (ExceptionClass::Exit, "boom"),
            ("c", "boom") => (ExceptionClass::Error, "sync_boom"),
            _ => {
                return Err(beam_engine::unsupported(&format!(
                    "bif {module}:{function}"
                )))
            }
        };
        Err(BeamError::Raise(Raise::new(class, ExitReason::atom(reason))))
    })
}

fn callees() -> Vec<(String, Arc<Module>)> {
    vec![("c".to_string(), callee_module())]
}

/// The caller-module atom term for `name` — the caller's atom table is the one
/// `raise_bif_error` resolves a landing reason against.
fn caller_atom(module: &Module, name: &str) -> Eterm {
    make_atom(module.atom_table.lookup(name).expect("caller names the atom"))
}

// ---------------------------------------------------------------------------
// (1) an atom-reason raise lands in the caller's catch frame
// ---------------------------------------------------------------------------

#[test]
fn same_context_atom_raise_from_a_bif_is_caught() {
    // `try erlang:error(boom) catch C:R -> R end` — the raise happens in the entry
    // module's OWN context (the `erlang:error/1` BIF), reason the bare atom `boom`.
    let (caller, run) = caller_module("erlang", "error", 1, Handler::Reason);
    let mut ctx = CatchCtx::new(callees(), raising_bif());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the atom-reason raise must be caught");

    assert_eq!(outcome, RunOutcome::Returned(caller_atom(&caller, "boom")));
}

#[test]
fn same_context_exit_raise_preserves_the_class_atom() {
    // The handler returns `x(0)` (what `try_case` unpacks from `x(3)`): class `exit`
    // must survive the landing, not be coerced to `error`.
    let (caller, run) = caller_module("erlang", "exit", 1, Handler::Class);
    let mut ctx = CatchCtx::new(callees(), raising_bif());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the class-`exit` raise must be caught");

    assert_eq!(outcome, RunOutcome::Returned(caller_atom(&caller, "exit")));
}

#[test]
fn same_context_throw_raise_preserves_the_class_atom() {
    let (caller, run) = caller_module("erlang", "throw", 1, Handler::Class);
    let mut ctx = CatchCtx::new(callees(), raising_bif());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the class-`throw` raise must be caught");

    assert_eq!(outcome, RunOutcome::Returned(caller_atom(&caller, "throw")));
}

// ---------------------------------------------------------------------------
// (2) a CROSS-MODULE callee's atom-reason raise lands in the caller's catch frame
// ---------------------------------------------------------------------------

#[test]
fn cross_module_atom_raise_lands_in_the_callers_catch_seam() {
    // `m:run/0` `call_ext`s `c:boom/0`, which raises via `erlang:error/1`. The
    // raise happens while the machine runs in the CALLEE's module context, so the
    // landing handler (in `m`) is in a different module context and `Machine::drive`
    // must switch back to it.
    let (caller, run) = caller_module("c", "boom", 0, Handler::Reason);
    let mut ctx = CatchCtx::new(callees(), raising_bif());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the cross-module atom-reason raise must be caught");

    assert_eq!(outcome, RunOutcome::Returned(caller_atom(&caller, "boom")));
}

#[test]
fn cross_module_throw_lands_in_the_callers_catch_seam() {
    let (caller, run) = caller_module("c", "thrown", 0, Handler::Class);
    let mut ctx = CatchCtx::new(callees(), raising_bif());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the cross-module throw must be caught");

    assert_eq!(outcome, RunOutcome::Returned(caller_atom(&caller, "throw")));
}

#[test]
fn cross_module_function_clause_lands_in_the_callers_catch_seam() {
    // `c:fc/0`'s failed head clause raises `function_clause` from the callee's own
    // `func_info` (a `throw_with_stacktrace` raise, not a BIF failure) — the
    // cross-module path that already worked must keep working.
    let (caller, run) = caller_module("c", "fc", 0, Handler::Reason);
    let mut ctx = CatchCtx::new(callees(), raising_bif());
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the cross-module func_info raise must be caught");

    assert_eq!(
        outcome,
        RunOutcome::Returned(caller_atom(&caller, "function_clause"))
    );
}

// ---------------------------------------------------------------------------
// (3) the cross-module raise caught through the SYNCHRONOUS call_bif path
// ---------------------------------------------------------------------------

#[test]
fn synchronous_cross_module_atom_raise_lands_in_the_callers_catch() {
    // Seam OFF: the `call_ext` is a synchronous `call_bif` that raises with an atom
    // reason; the raise surfaces at the call site in the caller's own context and
    // must still land in its catch frame.
    let (caller, run) = caller_module("c", "boom", 0, Handler::Reason);
    let mut ctx = CatchCtx::new(callees(), raising_bif()).without_seam();
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the synchronous atom-reason raise must be caught");

    assert_eq!(
        outcome,
        RunOutcome::Returned(caller_atom(&caller, "sync_boom"))
    );
}

// ---------------------------------------------------------------------------
// (4) a TERM-reason raise still lands (the already-working shape)
// ---------------------------------------------------------------------------

#[test]
fn term_reason_raise_still_lands() {
    // A `Raise` carrying a live reason term (the `{badmap,M}`/`{badkey,K}` shape)
    // lands in the handler with the term intact.
    let (caller, run) = caller_module("c", "boom", 0, Handler::Reason);
    // The seam enters `c:boom/0`, whose body is `erlang:error(boom)` — a BIF whose
    // floor raises with a live reason term.
    let mut ctx = CatchCtx::new(
        callees(),
        Box::new(|module, function, _arity, _args| {
            if module == "erlang" && function == "error" {
                return Err(BeamError::Raise(
                    Raise::new(ExceptionClass::Error, ExitReason::Structured)
                        .with_live_reason(LiveReason::new(make_small(7))),
                ));
            }
            Err(beam_engine::unsupported(&format!(
                "bif {module}:{function}"
            )))
        }),
    );
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("a term-reason raise must be caught");

    assert_eq!(outcome, RunOutcome::Returned(make_small(7)));
}

// ---------------------------------------------------------------------------
// (5) a same-module NATIVE raise is still caught
// ---------------------------------------------------------------------------

#[test]
fn same_module_native_raise_is_caught() {
    // `try case 1 of 2 -> two end catch C:R -> R end` — a `case_end` in the entry
    // module's own context, class `error`, reason `{case_clause,1}`. Unchanged.
    //
    // `MinimalContext` provides the heap arena the `{case_clause,1}` reason term is
    // built on; no seam/BIF is needed for a native raise.
    let (caller, run) = native_raise_module();
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::new();

    let outcome = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect("the native raise must be caught");

    let expected = terms::make_tuple(
        &mut ctx,
        &[caller_atom(&caller, "case_clause"), make_small(1)],
    )
    .expect("the expected reason builds on the arena");
    match outcome {
        RunOutcome::Returned(reason) => assert!(
            cmp(reason, expected, &caller.atom_table).is_eq(),
            "the caught reason must be {{case_clause,1}}"
        ),
        other => panic!("expected the caught reason, got {other:?}"),
    }
    // The context owns the arena the reason tuple lives on.
    let _ = &ctx;
}

// ---------------------------------------------------------------------------
// (6) an UNCAUGHT raise still propagates the caller's own typed error
// ---------------------------------------------------------------------------

/// `run/0` with **no** `try`: a plain `call_ext` straight through.
fn unguarded_module(callee_module: &str, callee_function: &str, callee_arity: u32) -> (Module, Atom) {
    let mut f = Fixture::new("m");
    let target = f.import(callee_module, callee_function, callee_arity);
    let boom = f.atom("boom");
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 0,
        },
    ];
    if callee_arity == 1 {
        code.push(Instr::Move {
            src: Operand::Atom(boom),
            dst: Operand::X(0),
        });
    }
    code.extend([
        Instr::CallExt {
            target,
            arity: callee_arity,
        },
        Instr::Deallocate(0),
        Instr::Return,
    ]);
    let run = f.add_function("run", 0, code);
    f.add_export("run", 0, run);
    let run_atom = f.atom("run");
    (f.finish(), run_atom)
}

#[test]
fn uncaught_atom_raise_still_propagates() {
    let (caller, run) = unguarded_module("erlang", "error", 1);
    let mut ctx = CatchCtx::new(callees(), raising_bif());
    let mut machine = Machine::new();

    let err = machine
        .run_export_outcome(&caller, run, 0, &[], &mut ctx)
        .expect_err("no catch frame: the raise propagates");

    match err {
        BeamError::Raise(raise) => {
            assert_eq!(raise.class, ExceptionClass::Error);
            assert_eq!(raise.reason.as_atom(), Some("boom"));
        }
        other => panic!("expected a typed raise, got {other:?}"),
    }
    assert_eq!(machine.catch_frame_depth(), 0);
}
