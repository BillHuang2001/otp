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

use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::{terms, ExecutionContext, Machine, MinimalContext};
use beam_loader::{
    load_file, Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{cmp, is_small, make_atom, make_small, signed_value, Atom, Eterm, NIL};

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

    // The raw caught value escapes to the caller. Oracle:
    // `{'EXIT',{{case_clause,2},Stacktrace}}`. M1 captures no stacktrace, so the
    // engine's reason is the shorter `{'EXIT',{case_clause,2}}` — the documented
    // `catch_end` limit (C's `add_stacktrace` is skipped).
    let actual = run(&module, "oldcatch", &[make_small(2)], &mut ctx)
        .0
        .expect("oldcatch(2) yields the EXIT tuple");
    let inner = terms::make_tuple(
        &mut ctx,
        &[atom_term(&module, "case_clause"), make_small(2)],
    )
    .unwrap();
    let expected = terms::make_tuple(&mut ctx, &[atom_term(&module, "EXIT"), inner]).unwrap();
    assert!(
        cmp(actual, expected, &module.atom_table).is_eq(),
        "oldcatch(2) must be {{'EXIT',{{case_clause,2}}}} (M1: no stacktrace)"
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
    load_file(STACKTRACE_BEAM).expect("load stacktrace.beam")
}

#[test]
fn build_stacktrace_fixture_cooks_a_nil_raw_trace_to_nil() {
    // `cap/1` is `try case X of 1 -> one end catch C:R:S -> {C,R,S} end`, so its
    // handler moves the raw stacktrace (`x(2)`) into `x(0)` and runs
    // `build_stacktrace` — the only shape `sys_core_fold` keeps a cooked trace for.
    //
    // ORACLE: `cap(2) = {error,{case_clause,2},[{stacktrace,cap,1,[{file,"stacktrace.erl"},{line,27}]} | <host frames>]}`.
    // M1 captures no stacktrace, so a landed exception has `x(2) = NIL`, and C's
    // `build_stacktrace` short-circuits a NIL raw trace to NIL
    // (`get_trace_from_exc(NIL) == NULL`, `beam_common.c:1219-1223`). The engine
    // therefore returns the oracle's class and reason with an EMPTY cooked trace —
    // the documented "no stacktrace capture" gap, never a wrong reason.
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
    let expected =
        terms::make_tuple(&mut ctx, &[atom_term(&module, "error"), reason, NIL]).unwrap();
    assert!(
        cmp(actual, expected, &module.atom_table).is_eq(),
        "cap(2) must be {{error,{{case_clause,2}},[]}} (M1: no stacktrace capture)"
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

/// A synthetic module whose `g/0` calls `f/0`, and whose `f/0` runs
/// `build_stacktrace` over a non-NIL raw stacktrace:
///
/// ```text
/// g() -> f().
/// f() -> X = 1, build_stacktrace(X).
/// ```
///
/// The compiler never emits this (it always moves the caught `x(2)` — NIL at M1 —
/// into `x(0)`), so a synthetic module is the only way to drive the engine's
/// live-frame derivation through the *dispatch loop*; it also pins that the loop
/// hands the family the right current `(func, pc)`.
fn synthetic_stacktrace_module() -> Module {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let f = module.atom_table.intern("f");
    let g = module.atom_table.intern("g");

    // f/0: load a (stand-in) raw stacktrace into x(0), then cook it.
    let f_code = vec![
        Instr::Label(Label(10)),
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
fn build_stacktrace_derives_the_live_call_chain_innermost_first() {
    // C walks the *saved* raw stacktrace; M1 keeps none, so the engine substitutes
    // the live call frames: the function executing right now, then every frame
    // suspended in a call, innermost first (`Machine::resumption_addresses`).
    // No `line` marker is in effect for `f/0`'s code, so the location is `[]`.
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
        "the cooked trace must be [{{m,f,0,[]}},{{m,g,0,[]}}]"
    );
}
