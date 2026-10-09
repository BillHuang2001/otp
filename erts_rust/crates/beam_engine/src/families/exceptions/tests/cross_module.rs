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

//! Cross-module exception containment: a raise inside a callee this machine
//! entered through the cross-module-call seam must land in the **caller's** live
//! handler even though that handler lives in another module context.
//!
//! The shape is the boot path's: `proc_lib:init_p_do_apply/3` is
//!
//! ```text
//! try apply(M, F, A) catch Class:Reason:Stacktrace -> exit_p(...) end
//! ```
//!
//! so the `try` pushes its catch frame in **`proc_lib`'s** module context while
//! the `apply` (or the `call_ext` it compiles to) runs the callee in a *different*
//! one. A `throw` from the callee must unwind back into `proc_lib`'s handler —
//! C: `handle_error` → `next_catch` finds the catch tag in the *frame*, and a code
//! address identifies its own module, so the transfer crosses modules freely
//! (`../../erts/emulator/beam/beam_common.c:580-679`).
//!
//! The engine reports such a landing as [`Step::Enter`](crate::Step::Enter) — the
//! driver switches the module context and re-binds the literal area — instead of
//! the intra-module [`Step::Goto`](crate::Step::Goto); the single decision point is
//! `Machine::throw_with_stacktrace` (`src/machine/catch.rs`), which compares the
//! landing catch frame's stamped context with the machine's current one. These
//! tests pin the *observable* consequences of every containment shape the compiler
//! emits, from the caller's point of view:
//!
//! 1. [`a_try_ofs_normal_path_returns_the_of_clause_value_for_a_cross_module_body`] —
//!    the `try ... of ... catch` normal path returns the `of` clause's value;
//! 2. [`a_raise_in_a_cross_module_callee_lands_in_the_callers_try_of_catch_handler`] —
//!    the same shape's exceptional path is contained by the caller;
//! 3. [`a_failing_of_clause_is_not_contained_by_its_own_try`] — the `try_end` pops
//!    the frame *before* the `of` dispatch, so a failing `of` clause is an uncaught
//!    `{try_clause,V}` (never a handler result);
//! 4. [`an_inner_handlers_reraise_lands_in_an_outer_handler_in_another_module_context`] —
//!    an inner handler that re-raises is contained by an outer handler in a
//!    *different* module context, class and reason included;
//! 5. [`a_throw_crossing_two_module_boundaries_lands_in_the_entry_modules_handler`] —
//!    A → B → C, contained by A's handler;
//! 6. [`compiler_generated_raises_are_contained_across_a_module_boundary`] — the
//!    containment is not specific to `raw_raise`: the raising *opcodes* the compiler
//!    emits (`badmatch`/`case_end`/`try_case_end`) are contained too.
//!
//! Plus the original `Apply`-seam case,
//! [`a_throw_in_a_cross_module_apply_callee_lands_in_the_callers_handler`].
//!
//! Every expectation below is the oracle's, measured with the system
//! `erl`/`erlc` (OTP 29 / erts-17.1) on the equivalent compiled Erlang
//! (`ca`, `cb` exporting the raising bodies, wrapped by a `try` in the entry
//! module):
//!
//! ```text
//! try ca:ok()      of 7 -> 100 catch C:R -> {C,R} end   =>  100
//! try ca:boom()    of 7 -> 100 catch C:R -> {C,R} end   =>  {throw,42}
//! try ca:ok()      of 1 -> 100 catch C:R -> {C,R} end   =>  uncaught error:{try_clause,7}
//! try cb:inner()                 catch C:R -> {C,R} end =>  {throw,43}   % inner re-raises 43
//! try cb:b()                     catch C:R -> {C,R} end =>  {throw,99}   % A -> B -> C
//! try ca:badm()                  catch C:R -> {C,R} end =>  {error,{badmatch,{error,1}}}
//! try ca:case_end()              catch C:R -> {C,R} end =>  {error,{case_clause,2}}
//! try ca:try_clause()            catch C:R -> {C,R} end =>  {error,{try_clause,2}}
//! ```
use super::*;
use crate::context::{ExternalResolver, MinimalContext};
use crate::machine::RunOutcome;
use beam_loader::{Atom, Export, Import, ImportRef};
use std::sync::Arc;

/// One shared atom universe: every module is built from the **same** table, so a
/// name has the same index everywhere. This is what `beam_rt`'s code server does
/// for the whole loaded corpus (one shared `AtomTable`,
/// `../../beam_rt/src/codeserver.rs`) — and what makes the exception *class* atoms
/// comparable across modules, exactly as the C's globally interned `am_thrown` etc.
/// are.
struct Universe {
    atoms: beam_term::AtomTable,
}

impl Universe {
    fn new(names: &[&str]) -> Universe {
        let mut atoms = beam_term::AtomTable::new();
        for name in names {
            atoms.intern(name);
        }
        Universe { atoms }
    }

    fn atom(&self, name: &str) -> Atom {
        self.atoms
            .lookup(name)
            .unwrap_or_else(|| panic!("{name} is not seeded"))
    }

    /// The atom `name` as the term the interpreter would carry, so an expected
    /// value can be built and compared with [`cmp`].
    fn term(&self, name: &str) -> Eterm {
        make_atom(self.atom(name))
    }

    /// A module whose atom table starts as the shared prefix (indices stay global).
    fn module(&self, name: &str) -> Module {
        let mut module = Module::default();
        module.atom_table = self.atoms.clone();
        module.name = self.atom(name);
        module
    }

    /// Assert `module`'s atom table is still exactly the shared universe.
    ///
    /// A module that `intern`ed a name the universe does not carry would append it
    /// at an *index of its own*: the same atom name would then be two different
    /// `Eterm`s in two modules, so a class atom installed in one module would fail to
    /// compare equal to the other module's — the exact silent divergence the shared
    /// table exists to rule out. Every name a fixture mentions must therefore be
    /// seeded, and this guard turns a forgotten one into a loud failure instead of a
    /// confusing mismatch.
    fn assert_sealed(&self, module: &Module) {
        assert_eq!(
            module.atom_table.len(),
            self.atoms.len(),
            "module `{}` interned an atom outside the shared universe",
            module.atom_name(module.name).unwrap_or("<unnamed>"),
        );
    }
}

/// Append a function; returns its entry label (the first label in `code`).
fn add_function(module: &mut Module, name: &str, arity: u32, code: Vec<Instr>) -> Label {
    let name = module.atom_table.intern(name);
    let module_name = module.name;
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    let entry = labels.keys().next().copied().unwrap_or(Label(0));
    module.functions.push(Function {
        func_label: entry,
        entry_label: entry,
        module: module_name,
        name,
        arity,
        code,
        blocks: Vec::new(),
        labels,
    });
    entry
}

/// Append the import `module:function/arity` and return its reference (what a
/// compiled `call_ext` names in its operand).
fn add_import(module: &mut Module, module_name: &str, function: &str, arity: u32) -> ImportRef {
    let module_atom = module.atom_table.intern(module_name);
    let function_atom = module.atom_table.intern(function);
    module.imports.push(Import {
        module: module_atom,
        function: function_atom,
        arity,
    });
    ImportRef((module.imports.len() - 1) as u32)
}

/// Export `function/arity` at `label` (what the seam resolves the callee through).
fn add_export(module: &mut Module, function: &str, arity: u32, label: Label) {
    let function = module.atom_table.intern(function);
    module.exports.push(Export {
        function,
        arity,
        label,
    });
}

/// A resolver answering the cross-module-call seam
/// ([`ExecutionContext::resolve_external_call`](crate::ExecutionContext::resolve_external_call))
/// for each `(module, function, callee)` triple with the callee's **own** export
/// entry — the shape `beam_rt`'s code server answers with, and the one the engine
/// requires (the same `Arc<Module>` it loaded the module with, and the export entry
/// label the run would enter).
fn resolve_callees(callees: Vec<(&'static str, &'static str, Arc<Module>)>) -> ExternalResolver {
    Box::new(move |module, function, arity| {
        for (name, target, callee) in &callees {
            if *name != module || *target != function {
                continue;
            }
            let Some(function_atom) = callee.atom_table.lookup(function) else {
                continue;
            };
            if let Some(label) = callee.export(function_atom, arity) {
                return Ok(Some((Arc::clone(callee), label)));
            }
        }
        Ok(None)
    })
}

// ---------------------------------------------------------------------------
// Fixture builders (the compiler's instruction shapes)
// ---------------------------------------------------------------------------

/// The caller `a`: `try <callee>:<target>() catch C:R -> {C,R} end`.
///
/// ```text
/// run/0:
///     allocate 1
///     try y(0) 20             ; the catch frame belongs to THIS module
///     call_ext c:<target>/0
///     try_end y(0)
///     deallocate 1
///     return
/// 20: try_case y(0)           ; x(0)=THE_NON_VALUE, x(1)=reason, x(3)=class atom
///     test_heap 3, 4
///     put_tuple2 x(0) [x(3), x(1)]   ; `catch C:R -> {C,R} end`
///     deallocate 1
///     return
/// ```
///
/// The handler returns `{Class, Reason}`, so one landed value pins **both** halves
/// of the exception the outer module observes.
fn outer_guarded_caller(universe: &Universe, callee: &str, target: &str) -> (Module, Label) {
    let mut caller = universe.module("a");
    let call = add_import(&mut caller, callee, target, 0);
    let entry = add_function(
        &mut caller,
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(20),
            },
            Instr::CallExt {
                target: call,
                arity: 0,
            },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(20)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::TestHeap {
                heap_need: HeapNeed::Words(3),
                live: 4,
            },
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![Operand::X(3), Operand::X(1)],
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    (caller, entry)
}

/// The `try ... of ... catch` shape the compiler emits for
/// `try <callee>:<target>() of 7 -> 100 catch C:R -> {C,R} end` (`erlc -S` of the
/// probe module; the `of` dispatch is the `select_val` → `try_case` pair):
///
/// ```text
/// run/0:
///     allocate 1
///     try y(0) 20                   ; the catch frame belongs to THIS module
///     call_ext c:<target>/0         ; the `of` expression
///     try_end y(0)                  ; NOTE: the frame is popped *before* the dispatch
///     select_val x(0) 40 [7 -> 10]  ; the `of` clause dispatch
/// 10: move 100 -> x(0)              ; the `of` clause body ran
///     deallocate 1
///     return
/// 20: try_case y(0)                 ; the catch handler (same fold as above)
///     test_heap 3, 4
///     put_tuple2 x(0) [x(3), x(1)]
///     deallocate 1
///     return
/// 40: try_case_end x(0)             ; no `of` clause matched => {try_clause, V}
/// ```
fn try_of_caller(universe: &Universe, callee: &str, target: &str) -> (Module, Label) {
    let mut caller = universe.module("a");
    let call = add_import(&mut caller, callee, target, 0);
    let entry = add_function(
        &mut caller,
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(20),
            },
            Instr::CallExt {
                target: call,
                arity: 0,
            },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::SelectVal {
                value: Operand::X(0),
                fail: Label(40),
                pairs: vec![(Operand::Integer(7), Label(10))],
            },
            Instr::Label(Label(10)),
            Instr::Move {
                src: Operand::Integer(100),
                dst: Operand::X(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(20)),
            Instr::TryCase {
                frame: Operand::Y(0),
            },
            Instr::TestHeap {
                heap_need: HeapNeed::Words(3),
                live: 4,
            },
            Instr::PutTuple2 {
                dst: Operand::X(0),
                elements: vec![Operand::X(3), Operand::X(1)],
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(40)),
            Instr::TryCaseEnd {
                value: Operand::X(0),
            },
        ],
    );
    (caller, entry)
}

/// A callee module exporting `function/0` whose body is `code` (entry `Label(1)`).
fn callee_module(
    universe: &Universe,
    module_name: &str,
    function: &str,
    code: Vec<Instr>,
) -> Module {
    let mut module = universe.module(module_name);
    let entry = add_function(&mut module, function, 0, code);
    add_export(&mut module, function, 0, entry);
    module
}

/// A callee `function/0` that returns the small integer `value`.
fn returning_callee(universe: &Universe, module_name: &str, function: &str, value: i64) -> Module {
    callee_module(
        universe,
        module_name,
        function,
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Integer(value),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    )
}

/// A callee `function/0` that `throw`s `value` (`raw_raise` with x(0) = the `throw`
/// class atom, x(1) = the value, x(2) = the empty raw stacktrace — what
/// `throw(42)` compiles to through `sys_core_fold`'s `raw_raise`).
fn throwing_callee(universe: &Universe, module_name: &str, function: &str, value: i64) -> Module {
    let throw = universe.atom("throw");
    callee_module(
        universe,
        module_name,
        function,
        vec![
            Instr::Label(Label(1)),
            Instr::Move {
                src: Operand::Atom(throw),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(value),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Nil,
                dst: Operand::X(2),
            },
            Instr::RawRaise,
        ],
    )
}

/// How an inner handler re-raises what it caught — the two shapes the compiler
/// emits and [`super::mod`](super) executes.
#[derive(Clone, Copy, Debug)]
enum ReRaise {
    /// The `raise Trace Value` opcode (what `catch C:_ -> erlang:raise(C, V, [])`
    /// folds to): the class is the exception currently being handled
    /// (`Machine::pending_class`), never re-derived from the operands.
    Opcode,
    /// `raw_raise` with the landed class atom still in `x(0)` after `try_case`.
    Raw,
}

/// The middle module `b`: `try c:<target>() of V -> V catch C:_ -> raise(C, 43, []) end`
/// — an inner handler that re-raises the class it caught with a **different** reason
/// (`43`), so an outer handler that observes `43` proves the *re-raise* crossed into
/// it (not the original raise, whose reason is 42).
///
/// ```text
/// inner/0:
///     allocate 1
///     try y(0) 20
///     call_ext c:<target>/0
///     try_end y(0)
///     deallocate 1
///     return
/// 20: try_case y(0)             ; x(0)=class atom, x(1)=reason, x(2)=raw trace
///     move 43 -> x(1)
///     raise x(2) x(1)           ; or `raw_raise` (x(0) is still the class atom)
/// ```
fn re_raising_callee(
    universe: &Universe,
    module_name: &str,
    function: &str,
    target: &str,
    re_raise: ReRaise,
) -> Module {
    let mut module = universe.module(module_name);
    let call = add_import(&mut module, "c", target, 0);
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(20),
        },
        Instr::CallExt {
            target: call,
            arity: 0,
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(20)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Integer(43),
            dst: Operand::X(1),
        },
    ];
    code.push(match re_raise {
        ReRaise::Opcode => Instr::Raise {
            trace: Operand::X(2),
            value: Operand::X(1),
        },
        ReRaise::Raw => Instr::RawRaise,
    });
    let entry = add_function(&mut module, function, 0, code);
    add_export(&mut module, function, 0, entry);
    module
}

/// The middle module `b` of the A → B → C chain: `b/0` is `V = c:<target>(), V` — a
/// **non-tail** cross-module call, so `b` owns a live frame the raise must unwind
/// through on its way back to `a` (the oracle compiles `b() -> V = ca:c_throw(), V.`
/// as a real `call_ext` with a frame, not a `call_ext_only` tail call).
fn forwarding_callee(
    universe: &Universe,
    module_name: &str,
    function: &str,
    target_module: &str,
    target: &str,
) -> Module {
    let mut module = universe.module(module_name);
    let call = add_import(&mut module, target_module, target, 0);
    let entry = add_function(
        &mut module,
        function,
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::CallExt {
                target: call,
                arity: 0,
            },
            Instr::Deallocate(1),
            Instr::Return,
        ],
    );
    add_export(&mut module, function, 0, entry);
    module
}

/// Run `caller`'s entry function with `ctx` and assert the machine ended **back at
/// the bottom frame, with no catch frame left, in the entry module's context** —
/// i.e. the raise was contained rather than escaping or corrupting the frame stack.
fn run_and_assert_contained(
    machine: &mut Machine,
    caller: &Module,
    entry: Label,
    ctx: &mut MinimalContext,
) -> RunOutcome {
    let outcome = machine
        .run_label_outcome(caller, entry, 0, &[], ctx)
        .expect("the cross-module raise must be contained by the caller's handler");
    assert_eq!(
        machine.frame_depth(),
        1,
        "the run is back at the bottom frame"
    );
    assert_eq!(
        machine.catch_frame_depth(),
        0,
        "the handler consumed exactly its catch frame"
    );
    assert_eq!(
        machine.current_context(),
        0,
        "the run ended in the entry module's context"
    );
    outcome
}

/// The `{Class, Reason}` an `outer_guarded_caller`/`try_of_caller` handler returns.
fn expected_class_reason(
    ctx: &mut MinimalContext,
    universe: &Universe,
    class: &str,
    value: i64,
) -> Eterm {
    let class = universe.term(class);
    terms::make_tuple(ctx, &[class, make_small(value)]).expect("the expected tuple is buildable")
}

// ---------------------------------------------------------------------------
// 0. the `Apply` seam (the original case)
// ---------------------------------------------------------------------------

/// The `proc_lib` shape: `run/0` guards a cross-module target with a `try`/`catch`
/// whose handler returns the landed reason from `x(1)`.
///
/// ```text
/// run/0:
///     allocate 1
///     try y(0) 20              ; the catch frame belongs to THIS module
///     move c -> x(0)           ; `apply c:throwing/0`
///     move throwing -> x(1)
///     apply 0
///     try_end y(0)
///     deallocate 1
///     return
/// 20: try_case y(0)            ; x(0) = the class atom
///     move x(1) -> x(0)        ; the reason
///     deallocate 1
///     return
/// ```
fn guarded_caller(universe: &Universe, callee: &str, target: &str) -> (Module, Module, Label) {
    let mut caller = universe.module("m");
    let callee_atom = universe.atom(callee);
    let target_atom = universe.atom(target);
    let entry = add_function(
        &mut caller,
        "run",
        0,
        vec![
            Instr::Label(Label(1)),
            Instr::Allocate {
                stack_need: HeapNeed::Words(1),
                live: 0,
            },
            Instr::Try {
                frame: Operand::Y(0),
                fail: Label(20),
            },
            Instr::Move {
                src: Operand::Atom(callee_atom),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Atom(target_atom),
                dst: Operand::X(1),
            },
            Instr::Apply { arity: 0 },
            Instr::TryEnd {
                frame: Operand::Y(0),
            },
            Instr::Deallocate(1),
            Instr::Return,
            Instr::Label(Label(20)),
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
    let mut callee_module = universe.module(callee);
    let throwing = add_function(
        &mut callee_module,
        target,
        0,
        vec![
            Instr::Label(Label(1)),
            // `throw(42)` — `raw_raise` with x(0) = class, x(1) = value, x(2) = [].
            Instr::Move {
                src: Operand::Atom(universe.atom("throw")),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(42),
                dst: Operand::X(1),
            },
            Instr::Move {
                src: Operand::Nil,
                dst: Operand::X(2),
            },
            Instr::RawRaise,
        ],
    );
    add_export(&mut callee_module, target, 0, throwing);
    (caller, callee_module, entry)
}

#[test]
fn a_throw_in_a_cross_module_apply_callee_lands_in_the_callers_handler() {
    let universe = Universe::new(&["m", "c", "run", "throwing", "throw", "error", "EXIT"]);
    let (caller, callee, entry) = guarded_caller(&universe, "c", "throwing");
    universe.assert_sealed(&caller);
    universe.assert_sealed(&callee);
    let target = Arc::new(callee);
    let mut ctx = MinimalContext::with_external_resolver(resolve_callees(vec![(
        "c",
        "throwing",
        Arc::clone(&target),
    )]));
    let mut machine = Machine::new();

    let outcome = machine
        .run_label_outcome(&caller, entry, 0, &[], &mut ctx)
        .expect("the cross-module throw must be contained by the caller's catch");

    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(42)),
        "the thrown value reaches the caller's handler"
    );
    assert_eq!(
        machine.frame_depth(),
        1,
        "the run is back at the bottom frame"
    );
    assert_eq!(
        machine.catch_frame_depth(),
        0,
        "the handler consumed its frame"
    );
    assert_eq!(
        machine.current_context(),
        0,
        "the handler ran in the caller's context, not the callee's"
    );
}

// ---------------------------------------------------------------------------
// 1. `try ... of ... catch`
// ---------------------------------------------------------------------------

/// Case 1a — the normal path. The callee runs in another module context, so the
/// body of the `try ... of` is executed there and its `return` transfers back to the
/// caller's context (`Step::Enter { context: 0, .. }` from the cross-module
/// `return`) before the `of` dispatch reads `x(0)`.
///
/// Oracle: `try ca:ok() of 7 -> 100 catch C:R -> {C,R} end` ⇒ `100`.
#[test]
fn a_try_ofs_normal_path_returns_the_of_clause_value_for_a_cross_module_body() {
    let universe = Universe::new(&["a", "c", "run", "ok", "throw", "error", "try_clause"]);
    let (caller, entry) = try_of_caller(&universe, "c", "ok");
    let callee = returning_callee(&universe, "c", "ok", 7);
    universe.assert_sealed(&caller);
    universe.assert_sealed(&callee);
    let callee = Arc::new(callee);
    let mut ctx = MinimalContext::with_external_resolver(resolve_callees(vec![(
        "c",
        "ok",
        Arc::clone(&callee),
    )]));
    let mut machine = Machine::new();

    let outcome = run_and_assert_contained(&mut machine, &caller, entry, &mut ctx);

    assert_eq!(
        outcome,
        RunOutcome::Returned(make_small(100)),
        "the `of` clause body ran after the cross-module body returned"
    );
    assert_eq!(
        machine.pending_exception(),
        None,
        "the normal path raises nothing"
    );
}

/// Case 1b — the exceptional path of the same `try ... of ... catch` shape: the
/// raise happens in the callee's module context and must be contained by the
/// caller's `try` (the `of` clauses never run — the handler is entered directly).
///
/// Oracle: `try ca:boom() of 7 -> 100 catch C:R -> {C,R} end` ⇒ `{throw, 42}`.
#[test]
fn a_raise_in_a_cross_module_callee_lands_in_the_callers_try_of_catch_handler() {
    let universe = Universe::new(&["a", "c", "run", "boom", "throw", "error", "try_clause"]);
    let (caller, entry) = try_of_caller(&universe, "c", "boom");
    let callee = throwing_callee(&universe, "c", "boom", 42);
    universe.assert_sealed(&caller);
    universe.assert_sealed(&callee);
    let callee = Arc::new(callee);
    let mut ctx = MinimalContext::with_external_resolver(resolve_callees(vec![(
        "c",
        "boom",
        Arc::clone(&callee),
    )]));
    let mut machine = Machine::new();

    let outcome = run_and_assert_contained(&mut machine, &caller, entry, &mut ctx);

    let expected = expected_class_reason(&mut ctx, &universe, "throw", 42);
    let RunOutcome::Returned(landed) = outcome else {
        panic!("expected the catch handler's {{Class,Reason}}, got {outcome:?}");
    };
    assert!(
        cmp(landed, expected, &caller.atom_table).is_eq(),
        "the caller's catch handler saw {{throw, 42}}"
    );
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Throw, make_small(42))),
        "the class AND reason survive into the caller's context"
    );
    assert_eq!(
        machine.x(3),
        Some(universe.term("throw")),
        "the class atom landed in the caller's handler registers"
    );
}

/// Case 1c — the `of` dispatch is *not* protected by the `try`'s own catch: the
/// compiler emits `try_end` (which pops the frame) before the `of` clauses, so a
/// failing `of` clause raises `{try_clause, V}` with no live frame and the run
/// propagates it — exactly as the oracle does.
///
/// Oracle: `try ca:ok() of 1 -> 100 catch C:R -> {C,R} end`
/// ⇒ uncaught `error:{try_clause,7}`.
#[test]
fn a_failing_of_clause_is_not_contained_by_its_own_try() {
    let universe = Universe::new(&["a", "c", "run", "ok", "throw", "error", "try_clause"]);
    let (caller, entry) = try_of_caller(&universe, "c", "ok");
    // `8` matches neither the `of` clause's `7` nor the catch handler: the `of`
    // dispatch fails, and its frame was already popped by `try_end`.
    let callee = returning_callee(&universe, "c", "ok", 8);
    universe.assert_sealed(&caller);
    universe.assert_sealed(&callee);
    let callee = Arc::new(callee);
    let mut ctx = MinimalContext::with_external_resolver(resolve_callees(vec![(
        "c",
        "ok",
        Arc::clone(&callee),
    )]));
    let mut machine = Machine::new();

    let err = machine
        .run_label_outcome(&caller, entry, 0, &[], &mut ctx)
        .expect_err("the `try_end` popped the frame before the `of` dispatch");
    let raise = match err {
        BeamError::Raise(raise) => raise,
        other => panic!("expected a typed raise, got {other:?}"),
    };
    assert_eq!(raise.class, ExceptionClass::Error);
    let landed = raise
        .live_reason()
        .and_then(|live| live.downcast_ref::<Eterm>())
        .copied()
        .expect("the uncaught reason term rides along");
    assert_reason_tuple(&mut ctx, &caller, landed, "try_clause", make_small(8));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, landed)),
        "the raised {{try_clause,V}} is the recorded pending exception"
    );
    assert_eq!(
        machine.catch_frame_depth(),
        0,
        "the try_end consumed the caller's frame before the `of` dispatch"
    );
}

// ---------------------------------------------------------------------------
// 2. nested catches, the inner handler re-raises
// ---------------------------------------------------------------------------

/// Case 2 — the inner handler (`b`'s) re-raises; the outer handler (`a`'s) lives in
/// another module context and must be the one to observe the re-raised class and
/// reason. The inner handler replaces the reason with `43`, so an outer handler
/// seeing `{throw, 43}` proves it saw the **re-raise** crossing the module boundary
/// (`Step::Enter { context: 0, .. }`), not the original `throw(42)`.
///
/// Both re-raise shapes are exercised: the `raise` opcode (class preserved from
/// `Machine::pending_class`) and `raw_raise` with the landed class atom still in
/// `x(0)`.
///
/// Oracle: with `inner() -> try ca:boom() of V -> {matched,V} catch C:_ ->
/// erlang:raise(C, 43, []) end`, `try cb:inner() catch C:R -> {C,R} end` ⇒
/// `{throw, 43}`.
#[test]
fn an_inner_handlers_reraise_lands_in_an_outer_handler_in_another_module_context() {
    for re_raise in [ReRaise::Opcode, ReRaise::Raw] {
        let universe = Universe::new(&["a", "b", "c", "run", "inner", "boom", "throw", "error"]);
        let (caller, entry) = outer_guarded_caller(&universe, "b", "inner");
        let middle = re_raising_callee(&universe, "b", "inner", "boom", re_raise);
        let callee = throwing_callee(&universe, "c", "boom", 42);
        universe.assert_sealed(&caller);
        universe.assert_sealed(&middle);
        universe.assert_sealed(&callee);
        let middle = Arc::new(middle);
        let callee = Arc::new(callee);
        let mut ctx = MinimalContext::with_external_resolver(resolve_callees(vec![
            ("b", "inner", Arc::clone(&middle)),
            ("c", "boom", Arc::clone(&callee)),
        ]));
        let mut machine = Machine::new();

        let outcome = run_and_assert_contained(&mut machine, &caller, entry, &mut ctx);

        let expected = expected_class_reason(&mut ctx, &universe, "throw", 43);
        let RunOutcome::Returned(landed) = outcome else {
            panic!("{re_raise:?}: expected the outer handler's value, got {outcome:?}");
        };
        assert!(
            cmp(landed, expected, &caller.atom_table).is_eq(),
            "{re_raise:?}: the outer handler (in module `a`) saw the re-raised {{throw, 43}}"
        );
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Throw, make_small(43))),
            "{re_raise:?}: the re-raised class AND reason survive the module-context crossing"
        );
        assert_eq!(
            machine.x(3),
            Some(universe.term("throw")),
            "{re_raise:?}: the class atom landed in the outer handler's registers"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. a `throw` crossing two module boundaries
// ---------------------------------------------------------------------------

/// Case 3 — the callee chain `a:run/0` → `b:b/0` → `c:c_throw/0` is established
/// entirely through the external-call seam, and `b` owns a live frame the raise has
/// to unwind through. The `throw(99)` from `c` must reach `a`'s handler with the
/// machine back at the bottom frame and the catch frame consumed.
///
/// Oracle: with `cb:b() -> V = ca:c_throw(), V.` and `ca:c_throw() -> throw(99)`,
/// `try cb:b() catch C:R -> {C,R} end` ⇒ `{throw, 99}`.
#[test]
fn a_throw_crossing_two_module_boundaries_lands_in_the_entry_modules_handler() {
    let universe = Universe::new(&["a", "b", "c", "run", "b", "c_throw", "throw", "error"]);
    let (caller, entry) = outer_guarded_caller(&universe, "b", "b");
    let middle = forwarding_callee(&universe, "b", "b", "c", "c_throw");
    let callee = throwing_callee(&universe, "c", "c_throw", 99);
    universe.assert_sealed(&caller);
    universe.assert_sealed(&middle);
    universe.assert_sealed(&callee);
    let middle = Arc::new(middle);
    let callee = Arc::new(callee);
    let mut ctx = MinimalContext::with_external_resolver(resolve_callees(vec![
        ("b", "b", Arc::clone(&middle)),
        ("c", "c_throw", Arc::clone(&callee)),
    ]));
    let mut machine = Machine::new();

    let outcome = run_and_assert_contained(&mut machine, &caller, entry, &mut ctx);

    let expected = expected_class_reason(&mut ctx, &universe, "throw", 99);
    let RunOutcome::Returned(landed) = outcome else {
        panic!("expected the entry module's handler value, got {outcome:?}");
    };
    assert!(
        cmp(landed, expected, &caller.atom_table).is_eq(),
        "the thrown value crossed two module boundaries into `a`'s handler"
    );
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Throw, make_small(99))),
        "the throw's class and reason are the ones `c` raised"
    );
}

// ---------------------------------------------------------------------------
// 4. compiler-generated raises (not `raw_raise`)
// ---------------------------------------------------------------------------

/// The containment is **not** a `raw_raise`-specific path: the raising *opcodes* the
/// compiler emits expand their reason at raise time and go through the same single
/// machine entry point, so each must be contained by a handler living in another
/// module context.
///
/// Oracle: `try ca:badm() catch C:R -> {C,R} end` ⇒ `{error,{badmatch,{error,1}}}`
/// (the fixture raises the same shape with the value `9`), `ca:case_end()` ⇒
/// `{error,{case_clause,2}}`, `ca:try_clause()` ⇒ `{error,{try_clause,2}}`.
#[test]
fn compiler_generated_raises_are_contained_across_a_module_boundary() {
    for (raise, tag, value) in [
        (
            Instr::Badmatch {
                value: Operand::Integer(9),
            },
            "badmatch",
            9,
        ),
        (
            Instr::CaseEnd {
                value: Operand::Integer(2),
            },
            "case_clause",
            2,
        ),
        (
            Instr::TryCaseEnd {
                value: Operand::Integer(2),
            },
            "try_clause",
            2,
        ),
    ] {
        let universe = Universe::new(&[
            "a",
            "c",
            "run",
            "f",
            "badmatch",
            "case_clause",
            "try_clause",
            "error",
            "throw",
        ]);
        let (caller, entry) = outer_guarded_caller(&universe, "c", "f");
        let callee = callee_module(&universe, "c", "f", vec![Instr::Label(Label(1)), raise]);
        universe.assert_sealed(&caller);
        universe.assert_sealed(&callee);
        let callee = Arc::new(callee);
        let mut ctx = MinimalContext::with_external_resolver(resolve_callees(vec![(
            "c",
            "f",
            Arc::clone(&callee),
        )]));
        let mut machine = Machine::new();

        let outcome = run_and_assert_contained(&mut machine, &caller, entry, &mut ctx);

        // The handler returns `{Class, Reason}`, so the landed value is
        // `{error, {<tag>, <value>}}` — the oracle's own shape.
        let tag_atom = universe.term(tag);
        let inner = terms::make_tuple(&mut ctx, &[tag_atom, make_small(value)])
            .expect("the inner reason tuple is buildable");
        let expected = terms::make_tuple(&mut ctx, &[universe.term("error"), inner])
            .expect("the outer tuple is buildable");
        let RunOutcome::Returned(landed) = outcome else {
            panic!("{tag}: expected the caller's handler value, got {outcome:?}");
        };
        assert!(
            cmp(landed, expected, &caller.atom_table).is_eq(),
            "{tag}: the caller's handler saw {{error, {{{tag}, {value}}}}}"
        );
        let reason = machine
            .x(1)
            .unwrap_or_else(|| panic!("{tag}: x(1) holds the landed reason"));
        assert_reason_tuple(&mut ctx, &caller, reason, tag, make_small(value));
        assert_eq!(
            machine.pending_exception(),
            Some((ExceptionClass::Error, reason)),
            "{tag}: the class `error` and the expanded reason are recorded"
        );
    }
}
