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

//! Exceptions-family integration test over the committed oracle fixture.
//!
//! Loads `tests/fixtures/exceptions.beam` (a real `.beam` produced by the system
//! `erlc` — the differential oracle) with `beam_loader::load_file` and runs its
//! exports on the interpreter. Every expected value is the oracle's (re-derived by
//! running the same module under the system `erl`), except where a documented
//! Milestone-1 gap changes the observable result (noted at the assertion).
//!
//! The fixture is deliberately restricted to the opcodes the interpreter + the
//! exceptions family execute (no `put_tuple2`/`get_tuple_element`/… stubs), so a
//! `catch` clause matches tuple elements only with `_`.
//!
//! **The `MinimalContext` must outlive every boxed result.** The context owns the
//! arena backing `alloc_words`, so a heap term the engine builds (e.g. the
//! unambiguous `{'EXIT',V}` tuple) is only valid while its context is alive — the
//! tests below create the context explicitly and keep it in scope.
//!
//! This file proves the machinery on **one** committed fixture; `tests/exceptions_corpus.rs`
//! (sibling) proves its **corpus reach** — the same raise opcodes executing on the real
//! installed `stdlib`/`kernel`/`compiler` modules and producing the system `erl`'s reason.

use std::collections::BTreeMap;
use std::process::Command;

use beam_common::{BeamError, ExceptionClass, ExitReason};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{
    load_file, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{
    atom_val, cmp, is_atom, is_nil, is_small, make_atom, make_small, signed_value, Atom, Eterm,
    NIL,
};

mod support;

use support::{assert_atom, ModuleHost, SEED_ATOMS};

/// The committed oracle fixture (produced by `erlc exceptions.erl`).
const EXCEPTIONS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/exceptions.beam"
);

fn module() -> Module {
    let mut module = load_file(EXCEPTIONS_BEAM).expect("load exceptions.beam");
    // `no` is only ever passed *as a value* by the tests (the `badmatch` cases);
    // it is not a literal in the fixture, so the module's atom table does not list
    // it. Intern it so the argument term is representable — adding a name to the
    // interning table leaves every existing lookup (the module's own atoms and the
    // class/tag atoms the family installs) unchanged.
    module.atom_table.intern("no");
    // A real runtime keeps the well-known exception atoms — `file`/`line` included
    // (`../../erts/emulator/beam/beam_common.c:382-406`) — globally interned; the
    // bare loader interns only what the module's own atom chunk names. Seed them so
    // the `catch_end` fold's cooked stacktrace can carry its source location — the
    // same policy the `tests/support` host applies to every module it loads
    // (`SEED_ATOMS`).
    module.atom_table.intern("file");
    module.atom_table.intern("line");
    module
}

/// Resolve a name interned in the module's atom table.
fn atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned in the module"))
}

/// The atom term for `name`.
fn atom_term(module: &Module, name: &str) -> Eterm {
    make_atom(atom(module, name))
}

/// Run `name/arity` on `ctx` and return the result + the machine.
fn run(
    module: &Module,
    name: &str,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> (Result<Eterm, BeamError>, Machine) {
    let mut machine = Machine::new();
    let result = machine.run_export(module, atom(module, name), args.len() as u32, args, ctx);
    (result, machine)
}

/// Run `name/arity` and expect the oracle's value (the context stays alive for
/// the caller, so a boxed result remains valid).
fn expect(module: &Module, name: &str, args: &[Eterm], ctx: &mut dyn ExecutionContext) -> Eterm {
    run(module, name, args, ctx)
        .0
        .unwrap_or_else(|err| panic!("{name} raised {err:?}"))
}

/// The `{Module,Function,Arity,Location}` stacktrace item the engine cooks (and the
/// oracle prints), built from `module`'s own atom table.
///
/// `module` names the frame's **own** module (the item's first element): a frame is
/// always resolved against the module its `(func, pc)` belongs to.
fn frame(
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    name: &str,
    arity: i64,
    location: Eterm,
) -> Eterm {
    terms::make_tuple(
        ctx,
        &[
            make_atom(module.name),
            atom_term(module, name),
            make_small(arity),
            location,
        ],
    )
    .expect("build a stacktrace item")
}

/// The `[{file,Name},{line,N}]` `Location` element of a stacktrace item — an Erlang
/// string (a list of code points) for the file, exactly the oracle's shape.
fn location(ctx: &mut dyn ExecutionContext, module: &Module, file: &str, line: i64) -> Eterm {
    let codes: Vec<Eterm> = file
        .chars()
        .map(|c| make_small(i64::from(c as u32)))
        .collect();
    let name = terms::make_list(ctx, &codes).expect("build the file-name list");
    let file_pair =
        terms::make_tuple(ctx, &[atom_term(module, "file"), name]).expect("build the file pair");
    let line_pair = terms::make_tuple(ctx, &[atom_term(module, "line"), make_small(line)])
        .expect("build the line pair");
    terms::make_list(ctx, &[file_pair, line_pair]).expect("build the location list")
}

/// A BIF hook: `erlang:'/'/2` raises `badarith` when the divisor is `0`, exactly
/// the guard the fixture's `guard_badarith/1` exercises.
fn div_hook(m: &str, f: &str, a: u32, args: &[Eterm]) -> Result<Eterm, BeamError> {
    if m == "erlang" && f == "/" && a == 2 && args.len() == 2 {
        if is_small(args[1]) && signed_value(args[1]) == 0 {
            return Err(BeamError::BadArith);
        }
    }
    Err(BeamError::BadArg)
}

// ---------------------------------------------------------------------------
// Caught raises land in the clause and yield its value
// ---------------------------------------------------------------------------

#[test]
fn caught_raises_yield_the_matching_clause_value() {
    let module = module();
    let mut ctx = MinimalContext::new();
    // case_clause/1
    assert_eq!(
        expect(&module, "case_clause", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(&module, "case_clause", &[make_small(2)], &mut ctx),
        atom_term(&module, "caught_case_clause")
    );
    // badmatch/1
    assert_eq!(
        expect(&module, "badmatch", &[atom_term(&module, "ok")], &mut ctx),
        atom_term(&module, "matched")
    );
    assert_eq!(
        expect(&module, "badmatch", &[atom_term(&module, "no")], &mut ctx),
        atom_term(&module, "caught_badmatch")
    );
    // if_clause/1
    assert_eq!(
        expect(&module, "if_clause", &[make_small(1)], &mut ctx),
        atom_term(&module, "positive")
    );
    assert_eq!(
        expect(&module, "if_clause", &[make_small(-1)], &mut ctx),
        atom_term(&module, "caught_if_clause")
    );
    // try_clause/1 (raised from `inner_of/1` through the outer try)
    assert_eq!(
        expect(&module, "try_clause", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(&module, "try_clause", &[make_small(2)], &mut ctx),
        atom_term(&module, "caught_try_clause")
    );
}

#[test]
fn nested_and_cross_function_raises_reach_the_right_handler() {
    let module = module();
    let mut ctx = MinimalContext::new();
    // nested/1: the inner `throw:` clause does not match an `error`, so the
    // re-raise reaches the outer `error:{case_clause,_}` handler.
    assert_eq!(
        expect(&module, "nested", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(&module, "nested", &[make_small(2)], &mut ctx),
        atom_term(&module, "outer_case_clause")
    );
    // deep/1: the raise crosses a call boundary (`deep/1` -> `deep_inner/1`).
    assert_eq!(
        expect(&module, "deep", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(&module, "deep", &[make_small(2)], &mut ctx),
        atom_term(&module, "deep_case_clause")
    );
}

#[test]
fn a_failing_guard_bif_fails_the_guard_and_yields_the_case_clause() {
    // `guard_badarith/1` is `try case N of N when N/0 > 0 -> positive end catch
    // error:{case_clause,_} -> ... end`. The `gc_bif '/'/2` has a live fail label,
    // so a `BadArith` from the hook is a guard failure: the case clause fails and
    // `case_end` raises `{case_clause,1}` — exactly the oracle's value
    // (`guard_badarith(1) = caught_case_clause`).
    let module = module();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(div_hook));
    let result = run(&module, "guard_badarith", &[make_small(1)], &mut ctx).0;
    assert_eq!(
        result.expect("guard_badarith(1)"),
        atom_term(&module, "caught_case_clause")
    );
}

// ---------------------------------------------------------------------------
// Old-style `catch` (catch_end)
// ---------------------------------------------------------------------------

#[test]
fn oldcatch_escapes_the_raw_caught_value() {
    let module = module();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        expect(&module, "oldcatch", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );

    // The raw caught value escapes to the caller as C's `{'EXIT',{Reason,Stacktrace}}`
    // (class `error`, `add_stacktrace`):
    //   $ erl -noshell -pz <fixtures> -eval 'io:format("~p~n",[exceptions:oldcatch(2)]), halt().'
    //   {'EXIT',{{case_clause,2},
    //            [{exceptions,oldcatch,1,[{file,"exceptions.erl"},{line,29}]}
    //             | <host erl_eval/init frames>]}}
    // The raise rebuilt the machine's pre-unwind frame chain
    // (`Machine::raise_snapshot`) and `catch_end` cooks it, so the engine's
    // stacktrace is the oracle's own **leading** frames — the raising `oldcatch/1`
    // — never `[]`; the trailing host frames live outside this run.
    let actual = run(&module, "oldcatch", &[make_small(2)], &mut ctx)
        .0
        .expect("oldcatch(2) yields the EXIT tuple");
    let reason = terms::make_tuple(
        &mut ctx,
        &[atom_term(&module, "case_clause"), make_small(2)],
    )
    .unwrap();
    let location = location(&mut ctx, &module, "exceptions.erl", 29);
    let item = frame(&mut ctx, &module, "oldcatch", 1, location);
    let frames = terms::make_list(&mut ctx, &[item]).unwrap();
    let pair = terms::make_tuple(&mut ctx, &[reason, frames]).unwrap();
    let expected = terms::make_tuple(&mut ctx, &[atom_term(&module, "EXIT"), pair]).unwrap();
    assert!(
        cmp(actual, expected, &module.atom_table).is_eq(),
        "oldcatch(2) must be 'EXIT' paired with {{Reason,Stacktrace}}, Reason = \
         {{case_clause,2}} and Stacktrace = the oracle's leading frames: the raising \
         exceptions:oldcatch/1 at exceptions.erl:29"
    );
}

#[test]
fn oldcatch_match_distinguishes_the_exit_tuple_from_a_value() {
    let module = module();
    let mut ctx = MinimalContext::new();
    // `oldcatch_match/1` inspects `catch ...` in-module; label 29's `catch_end`
    // produces the `{'EXIT',_}` tuple for a landed error, so the first clause wins.
    assert_eq!(
        expect(&module, "oldcatch_match", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(&module, "oldcatch_match", &[make_small(2)], &mut ctx),
        atom_term(&module, "caught_exit")
    );
}

// ---------------------------------------------------------------------------
// Uncaught raises propagate as typed errors (never a panic)
// ---------------------------------------------------------------------------

#[test]
fn raw_functions_return_their_normal_values() {
    let module = module();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        expect(&module, "case_clause_raw", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(
            &module,
            "badmatch_raw",
            &[atom_term(&module, "ok")],
            &mut ctx
        ),
        atom_term(&module, "ok")
    );
    assert_eq!(
        expect(&module, "if_clause_raw", &[make_small(1)], &mut ctx),
        atom_term(&module, "positive")
    );
    assert_eq!(
        expect(&module, "try_clause_raw", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
}

#[test]
fn raw_raises_propagate_as_typed_errors_with_the_live_reason() {
    let module = module();
    let mut ctx = MinimalContext::new();

    // The tuple-reason raises: {tag, Value}, class error.
    for (name, arg, tag) in [
        ("case_clause_raw", make_small(2), "case_clause"),
        ("badmatch_raw", atom_term(&module, "no"), "badmatch"),
        ("try_clause_raw", make_small(2), "try_clause"),
    ] {
        let (result, machine) = run(&module, name, &[arg], &mut ctx);
        assert!(
            matches!(result, Err(BeamError::Raise(_))),
            "{name} must propagate a raise, got {result:?}"
        );
        let (class, reason) = machine
            .pending_exception()
            .expect("the live reason must survive on the machine");
        assert_eq!(class, ExceptionClass::Error);
        let expected = terms::make_tuple(&mut ctx, &[atom_term(&module, tag), arg]).unwrap();
        assert!(
            cmp(reason, expected, &module.atom_table).is_eq(),
            "{name} reason must be {{{tag},..}}"
        );
    }

    // The bare-atom raise: the atom `if_clause`.
    let (result, machine) = run(&module, "if_clause_raw", &[make_small(-1)], &mut ctx);
    assert!(matches!(result, Err(BeamError::Raise(_))));
    let (_, reason) = machine.pending_exception().expect("reason recorded");
    assert_eq!(reason, atom_term(&module, "if_clause"));
}

#[test]
fn an_uncaught_raise_carries_the_reason_term_on_the_propagated_error() {
    // The end-to-end (compiled-fixture) form of `throw_with_trace`'s live-reason
    // contract: the `{case_clause,2}` / `{badmatch,no}` term the machine records as
    // its pending exception also rides out of `run_export` in the propagated
    // `BeamError::Raise`'s `live_reason` slot — the same reason the system `erl`
    // prints for `exceptions:case_clause_raw(2)` / `exceptions:badmatch_raw(no)`
    // (`erl -noshell -eval 'try exceptions:case_clause_raw(2) of _ -> ok catch
    // C:R -> io:format("~p~n",[{C,R}]) end, halt().'` ⇒ `{error,{case_clause,2}}`).
    let module = module();
    let mut ctx = MinimalContext::new();

    for (name, arg, tag) in [
        ("case_clause_raw", make_small(2), "case_clause"),
        ("badmatch_raw", atom_term(&module, "no"), "badmatch"),
    ] {
        let (result, machine) = run(&module, name, &[arg], &mut ctx);
        let raise = match result {
            Err(BeamError::Raise(raise)) => raise,
            other => panic!("{name} must propagate a raise, got {other:?}"),
        };
        assert_eq!(raise.reason, ExitReason::Structured);
        assert_eq!(raise.reason.as_atom(), None);
        let live = raise
            .live_reason()
            .and_then(|live| live.downcast_ref::<Eterm>())
            .copied()
            .expect("the propagated raise carries the live reason term");
        // The propagated term is exactly the machine's pending-exception reason …
        assert_eq!(
            machine.pending_exception().map(|(_, reason)| reason),
            Some(live)
        );
        // … and it structurally equals the oracle's `{Tag,Value}`.
        let expected = terms::make_tuple(&mut ctx, &[atom_term(&module, tag), arg]).unwrap();
        assert!(
            cmp(live, expected, &module.atom_table).is_eq(),
            "{name}'s live reason must be {{{tag}, Value}}"
        );
    }
}

// ---------------------------------------------------------------------------
// A synthetic module: the documented M1 gap for a raising *body* BIF
// ---------------------------------------------------------------------------

/// Build a module whose `f/0` runs a raising BIF (a `gc_bif` with `NO_FAIL`)
/// **inside a live try**:
///
/// ```text
/// f() -> try erlang:boom() catch _ -> caught end.
/// ```
///
/// The compiler emits `gc_bif0 erlang:boom {f,0}` (no fail label), so a raise
/// must land in the try's handler.
fn synthetic_body_bif_module() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let f = module.atom_table.intern("f");
    let erlang = module.atom_table.intern("erlang");
    let boom = module.atom_table.intern("boom");
    let caught = module.atom_table.intern("caught");

    module.imports.push(Import {
        module: erlang,
        function: boom,
        arity: 0,
    });

    let code = vec![
        Instr::Label(Label(1)),
        Instr::Try {
            frame: Operand::X(9),
            fail: Label(3),
        },
        Instr::GcBif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 0,
            args: Vec::new(),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::Move {
            src: Operand::Atom(caught),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut labels = BTreeMap::new();
    for (index, instr) in code.iter().enumerate() {
        if let Instr::Label(label) = instr {
            labels.insert(*label, index);
        }
    }
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module.name,
        name: f,
        arity: 0,
        code,
        blocks: Vec::new(),
        labels,
    });
    module.exports.push(Export {
        function: f,
        arity: 0,
        label: Label(1),
    });
    module
}

#[test]
fn a_raising_bif_in_a_try_body_propagates_instead_of_landing_in_the_catch_frame() {
    // KNOWN M1 GAP (locked deliberately). The `Bif`/`GcBif` arms live in
    // `src/interpreter.rs` (owned elsewhere); on a raise with `NO_FAIL` they
    // `return Err(e)` straight out of the run, **without consulting the live catch
    // frame**. The oracle would land this exception at label 3 and return the atom
    // `caught`; the engine instead propagates the `Err` from `run_export`. This
    // test pins that behavior so a future fix (routing body-BIF raises into catch
    // frames) is an explicit, visible change rather than a silent one.
    let module = synthetic_body_bif_module();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(|m, f, a, _args| {
        if m == "erlang" && f == "boom" && a == 0 {
            Err(BeamError::BadArith)
        } else {
            Err(BeamError::BadArg)
        }
    }));
    let result = run(&module, "f", &[], &mut ctx).0;
    assert!(
        matches!(result, Err(BeamError::BadArith)),
        "M1: a body-BIF raise propagates as Err (the oracle would catch it)"
    );
}

// ---------------------------------------------------------------------------
// `build_stacktrace` / `raw_raise` over the committed `stacktrace.beam` fixture
// ---------------------------------------------------------------------------

/// The committed oracle fixture (produced by `erlc stacktrace.erl`).
const STACKTRACE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/stacktrace.beam"
);

fn stacktrace_module() -> Module {
    let mut module = load_file(STACKTRACE_BEAM).expect("load stacktrace.beam");
    // A real runtime keeps the well-known exception atoms — `file`/`line` included
    // (`../../erts/emulator/beam/beam_common.c:382-406`) — globally interned; the
    // bare loader interns only what the module's own atom chunk names. Seed them so
    // the fixture's cooked `S` can carry its source location — the same policy the
    // `tests/support` host applies to every module it loads (`SEED_ATOMS`).
    module.atom_table.intern("file");
    module.atom_table.intern("line");
    module
}

#[test]
fn build_stacktrace_fixture_cooks_the_raise_snapshot_from_the_nil_raw_trace() {
    // `cap/1` is `try case X of 1 -> one end catch C:R:S -> {C,R,S} end`, so its
    // handler moves the raw stacktrace (`x(2)`, `NIL` — the engine keeps no raw
    // `StackTrace` object) into `x(0)` and runs `build_stacktrace` — the only shape
    // `sys_core_fold` keeps a cooked trace for.
    //
    // ORACLE: `cap(2) = {error,{case_clause,2},[{stacktrace,cap,1,[{file,"stacktrace.erl"},{line,27}]} | <host frames>]}`.
    // The raise rebuilt the machine's pre-unwind snapshot
    // (`Machine::raise_snapshot`), and `build_stacktrace` cooks it even though the
    // raw trace is `NIL`, so `S` is the exception's own frames: the raising `cap/1`
    // first. The engine's list is the oracle's **prefix** — the trailing host
    // `init:*`/`erl_eval` frames live outside this run — and the class and the
    // reason are oracle-exact.
    let module = stacktrace_module();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        expect(&module, "cap", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );

    let actual = run(&module, "cap", &[make_small(2)], &mut ctx)
        .0
        .expect("cap(2)");
    let reason = terms::make_tuple(
        &mut ctx,
        &[atom_term(&module, "case_clause"), make_small(2)],
    )
    .unwrap();
    let location = location(&mut ctx, &module, "stacktrace.erl", 27);
    let item = frame(&mut ctx, &module, "cap", 1, location);
    let frames = terms::make_list(&mut ctx, &[item]).unwrap();
    let expected =
        terms::make_tuple(&mut ctx, &[atom_term(&module, "error"), reason, frames]).unwrap();
    assert!(
        cmp(actual, expected, &module.atom_table).is_eq(),
        "cap(2) must be \
         {{error,{{case_clause,2}},[{{stacktrace,cap,1,[{{file,\"stacktrace.erl\"}},{{line,27}}]}}]}} \
         — the raise-snapshot frames the engine derives"
    );
}

#[test]
fn raw_raise_fixture_reraises_with_the_caught_class() {
    // `reraise/1` is `try ... catch C:R:S -> erlang:raise(C, R, S) end`, which
    // `sys_core_fold` folds into `raw_raise` (a re-raised stacktrace never needs
    // cooking). ORACLE: `reraise(2)` raises `error {case_clause,2}`; `reraise(1)`
    // is the ordinary value.
    let module = stacktrace_module();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        expect(&module, "reraise", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );

    let (result, machine) = run(&module, "reraise", &[make_small(2)], &mut ctx);
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "reraise(2) must propagate a raise, got {result:?}"
    );
    let (class, reason) = machine
        .pending_exception()
        .expect("the live reason must survive on the machine");
    assert_eq!(class, ExceptionClass::Error);
    let expected = terms::make_tuple(
        &mut ctx,
        &[atom_term(&module, "case_clause"), make_small(2)],
    )
    .unwrap();
    assert!(
        cmp(reason, expected, &module.atom_table).is_eq(),
        "reraise(2) reason must be {{case_clause,2}}"
    );
}

#[test]
fn raw_raise_fixture_falls_through_to_badarg_for_an_unknown_class() {
    // `badclass/1` passes the *literal* atom `nonsense` as the class. C's
    // `raw_raise` has no such class, so it stores `am_badarg` in x(0) and lets the
    // next instruction run (no raise). ORACLE: `badclass(2) = badarg`.
    let module = stacktrace_module();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        expect(&module, "badclass", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(&module, "badclass", &[make_small(2)], &mut ctx),
        atom_term(&module, "badarg")
    );
}

#[test]
fn the_stacktrace_fixture_class_clause_still_matches() {
    // `classify/1` is the plain `error:`-classed form of `cap/1`; it is also what
    // interns the `error`/`case_clause` atoms the engine resolves from the module's
    // atom table (a bare `C:R:S` pattern never names them).
    let module = stacktrace_module();
    let mut ctx = MinimalContext::new();
    assert_eq!(
        expect(&module, "classify", &[make_small(1)], &mut ctx),
        atom_term(&module, "one")
    );
    assert_eq!(
        expect(&module, "classify", &[make_small(2)], &mut ctx),
        atom_term(&module, "caught_case_clause")
    );
}

// ---------------------------------------------------------------------------
// The engine-derived symbolic stacktrace, end to end through the interpreter
// ---------------------------------------------------------------------------

/// A synthetic module whose `g/0` calls `f/0`, and whose `f/0` raises inside a
/// `try`, catches it and then runs `build_stacktrace` over a non-NIL raw
/// stacktrace:
///
/// ```text
/// g() -> f().
/// f() -> try error({badmatch, 1}) catch _:_ -> build_stacktrace(X) end.  % X: raw trace
/// ```
///
/// The raise is what populates the machine's **raise-time snapshot** (the
/// pre-unwind frame chain): the `badmatch` records the raising `(func, pc)` before
/// the catch lands, and the handler's `build_stacktrace` then cooks that snapshot.
/// An **empty** snapshot (no raise since the last `reset`) cooks to the empty list,
/// so this fixture is what drives the snapshot through the *dispatch loop* end to
/// end. The compiler never emits this shape (it always moves the caught `x(2)` —
/// NIL at M1 — into `x(0)`), so a synthetic module is required.
fn synthetic_stacktrace_module() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let f = module.atom_table.intern("f");
    let g = module.atom_table.intern("g");
    // The raise needs both the reason tag (`badmatch`, raised as `{badmatch,1}`)
    // and the class (`error`) nameable from the module's own atom table.
    module.atom_table.intern("badmatch");
    module.atom_table.intern("error");

    // f/0: a `try` around a `badmatch`-raising instruction (so the machine captures
    // its pre-unwind snapshot), then the handler loads a non-NIL raw stacktrace into
    // x(0) and cooks it.
    let f_code = vec![
        Instr::Label(Label(10)),
        Instr::Try {
            frame: Operand::X(9),
            fail: Label(20),
        },
        Instr::Badmatch {
            value: Operand::Integer(1),
        },
        Instr::Return,
        Instr::Label(Label(20)),
        Instr::TryCase {
            frame: Operand::X(9),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::BuildStacktrace,
        Instr::Return,
    ];
    // g/0: `allocate 0` (a non-tail call needs a frame), `call f/0`, return.
    let g_code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 0,
        },
        Instr::Call {
            arity: 0,
            target: Label(10),
        },
        Instr::Deallocate(0),
        Instr::Return,
    ];

    for (name, arity, code) in [(f, 0u32, f_code), (g, 0u32, g_code)] {
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
            module: module.name,
            name,
            arity,
            code,
            blocks: Vec::new(),
            labels,
        });
        module.exports.push(Export {
            function: name,
            arity,
            label: entry,
        });
    }
    module
}

#[test]
fn build_stacktrace_cooks_the_raise_snapshot_innermost_first() {
    // The frames come from the machine's **raise-time snapshot**
    // (`Machine::raise_snapshot`), the chain `f/0`'s `badmatch` captured before the
    // catch landed: the raising `f/0` first, then its caller `g/0`, innermost-first.
    // An empty snapshot cooks to the empty list, so the two non-empty items below
    // can only come from the snapshot the raise populated. No `line` marker is in
    // effect for either function, so each location is `[]`.
    let module = synthetic_stacktrace_module();
    let mut ctx = MinimalContext::new();
    let actual = run(&module, "g", &[], &mut ctx)
        .0
        .expect("g() runs to completion");

    let m = atom_term(&module, "m");
    let item_f = terms::make_tuple(&mut ctx, &[m, atom_term(&module, "f"), make_small(0), NIL])
        .unwrap();
    let item_g = terms::make_tuple(&mut ctx, &[m, atom_term(&module, "g"), make_small(0), NIL])
        .unwrap();
    let expected = terms::make_list(&mut ctx, &[item_f, item_g]).unwrap();
    assert!(
        cmp(actual, expected, &module.atom_table).is_eq(),
        "the cooked trace must be [{{m,f,0,[]}},{{m,g,0,[]}}] — the pre-unwind raise \
         snapshot innermost-first"
    );
}

// ---------------------------------------------------------------------------
// The whole exception path on a real module that names NONE of the reason atoms
// ---------------------------------------------------------------------------
//
// The real OTP compiler does **not** emit the runtime's well-known exception
// atoms (`function_clause`/`case_clause`/`if_clause`/`try_clause`/`badmatch`/
// `error`/`'EXIT'`/`file`/`line`/…) into a module's atom chunk: a real runtime
// keeps them globally interned (`../../erts/emulator/beam/beam_common.c:382-406`,
// `error_atom[NUMBER_EXIT_CODES]`). The engine resolves a reason atom **by name**
// from the running module's atom table and refuses to substitute a *different*
// atom, so on such a module every raise used to degrade to a typed
// `unsupported`. `tests/support/mod.rs` now seeds the well-known atoms into every
// module it loads — the host's analogue of the runtime's global interning, and
// the test-side counterpart of `beam_rt::boot::CORE_ATOMS` (which this node does
// not own).
//
// `tests/fixtures/excpaths.beam` (compiled with the system `erlc` from
// `excpaths.erl`) is exactly such a module: its own atom chunk contains none of
// those names (proved below with `beam_loader::load_file`). Every expected value
// is the oracle's, with the exact command quoted at the assertion.
//
// The fixture makes no external calls, so the host needs no stdlib `ebin` (the
// `"."` primary path is never consulted) and nothing here needs the OTP install
// at test time — the `.beam` is committed.

/// The committed fixture whose atom chunk carries none of the well-known atoms.
const EXCPATHS_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/excpaths.beam"
);

/// A [`ModuleHost`] with `excpaths` registered (the shared-atom host seeds the
/// well-known atoms on load).
fn excpaths_host() -> ModuleHost {
    let mut host = ModuleHost::new(".");
    host.register("excpaths", EXCPATHS_BEAM);
    host
}

/// `reason` for the `{Class, Reason}` tuple `catch_*(X)` returns.
fn caught_reason(got: Eterm) -> Eterm {
    terms::tuple_element(got, 1).expect("a {Class, Reason} 2-tuple")
}

#[test]
fn the_fixture_carries_none_of_the_reason_atoms_and_the_host_seeds_them() {
    // Loaded on its own, the module cannot name its own exception reasons: the
    // compiler never emitted them (a real runtime has them globally interned).
    let bare = load_file(EXCPATHS_BEAM).expect("load excpaths.beam on its own");
    for name in SEED_ATOMS {
        assert!(
            bare.atom_table.lookup(name).is_none(),
            "the fixture must NOT carry the well-known atom `{name}` (it would defeat the test)"
        );
    }

    // Loaded through the shared-atom host, every well-known atom is resolvable
    // from the module.
    let mut host = excpaths_host();
    let module = host.load("excpaths").expect("load excpaths through the host");
    for name in SEED_ATOMS {
        assert!(
            module.atom_table.lookup(name).is_some(),
            "the host must seed the well-known atom `{name}` into the module"
        );
    }

    // The seeding preserves the index-consistency invariant the host documents:
    // for every index in the module's table, the shared table names the same atom
    // (which is what lets an atom term built by one module compare equal to the
    // same-named atom interned elsewhere).
    let shared = host.atom_table();
    for index in 0..module.atom_table.len() {
        let atom = Atom::from_raw(index as u32);
        assert_eq!(
            module.atom_table.name(atom),
            shared.name(atom),
            "index {index} must agree between the module and the shared atom table"
        );
    }
}

#[test]
fn without_the_seed_a_real_module_cannot_name_its_own_exception_reason() {
    // The gap the seeding closes, pinned on the SAME module: without a global
    // atom universe the engine refuses to start the raise at all — a typed
    // `unsupported` rather than a substituted reason, and `pending_exception`
    // never fills in (the raise never began).
    let mut module = load_file(EXCPATHS_BEAM).expect("load excpaths.beam on its own");
    let b = make_atom(module.atom_table.intern("b"));
    let mut ctx = MinimalContext::new();
    let (result, machine) = run(&module, "cc", &[b], &mut ctx);
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "an unnameable reason must be a typed error, got {result:?}"
    );
    assert!(
        machine.pending_exception().is_none(),
        "the raise must not have started: the module cannot name `case_clause`"
    );
}

#[test]
fn a_failed_head_clause_raises_function_clause_on_a_real_module() {
    // `fc(a) -> 1.` reached through its `func_info` label with `b`:
    //   $ erl -noshell -eval 'try excpaths:fc(b) catch C:R:_ -> io:format("~p:~p~n",[C,R]) end, halt().'
    //   error:function_clause
    let mut host = excpaths_host();
    let b = host.intern("b");

    // Uncaught: a typed raise propagating out of the run (never a wrong value).
    assert!(
        matches!(host.call("excpaths", "fc", &[b]), Err(BeamError::Raise(_))),
        "an uncaught `function_clause` must propagate as a typed raise"
    );

    // Caught in-module: the class atom `error` and the *bare* `function_clause`
    // reason (the call arguments ride in the stacktrace, not the reason).
    let got = host
        .call("excpaths", "catch_fc", &[b])
        .expect("catch_fc(b) must run");
    assert_atom(&host, terms::tuple_element(got, 0).unwrap(), "error");
    assert_atom(&host, caught_reason(got), "function_clause");
}

#[test]
fn a_failed_case_raises_case_clause_on_a_real_module() {
    // `cc(X) -> case X of a -> 1 end.` with `b`:
    //   $ erl -noshell -eval 'try excpaths:cc(b) catch C:R:_ -> io:format("~p:~p~n",[C,R]) end, halt().'
    //   error:{case_clause,b}
    let mut host = excpaths_host();
    let b = host.intern("b");

    assert!(matches!(
        host.call("excpaths", "cc", &[b]),
        Err(BeamError::Raise(_))
    ));

    let got = host
        .call("excpaths", "catch_cc", &[b])
        .expect("catch_cc(b) must run");
    assert_atom(&host, terms::tuple_element(got, 0).unwrap(), "error");
    let reason = caught_reason(got);
    assert_atom(&host, terms::tuple_element(reason, 0).unwrap(), "case_clause");
    assert!(
        cmp(
            terms::tuple_element(reason, 1).unwrap(),
            b,
            host.atom_table()
        )
        .is_eq(),
        "the reason's payload is the failing value `b`"
    );
}

#[test]
fn a_failed_if_raises_the_bare_atom_if_clause_on_a_real_module() {
    // `ii(X) -> if X > 0 -> 1 end.` with `-1`:
    //   $ erl -noshell -eval 'try excpaths:ii(-1) catch C:R:_ -> io:format("~p:~p~n",[C,R]) end, halt().'
    //   error:if_clause
    let mut host = excpaths_host();

    assert!(matches!(
        host.call("excpaths", "ii", &[make_small(-1)]),
        Err(BeamError::Raise(_))
    ));

    let got = host
        .call("excpaths", "catch_ii", &[make_small(-1)])
        .expect("catch_ii(-1) must run");
    assert_atom(&host, terms::tuple_element(got, 0).unwrap(), "error");
    assert_atom(&host, caught_reason(got), "if_clause");
}

#[test]
fn a_failed_binary_match_raises_badmatch_on_a_real_module() {
    // `bm(X) -> <<Y:16>> = X, Y.` with the atom `a` (a non-binary):
    //   $ erl -noshell -eval 'try excpaths:bm(a) catch C:R:_ -> io:format("~p:~p~n",[C,R]) end, halt().'
    //   error:{badmatch,a}
    let mut host = excpaths_host();
    let a = host.intern("a");

    assert!(matches!(
        host.call("excpaths", "bm", &[a]),
        Err(BeamError::Raise(_))
    ));

    let got = host
        .call("excpaths", "catch_bm", &[a])
        .expect("catch_bm(a) must run");
    assert_atom(&host, terms::tuple_element(got, 0).unwrap(), "error");
    let reason = caught_reason(got);
    assert_atom(&host, terms::tuple_element(reason, 0).unwrap(), "badmatch");
    assert!(
        cmp(terms::tuple_element(reason, 1).unwrap(), a, host.atom_table()).is_eq(),
        "the reason's payload is the failing value `a`"
    );
}

#[test]
fn a_failed_try_of_clause_raises_try_clause_on_a_real_module() {
    // `tc(X) -> try X of a -> 1 catch _:_ -> nope end.` with `b` — the `of`
    // clause failure is NOT caught by the same `try`'s catch part:
    //   $ erl -noshell -eval 'try excpaths:tc(b) catch C:R:_ -> io:format("~p:~p~n",[C,R]) end, halt().'
    //   error:{try_clause,b}
    let mut host = excpaths_host();
    let b = host.intern("b");

    assert!(matches!(
        host.call("excpaths", "tc", &[b]),
        Err(BeamError::Raise(_))
    ));

    let got = host
        .call("excpaths", "catch_tc", &[b])
        .expect("catch_tc(b) must run");
    assert_atom(&host, terms::tuple_element(got, 0).unwrap(), "error");
    let reason = caught_reason(got);
    assert_atom(&host, terms::tuple_element(reason, 0).unwrap(), "try_clause");
    assert!(
        cmp(terms::tuple_element(reason, 1).unwrap(), b, host.atom_table()).is_eq(),
        "the reason's payload is the failing value `b`"
    );
}

#[test]
fn the_caught_class_atom_is_the_error_class_on_a_real_module() {
    // `catch_class(X) -> try cc(X) catch C:_ -> C end.` with `b`:
    //   $ erl -noshell -eval 'io:format("~p~n",[excpaths:catch_class(b)]), halt().'
    //   error
    let mut host = excpaths_host();
    let b = host.intern("b");
    let got = host
        .call("excpaths", "catch_class", &[b])
        .expect("catch_class(b) must run");
    assert_atom(&host, got, "error");
}

#[test]
fn old_style_catch_builds_the_exit_tuple_without_a_stacktrace() {
    // `oldcatch_bm(X) -> catch bm(X).` — a class-`error` raise (the `bm/1` binary
    // match fails), so the caught term is C's `{'EXIT',{Reason,Stacktrace}}` pair
    // (`add_stacktrace`), not the 2-element `{'EXIT',Reason}`:
    //   $ erl -noshell -pz <fixtures> -eval 'io:format("~p~n",[excpaths:oldcatch_bm(a)]), halt().'
    //   {'EXIT',{{badmatch,a},
    //            [{excpaths,bm,1,[{file,"excpaths.erl"},{line,54}]},
    //             {excpaths,oldcatch_bm,1,[{file,"excpaths.erl"},{line,68}]}
    //             | <host erl_eval/init frames>]}}
    // The raise rebuilt the machine's pre-unwind frame chain and `catch_end` cooks
    // it, so the engine's stacktrace is the oracle's leading frames — the raising
    // `bm/1` first, then its live caller `oldcatch_bm/1` — and the REASON is
    // oracle-exact; the trailing host frames live outside this run.
    let mut host = excpaths_host();
    let a = host.intern("a");
    let got = host
        .call("excpaths", "oldcatch_bm", &[a])
        .expect("oldcatch_bm(a) must yield the EXIT tuple");
    let module = host.load("excpaths").expect("excpaths is registered");

    let badmatch = host.intern("badmatch");
    let exit = host.intern("EXIT");
    let reason = terms::make_tuple(&mut host, &[badmatch, a]).unwrap();
    let bm_location = location(&mut host, &module, "excpaths.erl", 54);
    let bm_item = frame(&mut host, &module, "bm", 1, bm_location);
    let caller_location = location(&mut host, &module, "excpaths.erl", 68);
    let caller_item = frame(&mut host, &module, "oldcatch_bm", 1, caller_location);
    let frames = terms::make_list(&mut host, &[bm_item, caller_item]).unwrap();
    let pair = terms::make_tuple(&mut host, &[reason, frames]).unwrap();
    let expected = terms::make_tuple(&mut host, &[exit, pair]).unwrap();
    assert!(
        cmp(got, expected, host.atom_table()).is_eq(),
        "oldcatch_bm(a) must be 'EXIT' paired with {{Reason,Stacktrace}}, Reason = \
         {{badmatch,a}} and Stacktrace = the oracle's leading frames: the raising \
         excpaths:bm/1 at excpaths.erl:54, then its caller excpaths:oldcatch_bm/1 at \
         excpaths.erl:68"
    );
}

#[test]
fn the_try_catch_class_reason_stacktrace_shape_cooks_the_raise_snapshot() {
    // `trace(X) -> try cc(X) catch C:R:S -> {C, R, S} end.`
    // ORACLE: `excpaths:trace(b) =
    //   {error,{case_clause,b},
    //    [{excpaths,cc,1,[{file,"excpaths.erl"},{line,52}]},
    //     {excpaths,trace,1,[{file,"excpaths.erl"},{line,71}]} | <host frames>]}`.
    // The engine lands no raw stacktrace (`x(2) = NIL`), but it cooks its
    // raise-snapshot frames, so `S` is exactly the oracle's leading items: the
    // raising `cc/1` first, then its live caller `trace/1`, innermost-first. The
    // trailing host frames live outside this run; the class and the reason are
    // oracle-exact.
    let mut host = excpaths_host();
    let b = host.intern("b");
    let got = host
        .call("excpaths", "trace", &[b])
        .expect("trace(b) must run");

    assert_atom(&host, terms::tuple_element(got, 0).unwrap(), "error");
    let reason = terms::tuple_element(got, 1).unwrap();
    assert_atom(&host, terms::tuple_element(reason, 0).unwrap(), "case_clause");
    assert!(
        cmp(terms::tuple_element(reason, 1).unwrap(), b, host.atom_table()).is_eq(),
        "the reason's payload is the failing value `b`"
    );

    let module = host.load("excpaths").expect("excpaths is registered");
    let cc_location = location(&mut host, &module, "excpaths.erl", 52);
    let cc_item = frame(&mut host, &module, "cc", 1, cc_location);
    let trace_location = location(&mut host, &module, "excpaths.erl", 71);
    let trace_item = frame(&mut host, &module, "trace", 1, trace_location);
    let expected = terms::make_list(&mut host, &[cc_item, trace_item]).unwrap();
    let cooked = terms::tuple_element(got, 2).expect("the S element");
    assert!(
        cmp(cooked, expected, host.atom_table()).is_eq(),
        "the cooked `S` must be the raise-snapshot frames \
         [{{excpaths,cc,1,[{{file,\"excpaths.erl\"}},{{line,52}}]}},\
         {{excpaths,trace,1,[{{file,\"excpaths.erl\"}},{{line,71}}]}}]"
    );
}

// ---------------------------------------------------------------------------
// The caught-`S` stacktrace: `try erlang:error(badarg) catch C:R:S -> S` over
// the committed `catchtrace.beam` fixture
// ---------------------------------------------------------------------------

/// The committed oracle fixture (produced by `erlc catchtrace.erl`).
const CATCHTRACE_BEAM: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/fixtures/catchtrace.beam"
);

/// A [`ModuleHost`] with `catchtrace` registered (the shared-atom host seeds the
/// well-known atoms — `file`/`line` included — on load).
fn catchtrace_host() -> ModuleHost {
    let mut host = ModuleHost::new(".");
    host.register("catchtrace", CATCHTRACE_BEAM);
    host
}

/// The name of the atom `term`, resolved through the host's shared atom table.
fn atom_name_at(host: &ModuleHost, term: Eterm) -> String {
    assert!(
        is_atom(term),
        "a stacktrace field must be an atom, got {term:?}"
    );
    host.atom_table()
        .name(atom_val(term))
        .unwrap_or_else(|| panic!("the shared atom table does not name {term:?}"))
        .to_string()
}

/// Reads an Erlang string (a proper list of code points) into a Rust `String`.
fn erl_string(term: Eterm) -> String {
    let codes: Vec<u32> = terms::list_elements(term)
        .expect("an Erlang string is a proper list")
        .into_iter()
        .map(|code| {
            assert!(
                is_small(code),
                "a string code point must be a small integer, got {code:?}"
            );
            signed_value(code) as u32
        })
        .collect();
    codes
        .iter()
        .map(|&code| char::from_u32(code).expect("a string code point is a valid char"))
        .collect()
}

/// The `(Module, Function, Arity, File, Line)` view of one cooked stacktrace item.
///
/// The location is the oracle's `[{file,Name},{line,N}]`; a frame with no location
/// (the oracle's `{init,start_it,1,[]}`) yields an empty file and line `0`.
fn frame_fields(host: &ModuleHost, item: Eterm) -> (String, String, i64, String, i64) {
    assert_eq!(
        terms::tuple_arity(item).ok(),
        Some(4),
        "a cooked stacktrace item is the 4-tuple {{M,F,A,Location}}: {item:?}"
    );
    let module = atom_name_at(host, terms::tuple_element(item, 0).expect("module"));
    let function = atom_name_at(host, terms::tuple_element(item, 1).expect("function"));
    let arity_term = terms::tuple_element(item, 2).expect("arity");
    assert!(
        is_small(arity_term),
        "a frame arity must be a small integer, got {arity_term:?}"
    );
    let arity = signed_value(arity_term);
    let location = terms::tuple_element(item, 3).expect("location");
    if is_nil(location) {
        return (module, function, arity, String::new(), 0);
    }
    let pairs = terms::list_elements(location).expect("the location is a proper list");
    assert_eq!(
        pairs.len(),
        2,
        "the location must be [{{file,Name}},{{line,N}}], got {pairs:?}"
    );
    let file_pair = pairs[0];
    let line_pair = pairs[1];
    assert_eq!(
        atom_name_at(host, terms::tuple_element(file_pair, 0).expect("the file key")),
        "file",
        "the location's first pair must be {{file,Name}}"
    );
    let file = erl_string(terms::tuple_element(file_pair, 1).expect("the file name"));
    assert_eq!(
        atom_name_at(host, terms::tuple_element(line_pair, 0).expect("the line key")),
        "line",
        "the location's second pair must be {{line,N}}"
    );
    let line_term = terms::tuple_element(line_pair, 1).expect("the line number");
    assert!(
        is_small(line_term),
        "a frame line must be a small integer, got {line_term:?}"
    );
    (module, function, arity, file, signed_value(line_term))
}

/// Renders cooked frames in the flat `Module|Function|Arity|File|Line` form the
/// oracle helper below prints (frames joined by `;`), so the engine's list and the
/// oracle's can be compared as strings.
fn render_frames(frames: &[(String, String, i64, String, i64)]) -> String {
    frames
        .iter()
        .map(|(module, function, arity, file, line)| {
            format!("{module}|{function}|{arity}|{file}|{line}")
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// The `catchtrace`-module frames the system `erl` cooks for `run/0` and `run2/0`,
/// rendered in [`render_frames`]'s flat form; `None` when no runnable `erl` is on
/// `PATH` (the caller skips loudly). A spawned `erl` that exits non-zero is a
/// **harness** failure, not "no oracle" — it panics with the captured stderr so a
/// broken oracle can never masquerade as a silent skip.
fn oracle_catchtrace_frames() -> Option<(String, String)> {
    let eval = concat!(
        "Pick = fun(L) -> [X || {catchtrace,_,_,_} = X <- L] end, ",
        "Show = fun(L) -> lists:flatten(lists:join(\";\", ",
        "[io_lib:format(\"~s|~s|~w|~s|~w\", [M, atom_to_list(FN), A, ",
        "proplists:get_value(file, Loc), proplists:get_value(line, Loc)]) ",
        "|| {M,FN,A,Loc} <- Pick(L)])) end, ",
        "io:format(\"~s~n~s~n\", [Show(catchtrace:run()), Show(catchtrace:run2())]), halt()."
    );
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let output = Command::new("erl")
        .args(["-noshell", "-pz", fixtures, "-eval", eval])
        .output()
        .ok()?;
    assert!(
        output.status.success(),
        "the system `erl` exited {} for `{eval}`\n  stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let run = lines.next().unwrap_or_default().to_string();
    let run2 = lines.next().unwrap_or_default().to_string();
    Some((run, run2))
}

#[test]
fn the_caught_stacktrace_names_the_raising_function_and_its_live_callers() {
    // The fixture's two caught-`S` shapes:
    //   run()  = `try erlang:error(badarg) catch _C:_R:S -> S end`
    //   run2() = `try raise_badarg() catch _C:_R:S -> S end`
    // with `raise_badarg() -> erlang:error(badarg).`
    //
    // ORACLE (`erl -noshell -pz <fixtures> -eval 'catchtrace:run()'`):
    //   run()  = [{catchtrace,run,0,[{file,"catchtrace.erl"},{line,20}]} | <host frames>]
    //   run2() = [{catchtrace,raise_badarg,0,[{file,"catchtrace.erl"},{line,26}]},
    //             {catchtrace,run2,0,[{file,"catchtrace.erl"},{line,23}]} | <host frames>]
    // The engine keeps no raw `StackTrace` object, but the raise rebuilt the machine's
    // pre-unwind frame chain (`Machine::raise_snapshot`), and the handler's
    // `build_stacktrace` cooks it — so the caught `S` is the oracle's own **leading
    // frames**, never `[]`: the raising function first, then its live callers
    // innermost-first. The engine's list is a strictly-shorter prefix of the oracle's
    // (the trailing host `init:*` frames live outside this run).
    let mut host = catchtrace_host();

    let run = host
        .call("catchtrace", "run", &[])
        .expect("run/0 must return its caught S");
    let run_frames: Vec<_> = terms::list_elements(run)
        .expect("S is a list")
        .iter()
        .map(|&item| frame_fields(&host, item))
        .collect();
    assert_eq!(
        run_frames,
        vec![(
            "catchtrace".to_string(),
            "run".to_string(),
            0,
            "catchtrace.erl".to_string(),
            20
        )],
        "run/0's caught S must name the raising function `catchtrace:run/0` first \
         (the raising call is inline in the try body)"
    );

    let run2 = host
        .call("catchtrace", "run2", &[])
        .expect("run2/0 must return its caught S");
    let run2_frames: Vec<_> = terms::list_elements(run2)
        .expect("S is a list")
        .iter()
        .map(|&item| frame_fields(&host, item))
        .collect();
    assert_eq!(
        run2_frames,
        vec![
            (
                "catchtrace".to_string(),
                "raise_badarg".to_string(),
                0,
                "catchtrace.erl".to_string(),
                26
            ),
            (
                "catchtrace".to_string(),
                "run2".to_string(),
                0,
                "catchtrace.erl".to_string(),
                23
            ),
        ],
        "run2/0's caught S must name `raise_badarg/0` first, then its live caller `run2/0`"
    );

    // Oracle cross-check: re-derive the same frames from the system `erl` (skip
    // loudly when it is absent). The oracle filters its own `S` to the `catchtrace`
    // frames and prints them flat, so a drift in any field — module, function,
    // arity, file or line — fails here too.
    let Some((oracle_run, oracle_run2)) = oracle_catchtrace_frames() else {
        eprintln!(
            "SKIP exceptions::the_caught_stacktrace_names_the_raising_function_and_its_live_callers \
             oracle half: no `erl` oracle on PATH"
        );
        return;
    };
    assert_eq!(
        render_frames(&run_frames),
        oracle_run,
        "run/0's cooked S must equal the oracle's leading frames"
    );
    assert_eq!(
        render_frames(&run2_frames),
        oracle_run2,
        "run2/0's cooked S must equal the oracle's leading frames"
    );
}
