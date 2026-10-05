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
    load_file, Export, Function, Import, ImportRef, Instr, Label, Module, Operand, NO_FAIL,
};
use beam_term::{cmp, is_small, make_atom, make_small, signed_value, Atom, Eterm};

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
    let result =
        machine.run_export(module, atom(module, name), args.len() as u32, args, ctx);
    (result, machine)
}

/// Run `name/arity` and expect the oracle's value (the context stays alive for
/// the caller, so a boxed result remains valid).
fn expect(
    module: &Module,
    name: &str,
    args: &[Eterm],
    ctx: &mut dyn ExecutionContext,
) -> Eterm {
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
    assert_eq!(result.expect("guard_badarith(1)"), atom_term(&module, "caught_case_clause"));
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
    let inner =
        terms::make_tuple(&mut ctx, &[atom_term(&module, "case_clause"), make_small(2)]).unwrap();
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
        expect(&module, "badmatch_raw", &[atom_term(&module, "ok")], &mut ctx),
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
