//! Synthetic-module opcode tests.
//!
//! The vertical slice (`tests/slice.rs`) exercises only the handful of opcodes the
//! fixture uses. These tests hand-build [`Module`]s (the loader is not involved) to
//! cover the rest of the Milestone-1 subset: every [`TestKind`] (success and
//! failure), `SelectVal`, the frame instructions (`Allocate`/`AllocateHeap`/
//! `TestHeap`/`Deallocate`/`Trim`) and the call family (`Call`/`CallLast`/
//! `CallOnly`/`CallExt`/`CallExtOnly`/`CallExtLast`), the `GcBif` guard-fail/raise
//! paths, the catch-frame landing of a raising BIF (`gc_bif`/`bif`/`bif0`/
//! `call_ext*` with no fail label inside a live `try`) and the documented error
//! paths. Every error is a typed [`beam_common::BeamError`] — no test panics.

use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::{Machine, MinimalContext};
use beam_loader::{
    Export, Function, HeapNeed, Import, ImportRef, Instr, Label, Module, Opcode, Operand, TestKind,
    NO_FAIL,
};
use beam_term::{
    is_small, make_arityval, make_boxed, make_header, make_internal_pid, make_internal_port,
    make_pid_data, make_port_data, make_small, signed_value, Atom, Eterm, HEADER_FLONUM,
    HEAP_BITS_SUBTAG, MAP_SUBTAG, NIL, REF_SUBTAG,
};

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

/// A builder for synthetic modules with an owned atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
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

/// Run `entry` with no BIF support.
fn run(module: &Module, entry: Label, args: &[Eterm]) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    machine.run_label(module, entry, args.len() as u32, args, &mut ctx)
}

/// A hook implementing `erlang:'+'/2` and `erlang:'-'/2` over small integers.
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

/// Run `entry` with [`arith_hook`] as the BIF implementation.
fn run_with_hook(module: &Module, entry: Label, args: &[Eterm]) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    machine.run_label(module, entry, args.len() as u32, args, &mut ctx)
}

/// Run `entry` and also return the machine (to inspect frame/stack state).
fn run_inspected(
    module: &Module,
    entry: Label,
    args: &[Eterm],
) -> (Result<Eterm, BeamError>, Machine) {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    let result = machine.run_label(module, entry, args.len() as u32, args, &mut ctx);
    (result, machine)
}

// ---------------------------------------------------------------------------
// Move / Swap / metadata
// ---------------------------------------------------------------------------

#[test]
fn move_writes_registers() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(9),
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(make_small(9)));
}

#[test]
fn swap_exchanges_registers() {
    // After the swap x0 holds the old x1 (2).
    let swapped_x0 = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(2),
            dst: Operand::X(1),
        },
        Instr::Swap {
            a: Operand::X(0),
            b: Operand::X(1),
        },
        Instr::Return,
    ];
    // After the swap x1 holds the old x0 (1); move it into x0 to observe it.
    // (Distinct labels: labels are module-unique.)
    let swapped_x1 = vec![
        Instr::Label(Label(11)),
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(2),
            dst: Operand::X(1),
        },
        Instr::Swap {
            a: Operand::X(0),
            b: Operand::X(1),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry_a = fixture.add_function("a", 0, swapped_x0);
    let entry_b = fixture.add_function("b", 0, swapped_x1);
    let module = fixture.finish();
    assert_eq!(
        run(&module, entry_a, &[]).map_err(|_| ()),
        Ok(make_small(2))
    );
    assert_eq!(
        run(&module, entry_b, &[]).map_err(|_| ()),
        Ok(make_small(1))
    );
}

#[test]
fn metadata_instructions_are_no_ops() {
    let mut fixture = Fixture::new();
    let module_atom = fixture.atom("m");
    let function_atom = fixture.atom("f");
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Line(0),
        Instr::FuncInfo {
            module: module_atom,
            function: function_atom,
            arity: 0,
        },
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(42),
            dst: Operand::X(0),
        },
        Instr::IntCodeEnd,
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(make_small(42)));
}

// ---------------------------------------------------------------------------
// Test opcodes — every TestKind, success and failure
// ---------------------------------------------------------------------------

/// Run an `is_*` test with register arguments; `x(0)` in the result is `1` on
/// success and `0` on failure.
fn run_istest(kind: TestKind, args: &[Eterm]) -> Result<Eterm, BeamError> {
    let operands: Vec<Operand> = (0..args.len() as u32).map(Operand::X).collect();
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Test {
            kind,
            fail: Label(2),
            args: operands,
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("t", args.len() as u32, code);
    let module = fixture.finish();
    run(&module, entry, args)
}

fn ok_true(kind: TestKind, args: &[Eterm]) {
    assert_eq!(
        run_istest(kind, args).map_err(|_| ()),
        Ok(make_small(1)),
        "{kind:?}"
    );
}

fn ok_false(kind: TestKind, args: &[Eterm]) {
    assert_eq!(
        run_istest(kind, args).map_err(|_| ()),
        Ok(make_small(0)),
        "{kind:?}"
    );
}

#[test]
fn comparison_tests() {
    // is_lt / is_ge use arithmetic comparison.
    ok_true(TestKind::IsLt, &[make_small(1), make_small(2)]);
    ok_false(TestKind::IsLt, &[make_small(2), make_small(1)]);
    ok_true(TestKind::IsGe, &[make_small(2), make_small(1)]);
    ok_false(TestKind::IsGe, &[make_small(1), make_small(2)]);
    ok_true(TestKind::IsEq, &[make_small(2), make_small(2)]);
    ok_false(TestKind::IsEq, &[make_small(2), make_small(3)]);
    ok_true(TestKind::IsNe, &[make_small(2), make_small(3)]);
    ok_false(TestKind::IsNe, &[make_small(2), make_small(2)]);
    ok_true(TestKind::IsEqExact, &[make_small(2), make_small(2)]);
    ok_false(TestKind::IsEqExact, &[make_small(2), make_small(3)]);
    ok_true(TestKind::IsNeExact, &[make_small(2), make_small(3)]);
    ok_false(TestKind::IsNeExact, &[make_small(2), make_small(2)]);
}

#[test]
fn number_type_tests() {
    let mut float_words = [HEADER_FLONUM, Eterm::from_raw(0)];
    let float = make_boxed(float_words.as_mut_ptr());
    let atom = make_atom_local(3);

    ok_true(TestKind::IsInteger, &[make_small(5)]);
    ok_false(TestKind::IsInteger, &[atom]);
    ok_true(TestKind::IsFloat, &[float]);
    ok_false(TestKind::IsFloat, &[make_small(5)]);
    ok_true(TestKind::IsNumber, &[make_small(5)]);
    ok_true(TestKind::IsNumber, &[float]);
    ok_false(TestKind::IsNumber, &[atom]);
}

/// A local helper so the tests do not depend on whether an atom is interned.
fn make_atom_local(raw: u32) -> Eterm {
    beam_term::make_atom(Atom::from_raw(raw))
}

#[test]
fn other_type_tests() {
    // Tuple (arity 2), map, reference and bitstring are boxed things.
    let mut tuple_words = [make_arityval(2), make_small(1), make_small(2)];
    let tuple = make_boxed(tuple_words.as_mut_ptr());
    let mut map_words = [make_header(0, MAP_SUBTAG)];
    let map = make_boxed(map_words.as_mut_ptr());
    let mut ref_words = [make_header(0, REF_SUBTAG)];
    let reference = make_boxed(ref_words.as_mut_ptr());
    let mut bin_words = [make_header(1, HEAP_BITS_SUBTAG), Eterm::from_raw(8)];
    let binary = make_boxed(bin_words.as_mut_ptr());
    let pid = make_internal_pid(make_pid_data(0, 1));
    let port = make_internal_port(make_port_data(1));
    let atom = make_atom_local(1);

    ok_true(TestKind::IsAtom, &[atom]);
    ok_false(TestKind::IsAtom, &[make_small(1)]);
    ok_true(TestKind::IsPid, &[pid]);
    ok_false(TestKind::IsPid, &[make_small(1)]);
    ok_true(TestKind::IsPort, &[port]);
    ok_false(TestKind::IsPort, &[make_small(1)]);
    ok_true(TestKind::IsReference, &[reference]);
    ok_false(TestKind::IsReference, &[make_small(1)]);
    ok_true(TestKind::IsBinary, &[binary]);
    ok_false(TestKind::IsBinary, &[make_small(1)]);
    ok_true(TestKind::IsBitstr, &[binary]);
    ok_true(TestKind::IsTuple, &[tuple]);
    ok_false(TestKind::IsTuple, &[make_small(1)]);
    ok_true(TestKind::IsMap, &[map]);
    ok_false(TestKind::IsMap, &[make_small(1)]);
}

#[test]
fn list_and_nil_tests_distinguish_nil_cons_and_other() {
    let cons_words = [make_small(1), NIL];
    let cons = beam_term::make_list(cons_words.as_ptr());

    // is_list: true for [] *and* a cons cell.
    ok_true(TestKind::IsList, &[NIL]);
    ok_true(TestKind::IsList, &[cons]);
    ok_false(TestKind::IsList, &[make_small(1)]);

    // is_nonempty_list: false for [].
    ok_false(TestKind::IsNonemptyList, &[NIL]);
    ok_true(TestKind::IsNonemptyList, &[cons]);

    // is_nil: true only for [].
    ok_true(TestKind::IsNil, &[NIL]);
    ok_false(TestKind::IsNil, &[cons]);
    ok_false(TestKind::IsNil, &[make_small(1)]);
}

#[test]
fn test_arity_matches_tuple_arity() {
    let mut tuple_words = [make_arityval(2), make_small(1), make_small(2)];
    let tuple = make_boxed(tuple_words.as_mut_ptr());

    let build = |arity: i64| {
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Test {
                kind: TestKind::TestArity,
                fail: Label(2),
                args: vec![Operand::X(0), Operand::Integer(arity)],
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let mut fixture = Fixture::new();
        let entry = fixture.add_function("t", 1, code);
        let module = fixture.finish();
        run(&module, entry, &[tuple])
    };

    assert_eq!(build(2).map_err(|_| ()), Ok(make_small(1)));
    assert_eq!(build(3).map_err(|_| ()), Ok(make_small(0)));
    // A negative/out-of-range arity can never match a tuple.
    assert_eq!(build(-1).map_err(|_| ()), Ok(make_small(0)));
}

#[test]
fn is_boolean_recognises_true_and_false_atoms() {
    // `is_boolean` compares against the module's `true`/`false` atoms, so those
    // names must be interned into the *same* module whose code is run.
    let mut fixture = Fixture::new();
    let true_atom = fixture.atom("true");
    let false_atom = fixture.atom("false");
    let other_atom = fixture.atom("other");
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Test {
            kind: TestKind::IsBoolean,
            fail: Label(2),
            args: vec![Operand::X(0)],
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("t", 1, code);
    let module = fixture.finish();

    let verdict = |value: Eterm| run(&module, entry, &[value]).map_err(|_| ());
    assert_eq!(verdict(beam_term::make_atom(true_atom)), Ok(make_small(1)));
    assert_eq!(verdict(beam_term::make_atom(false_atom)), Ok(make_small(1)));
    // Any other atom, or a non-atom, is not a boolean.
    assert_eq!(verdict(beam_term::make_atom(other_atom)), Ok(make_small(0)));
    assert_eq!(verdict(make_small(1)), Ok(make_small(0)));
    assert_eq!(verdict(NIL), Ok(make_small(0)));
}

#[test]
fn is_function_is_true_only_for_funs() {
    let mut ctx = MinimalContext::new();
    let fun = beam_engine::terms::make_fun(&mut ctx, 2, 0x1234, 3, &[]).unwrap();

    ok_true(TestKind::IsFunction, &[fun]);
    ok_false(TestKind::IsFunction, &[make_small(1)]);
    ok_false(TestKind::IsFunction, &[NIL]);
    ok_false(TestKind::IsFunction, &[make_atom_local(7)]);
}

#[test]
fn is_function2_matches_a_fun_of_the_given_arity() {
    let mut ctx = MinimalContext::new();
    let fun = beam_engine::terms::make_fun(&mut ctx, 2, 0x1234, 3, &[]).unwrap();

    // `is_function2 Fail Fun Arity` — args = [Fun, {integer,Arity}].
    let build = |value: Eterm, arity: Operand| {
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Test {
                kind: TestKind::IsFunction2,
                fail: Label(2),
                args: vec![Operand::X(0), arity],
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let mut fixture = Fixture::new();
        let entry = fixture.add_function("t", 1, code);
        let module = fixture.finish();
        run(&module, entry, &[value])
    };

    // A fun of the matching arity succeeds; the wrong arity fails.
    assert_eq!(
        build(fun, Operand::Integer(2)).map_err(|_| ()),
        Ok(make_small(1))
    );
    assert_eq!(
        build(fun, Operand::Integer(3)).map_err(|_| ()),
        Ok(make_small(0))
    );
    // A non-fun value fails regardless of the arity literal.
    assert_eq!(
        build(make_small(1), Operand::Integer(2)).map_err(|_| ()),
        Ok(make_small(0))
    );
    // A malformed (non-integer) arity operand is a typed error, never a wrong
    // result — and only reached for a real fun (the non-fun case short-circuits).
    assert!(build(fun, Operand::X(1)).is_err());
}

#[test]
fn is_tagged_tuple_matches_the_arity_and_the_tag() {
    // `{r, 9}`: a 2-tuple whose first element is the atom `r`. The tag operand is a
    // raw atom index (`make_atom`), so the module's atom *table* is not consulted.
    let tag = Atom::from_raw(11);
    let other = Atom::from_raw(12);
    let mut words = [make_arityval(2), beam_term::make_atom(tag), make_small(9)];
    let tuple = make_boxed(words.as_mut_ptr());

    // `is_tagged_tuple Fail Src Arity Tag` — args = [Src, {integer,Arity}, Tag].
    let build = |value: Eterm, arity: i64, tag_operand: Atom| {
        let code = vec![
            Instr::Label(Label(1)),
            Instr::Test {
                kind: TestKind::IsTaggedTuple,
                fail: Label(2),
                args: vec![
                    Operand::X(0),
                    Operand::Integer(arity),
                    Operand::Atom(tag_operand),
                ],
            },
            Instr::Move {
                src: Operand::Integer(1),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(Label(2)),
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Return,
        ];
        let mut fixture = Fixture::new();
        let entry = fixture.add_function("t", 1, code);
        let module = fixture.finish();
        run(&module, entry, &[value])
    };

    // Matching arity and tag.
    assert_eq!(build(tuple, 2, tag).map_err(|_| ()), Ok(make_small(1)));
    // Right tag, wrong arity.
    assert_eq!(build(tuple, 3, tag).map_err(|_| ()), Ok(make_small(0)));
    // Right arity, wrong tag.
    assert_eq!(build(tuple, 2, other).map_err(|_| ()), Ok(make_small(0)));
    // A negative or out-of-range arity can never match a tuple.
    assert_eq!(build(tuple, -1, tag).map_err(|_| ()), Ok(make_small(0)));
    // A non-tuple value fails.
    assert_eq!(
        build(make_small(1), 2, tag).map_err(|_| ()),
        Ok(make_small(0))
    );
}

// ---------------------------------------------------------------------------
// SelectVal
// ---------------------------------------------------------------------------

#[test]
fn select_val_matches_and_falls_through() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectVal {
            value: Operand::X(0),
            fail: Label(12),
            pairs: vec![
                (Operand::Integer(1), Label(10)),
                (Operand::Integer(2), Label(11)),
            ],
        },
        Instr::Label(Label(10)),
        Instr::Move {
            src: Operand::Integer(100),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(11)),
        Instr::Move {
            src: Operand::Integer(200),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(12)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    assert_eq!(
        run(&module, entry, &[make_small(1)]).map_err(|_| ()),
        Ok(make_small(100))
    );
    assert_eq!(
        run(&module, entry, &[make_small(2)]).map_err(|_| ()),
        Ok(make_small(200))
    );
    assert_eq!(
        run(&module, entry, &[make_small(9)]).map_err(|_| ()),
        Ok(make_small(0))
    );
}

#[test]
fn select_val_without_fail_label_raises_when_nothing_matches() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::SelectVal {
            value: Operand::X(0),
            fail: NO_FAIL,
            pairs: vec![(Operand::Integer(1), Label(10))],
        },
        Instr::Label(Label(10)),
        Instr::Move {
            src: Operand::Integer(100),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    assert_eq!(
        run(&module, entry, &[make_small(1)]).map_err(|_| ()),
        Ok(make_small(100))
    );
    assert!(run(&module, entry, &[make_small(9)]).is_err());
}

// ---------------------------------------------------------------------------
// GcBif
// ---------------------------------------------------------------------------

#[test]
fn gc_bif_writes_the_result_and_advances() {
    let mut fixture = Fixture::new();
    let plus = fixture.import("erlang", "+", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 1,
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("add", 2, code);
    let module = fixture.finish();
    assert_eq!(
        run_with_hook(&module, entry, &[make_small(2), make_small(3)]).map_err(|_| ()),
        Ok(make_small(5))
    );
}

#[test]
fn gc_bif_failure_jumps_to_the_fail_label() {
    let mut fixture = Fixture::new();
    let bad = fixture.import("erlang", "bad", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GcBif {
            arity: 2,
            bif: bad,
            fail: Label(2),
            live: 1,
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();
    assert_eq!(
        run_with_hook(&module, entry, &[make_small(2), make_small(3)]).map_err(|_| ()),
        Ok(make_small(0))
    );
}

#[test]
fn gc_bif_failure_without_a_fail_label_raises() {
    let mut fixture = Fixture::new();
    let bad = fixture.import("erlang", "bad", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::GcBif {
            arity: 2,
            bif: bad,
            fail: NO_FAIL,
            live: 1,
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();
    assert!(run_with_hook(&module, entry, &[make_small(2), make_small(3)]).is_err());
}

// ---------------------------------------------------------------------------
// Core opcodes: Bif0 / Bif / Jump / InitYregs / debug-line no-ops
// ---------------------------------------------------------------------------

/// A hook implementing `erlang:self/0` (returns `7`) and `erlang:'+'/2`.
fn self_and_arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module == "erlang" && function == "self" && arity == 0 && args.is_empty() {
        return Ok(make_small(7));
    }
    arith_hook(module, function, arity, args)
}

#[test]
fn bif0_calls_a_zero_arity_bif_and_writes_the_result() {
    let mut fixture = Fixture::new();
    let self_bif = fixture.import("erlang", "self", 0);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Bif0 {
            bif: self_bif,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(self_and_arith_hook));
    assert_eq!(
        machine
            .run_label(&module, entry, 0, &[], &mut ctx)
            .map_err(|_| ()),
        Ok(make_small(7))
    );
}

#[test]
fn bif0_failure_propagates_without_a_fail_label() {
    let mut fixture = Fixture::new();
    let self_bif = fixture.import("erlang", "self", 0);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Bif0 {
            bif: self_bif,
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    // The rejecting default hook makes the import fail; with no fail label the
    // error propagates as a typed error.
    assert!(run(&module, entry, &[]).is_err());
}

#[test]
fn bif_calls_with_arguments_and_writes_the_result() {
    let mut fixture = Fixture::new();
    let plus = fixture.import("erlang", "+", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Bif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("add", 2, code);
    let module = fixture.finish();
    assert_eq!(
        run_with_hook(&module, entry, &[make_small(2), make_small(3)]).map_err(|_| ()),
        Ok(make_small(5))
    );
}

#[test]
fn bif_failure_jumps_to_the_fail_label() {
    let mut fixture = Fixture::new();
    let bad = fixture.import("erlang", "bad", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Bif {
            arity: 2,
            bif: bad,
            fail: Label(2),
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();
    assert_eq!(
        run_with_hook(&module, entry, &[make_small(2), make_small(3)]).map_err(|_| ()),
        Ok(make_small(0))
    );
}

#[test]
fn bif_failure_without_a_fail_label_raises() {
    let mut fixture = Fixture::new();
    let bad = fixture.import("erlang", "bad", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Bif {
            arity: 2,
            bif: bad,
            fail: NO_FAIL,
            args: vec![Operand::X(0), Operand::X(1)],
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 2, code);
    let module = fixture.finish();
    assert!(run_with_hook(&module, entry, &[make_small(2), make_small(3)]).is_err());
}

#[test]
fn jump_transfers_control() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Jump { target: Label(2) },
        // Skipped by the jump.
        Instr::Move {
            src: Operand::Integer(9),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(5),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(make_small(5)));
}

#[test]
fn jump_to_an_unknown_label_is_a_typed_error() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Jump { target: Label(500) },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert!(run(&module, entry, &[]).is_err());
}

#[test]
fn init_yregs_clears_the_listed_slots_to_nil() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(3),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Integer(2),
            dst: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::Integer(3),
            dst: Operand::Y(2),
        },
        // Only Y(0) and Y(2) are re-initialised; Y(1) keeps its value.
        Instr::InitYregs {
            registers: vec![Operand::Y(0), Operand::Y(2)],
        },
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(3),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(make_small(2)));
}

#[test]
fn init_yregs_sets_every_listed_register_to_nil() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::Y(0),
        },
        Instr::InitYregs {
            registers: vec![Operand::Y(0), Operand::Y(1)],
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(NIL));
}

#[test]
fn debug_line_instructions_are_no_ops() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::ExecutableLine {
            location: 1,
            index: 0,
        },
        Instr::DebugLine {
            kind: Operand::Atom(Atom::from_raw(0)),
            location: 1,
            index: 1,
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(13),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(make_small(13)));
}

// ---------------------------------------------------------------------------
// Frames and the call family
// ---------------------------------------------------------------------------

#[test]
fn allocate_call_return_deallocate_preserve_y_slots() {
    let callee_label = Label(20);
    let caller_code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 1,
        },
        Instr::Move {
            src: Operand::Integer(11),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Integer(22),
            dst: Operand::Y(1),
        },
        Instr::Call {
            arity: 1,
            target: callee_label,
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(1),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let callee_code = vec![
        Instr::Label(callee_label),
        Instr::Move {
            src: Operand::Integer(7),
            dst: Operand::X(2),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("caller", 0, caller_code);
    fixture.add_function("callee", 1, callee_code);
    let module = fixture.finish();

    let (result, machine) = run_inspected(&module, entry, &[]);
    assert_eq!(result.map_err(|_| ()), Ok(make_small(22)));
    // The call returned through the caller's frame; only the bottom frame remains.
    assert_eq!(machine.frame_depth(), 1);
    assert_eq!(machine.stack_depth(), 0);
}

#[test]
fn call_last_deallocates_then_jumps() {
    let leaf = Label(20);
    let caller_code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(5),
            dst: Operand::Y(0),
        },
        Instr::CallLast {
            arity: 0,
            target: leaf,
            deallocate: 1,
        },
    ];
    let leaf_code = vec![
        Instr::Label(leaf),
        Instr::Move {
            src: Operand::Integer(77),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("tail", 0, caller_code);
    fixture.add_function("leaf", 0, leaf_code);
    let module = fixture.finish();

    let (result, machine) = run_inspected(&module, entry, &[]);
    assert_eq!(result.map_err(|_| ()), Ok(make_small(77)));
    assert_eq!(machine.frame_depth(), 1);
    assert_eq!(machine.stack_depth(), 0);
}

#[test]
fn call_last_with_wrong_deallocate_is_a_typed_error() {
    let leaf = Label(20);
    let caller_code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::CallLast {
            arity: 0,
            target: leaf,
            deallocate: 2,
        },
    ];
    let leaf_code = vec![
        Instr::Label(leaf),
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("tail", 0, caller_code);
    fixture.add_function("leaf", 0, leaf_code);
    let module = fixture.finish();
    assert!(run(&module, entry, &[]).is_err());
}

#[test]
fn call_only_jumps_without_touching_the_stack() {
    let leaf = Label(20);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Integer(9),
            dst: Operand::X(1),
        },
        Instr::CallOnly {
            arity: 0,
            target: leaf,
        },
    ];
    let leaf_code = vec![
        Instr::Label(leaf),
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("only", 0, code);
    fixture.add_function("leaf", 0, leaf_code);
    let module = fixture.finish();

    let (result, machine) = run_inspected(&module, entry, &[]);
    assert_eq!(result.map_err(|_| ()), Ok(make_small(9)));
    assert_eq!(machine.frame_depth(), 1);
    assert_eq!(machine.stack_depth(), 0);
}

#[test]
fn test_heap_and_allocate_heap() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::TestHeap {
            heap_need: HeapNeed::Words(4),
            live: 1,
        },
        Instr::AllocateHeap {
            stack_need: HeapNeed::Words(2),
            heap_need: HeapNeed::Words(8),
            live: 1,
        },
        Instr::Move {
            src: Operand::Integer(5),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(make_small(5)));
}

#[test]
fn trim_drops_y_slots() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(3),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::Integer(2),
            dst: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::Integer(3),
            dst: Operand::Y(2),
        },
        Instr::Trim { n: 1, remaining: 2 },
        Instr::Move {
            src: Operand::Y(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(2),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(make_small(2)));
}

#[test]
fn trim_overrun_is_a_typed_error() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(2),
            live: 0,
        },
        Instr::Trim { n: 3, remaining: 0 },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert!(run(&module, entry, &[]).is_err());
}

// ---------------------------------------------------------------------------
// CallExt family
// ---------------------------------------------------------------------------

#[test]
fn call_ext_writes_x0_and_continues_to_the_next_instruction() {
    let mut fixture = Fixture::new();
    let plus = fixture.import("erlang", "+", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(4),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(5),
            dst: Operand::X(1),
        },
        Instr::CallExt {
            arity: 2,
            target: plus,
        },
        // The call returned 9 in x0; only a real continuation reaches this test.
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: Label(2),
            args: vec![Operand::X(0), Operand::Integer(9)],
        },
        Instr::Move {
            src: Operand::Integer(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(2)),
        Instr::Move {
            src: Operand::Integer(0),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    // 1 means the call returned 9 *and* execution continued past the call.
    assert_eq!(
        run_with_hook(&module, entry, &[]).map_err(|_| ()),
        Ok(make_small(1))
    );
}

#[test]
fn call_ext_only_calls_then_returns() {
    let mut fixture = Fixture::new();
    let plus = fixture.import("erlang", "+", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Integer(4),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(5),
            dst: Operand::X(1),
        },
        Instr::CallExtOnly {
            arity: 2,
            target: plus,
        },
    ];
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert_eq!(
        run_with_hook(&module, entry, &[]).map_err(|_| ()),
        Ok(make_small(9))
    );
}

#[test]
fn call_ext_last_deallocates_calls_and_returns() {
    let mut fixture = Fixture::new();
    let plus = fixture.import("erlang", "+", 2);
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Move {
            src: Operand::Integer(4),
            dst: Operand::X(0),
        },
        Instr::Move {
            src: Operand::Integer(5),
            dst: Operand::X(1),
        },
        Instr::CallExtLast {
            arity: 2,
            target: plus,
            deallocate: 1,
        },
    ];
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    let result = machine.run_label(&module, entry, 0, &[], &mut ctx);
    assert_eq!(result.map_err(|_| ()), Ok(make_small(9)));
    assert_eq!(machine.frame_depth(), 1);
    assert_eq!(machine.stack_depth(), 0);
}

// ---------------------------------------------------------------------------
// Error paths — every one a typed error, never a panic
// ---------------------------------------------------------------------------

#[test]
fn generic_opcode_is_unsupported() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Generic {
            op: Opcode::send,
            operands: vec![],
            offset: 0,
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert!(run(&module, entry, &[]).is_err());
}

#[test]
fn float_register_operand_is_read_from_the_fr_file() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Test {
            kind: TestKind::IsInteger,
            fail: Label(2),
            args: vec![Operand::FloatReg(0)],
        },
        Instr::Return,
        Instr::Label(Label(2)),
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    // The FR file defaults to NIL, so `is_integer(FloatReg(0))` is readable and
    // fails the test, branching to the fail label (which returns `x(0) == NIL`).
    assert_eq!(run(&module, entry, &[]).map_err(|_| ()), Ok(NIL));
}

#[test]
fn literal_operand_is_unsupported() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::Literal(0),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert!(run(&module, entry, &[]).is_err());
}

#[test]
fn out_of_range_register_is_a_typed_error() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Move {
            src: Operand::X(5000),
            dst: Operand::X(0),
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert!(run(&module, entry, &[]).is_err());
}

#[test]
fn unknown_jump_label_is_a_typed_error() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Call {
            arity: 0,
            target: Label(500),
        },
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, code);
    let module = fixture.finish();
    assert!(run(&module, entry, &[]).is_err());
}

#[test]
fn failing_test_without_a_fail_label_raises() {
    let code = vec![
        Instr::Label(Label(1)),
        Instr::Test {
            kind: TestKind::IsInteger,
            fail: NO_FAIL,
            args: vec![Operand::X(0)],
        },
        Instr::Return,
    ];
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 1, code);
    let module = fixture.finish();
    // An atom is not an integer, so the test fails with no fail label.
    assert!(run(&module, entry, &[make_atom_local(1)]).is_err());
}

#[test]
fn run_export_of_unknown_export_is_a_typed_error() {
    let mut fixture = Fixture::new();
    let entry = fixture.add_function("f", 0, vec![Instr::Label(Label(1)), Instr::Return]);
    let name = fixture.atom("f");
    fixture.module.exports.push(Export {
        function: name,
        arity: 0,
        label: entry,
    });
    let module = fixture.finish();
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::new();
    // Correct export runs; a missing arity is a typed error.
    assert_eq!(
        machine
            .run_export(&module, name, 0, &[], &mut ctx)
            .map_err(|_| ()),
        Ok(NIL)
    );
    assert!(machine.run_export(&module, name, 1, &[], &mut ctx).is_err());
}

// ---------------------------------------------------------------------------
// A raising BIF with no fail label lands in a live catch frame
//
// Ground truth: with no fail label `gc_bif`/`bif` fall through to
// `find_func_info` (`erts/emulator/beam/emu/macros.tab:266-278`) and
// `call_light_bif*` runs `handle_error` (`emu/bif_instrs.tab:308-314`, `:409-415`);
// both end in `next_catch` (`erts/emulator/beam/beam_common.c:580-679`), which
// jumps to the innermost live `try`/`catch` handler. The oracle therefore
// *catches* `try 1/0 catch error:badarith -> caught end`.
// ---------------------------------------------------------------------------

/// Build a one-function `f/0` module from `code`.
///
/// `imports` are registered (in order, so the *i*-th entry is `ImportRef(i)`) and
/// `atoms` interned **before** `code` is added — the body refers to the imports'
/// `ImportRef`s, and the module must be able to *name* the reason a BIF failure
/// raises (`badarith`, …) plus the `error` class atom a landing installs in `x(3)`.
fn module_with(imports: &[(&str, &str, u32)], atoms: &[&str], code: Vec<Instr>) -> (Module, Label) {
    let mut fixture = Fixture::new();
    for (module, function, arity) in imports {
        fixture.import(module, function, *arity);
    }
    for name in atoms {
        fixture.atom(name);
    }
    let entry = fixture.add_function("f", 0, code);
    (fixture.finish(), entry)
}

/// Run `entry` with `hook` as the BIF implementation, returning the result **and**
/// the machine (so the pending exception / catch-frame state can be inspected).
fn run_hooked(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    hook: impl FnMut(&str, &str, u32, &[Eterm]) -> Result<Eterm, BeamError> + 'static,
) -> (Result<Eterm, BeamError>, Machine) {
    let mut machine = Machine::new();
    let mut ctx = MinimalContext::with_bif_hook(Box::new(hook));
    let result = machine.run_label(module, entry, args.len() as u32, args, &mut ctx);
    (result, machine)
}

/// `erlang:boom/N` always raises `badarith`; every other call raises `badarg`.
fn boom_hook(
    _module: &str,
    function: &str,
    _arity: u32,
    _args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if function == "boom" {
        Err(BeamError::BadArith)
    } else {
        Err(BeamError::BadArg)
    }
}

/// `f() -> try <body> catch _ -> Reason end`: `Label(1)` enters the `try`,
/// `Label(3)` is the handler, which returns the landed reason from `x(1)` (a
/// catch-all clause needs no class atom in its code, but a landing needs the
/// `error` class atom, so it must be interned).
fn try_wrapped(body: Vec<Instr>) -> Vec<Instr> {
    let mut code = vec![
        Instr::Label(Label(1)),
        Instr::Try {
            frame: Operand::X(9),
            fail: Label(3),
        },
    ];
    code.extend(body);
    code.extend([
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
    ]);
    code
}

/// The interned `badarith` term of `module`.
fn badarith(module: &Module) -> Eterm {
    beam_term::make_atom(
        module
            .atom_table
            .lookup("badarith")
            .expect("badarith interned"),
    )
}

#[test]
fn a_raising_gc_bif_without_a_fail_label_lands_in_a_live_catch_frame() {
    // f() -> try erlang:boom() catch _ -> Reason end.
    // `gc_bif0 erlang:boom` with NO fail label: `badarith` must land at `Label(3)`.
    let (module, entry) = module_with(
        &[("erlang", "boom", 0)],
        &["badarith", "error"],
        try_wrapped(vec![Instr::GcBif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 0,
            args: Vec::new(),
            dst: Operand::X(0),
        }]),
    );

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert_eq!(
        result.map_err(|_| ()),
        Ok(badarith(&module)),
        "the raise must land in the handler and yield the reason"
    );
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, badarith(&module))),
        "the landed exception is class `error` with the BIF's error atom"
    );
}

#[test]
fn a_raising_gc_bif_without_a_catch_frame_propagates_the_typed_error() {
    // The same failing `gc_bif0`, but with no live `try`: the error still
    // propagates as the caller's typed error (never a panic, never a wrong value).
    // The module *can* name `badarith`, so the fallback is purely the missing
    // catch frame.
    let (module, entry) = module_with(
        &[("erlang", "boom", 0)],
        &["badarith", "error"],
        vec![
            Instr::Label(Label(1)),
            Instr::GcBif {
                arity: 0,
                bif: ImportRef(0),
                fail: NO_FAIL,
                live: 0,
                args: Vec::new(),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert!(
        matches!(result, Err(BeamError::BadArith)),
        "an uncaught raise propagates the typed error, got {result:?}"
    );
    assert_eq!(machine.catch_frame_depth(), 0);
}

#[test]
fn a_raising_bif_without_a_fail_label_lands_in_a_live_catch_frame() {
    // The non-garbage-collecting `bif1` opcode behaves identically.
    let (module, entry) = module_with(
        &[("erlang", "boom", 1)],
        &["badarith", "error"],
        try_wrapped(vec![Instr::Bif {
            arity: 1,
            bif: ImportRef(0),
            fail: NO_FAIL,
            args: vec![Operand::X(0)],
            dst: Operand::X(0),
        }]),
    );

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert_eq!(result.map_err(|_| ()), Ok(badarith(&module)));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, badarith(&module)))
    );
}

#[test]
fn a_raising_bif0_lands_in_a_live_catch_frame() {
    // `bif0` has no fail label at all (C routes it through `call_light_bif`).
    let (module, entry) = module_with(
        &[("erlang", "boom", 0)],
        &["badarith", "error"],
        try_wrapped(vec![Instr::Bif0 {
            bif: ImportRef(0),
            dst: Operand::X(0),
        }]),
    );

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert_eq!(result.map_err(|_| ()), Ok(badarith(&module)));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, badarith(&module)))
    );
}

#[test]
fn a_guard_bif_failure_still_jumps_to_the_fail_label_with_a_live_catch_frame() {
    // f() -> try (case boom() of _ -> ok end) catch _ -> Reason end.
    // The guard `gc_bif0` HAS a fail label, so a failure must jump to `Label(4)`
    // — a guard failure is not a raise, even with a live catch frame: no
    // exception is recorded and the catch frame is left untouched.
    let (module, entry) = module_with(
        &[("erlang", "boom", 0)],
        &["badarith", "error"],
        vec![
            Instr::Label(Label(1)),
            Instr::Try {
                frame: Operand::X(9),
                fail: Label(3),
            },
            Instr::GcBif {
                arity: 0,
                bif: ImportRef(0),
                fail: Label(4),
                live: 0,
                args: Vec::new(),
                dst: Operand::X(0),
            },
            Instr::Move {
                src: Operand::Integer(100),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(Label(4)),
            Instr::Move {
                src: Operand::Integer(0),
                dst: Operand::X(0),
            },
            Instr::Return,
            Instr::Label(Label(3)),
            Instr::Move {
                src: Operand::Integer(-1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert_eq!(
        result.map_err(|_| ()),
        Ok(make_small(0)),
        "the guard failure jumps to its fail label"
    );
    assert_eq!(
        machine.pending_exception(),
        None,
        "a guard failure is not a raise"
    );
    assert_eq!(
        machine.catch_frame_depth(),
        1,
        "the catch frame is left in place (no unwind)"
    );
}

#[test]
fn a_raising_call_ext_lands_in_a_live_catch_frame() {
    let (module, entry) = module_with(
        &[("erlang", "boom", 1)],
        &["badarith", "error"],
        try_wrapped(vec![Instr::CallExt {
            arity: 1,
            target: ImportRef(0),
        }]),
    );

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert_eq!(result.map_err(|_| ()), Ok(badarith(&module)));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, badarith(&module)))
    );
}

#[test]
fn a_raising_call_ext_only_lands_in_a_live_catch_frame() {
    let (module, entry) = module_with(
        &[("erlang", "boom", 1)],
        &["badarith", "error"],
        try_wrapped(vec![Instr::CallExtOnly {
            arity: 1,
            target: ImportRef(0),
        }]),
    );

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert_eq!(result.map_err(|_| ()), Ok(badarith(&module)));
    assert_eq!(
        machine.pending_exception(),
        Some((ExceptionClass::Error, badarith(&module)))
    );
}

#[test]
fn a_raising_call_ext_last_lands_in_the_outer_catch_frame() {
    // outer() -> try inner() catch _ -> Reason end.
    // inner() -> allocate 0, then `call_ext_last erlang:boom/1 D=0`.
    //
    // `call_ext_last` pops *its own* (inner) frame before the call, while the
    // catch frame lives in `outer`'s frame, so the raise must still unwind to
    // `outer`'s handler — the frame the catch tag belongs to.
    let mut fixture = Fixture::new();
    let boom = fixture.import("erlang", "boom", 1);
    fixture.atom("badarith");
    fixture.atom("error");

    let outer = vec![
        Instr::Label(Label(1)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(1),
            live: 0,
        },
        Instr::Try {
            frame: Operand::Y(0),
            fail: Label(3),
        },
        Instr::Call {
            arity: 0,
            target: Label(20),
        },
        Instr::TryEnd {
            frame: Operand::Y(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
        Instr::Label(Label(3)),
        Instr::TryCase {
            frame: Operand::Y(0),
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Deallocate(1),
        Instr::Return,
    ];
    let inner = vec![
        Instr::Label(Label(20)),
        Instr::Allocate {
            stack_need: HeapNeed::Words(0),
            live: 0,
        },
        Instr::CallExtLast {
            arity: 1,
            target: boom,
            deallocate: 0,
        },
    ];
    let entry = fixture.add_function("outer", 0, outer);
    fixture.add_function("inner", 0, inner);
    let module = fixture.finish();

    let (result, machine) = run_hooked(&module, entry, &[], boom_hook);
    assert_eq!(result.map_err(|_| ()), Ok(badarith(&module)));
    assert_eq!(
        machine.catch_frame_depth(),
        0,
        "the handler consumed the frame"
    );
    assert_eq!(
        machine.frame_depth(),
        1,
        "the run is back at the bottom frame"
    );
}

#[test]
fn a_bif_error_the_module_cannot_name_propagates_unchanged() {
    // A live catch frame but no `badarith` atom in the module: the engine refuses
    // to substitute a *different* atom (that would be a wrong result), so the
    // typed error propagates — a documented Milestone-1 limit.
    let (module, entry) = module_with(
        &[("erlang", "boom", 0)],
        &["error"],
        try_wrapped(vec![Instr::GcBif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 0,
            args: Vec::new(),
            dst: Operand::X(0),
        }]),
    );

    let (result, _machine) = run_hooked(&module, entry, &[], boom_hook);
    assert!(
        matches!(result, Err(BeamError::BadArith)),
        "an unnameable error atom propagates, got {result:?}"
    );
}

#[test]
fn an_explicit_raise_error_is_not_folded_into_a_catch_frame() {
    // A `BeamError::Raise` carries its own class/reason term-independently (the
    // reason may be `ExitReason::Structured`), so it is not converted here: it
    // propagates unchanged even with a live catch frame.
    let (module, entry) = module_with(
        &[("erlang", "boom", 0)],
        &["badarith", "error"],
        try_wrapped(vec![Instr::GcBif {
            arity: 0,
            bif: ImportRef(0),
            fail: NO_FAIL,
            live: 0,
            args: Vec::new(),
            dst: Operand::X(0),
        }]),
    );

    let (result, _machine) = run_hooked(&module, entry, &[], |_m, _f, _a, _args| {
        Err(BeamError::Raise(beam_common::Raise::error(
            beam_common::ExitReason::atom("badarith"),
        )))
    });
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "an explicit raise propagates, got {result:?}"
    );
}
