//! Records instruction-family tests.
//!
//! Two halves:
//!
//! * **Fixture-driven integration** — load the committed oracle `.beam`
//!   (`tests/fixtures/records.beam`, produced by the system `erlc`) and run its
//!   exports end to end through [`Machine::run_export`], asserting the values the
//!   oracle produces. The fixture covers all three compiler reuse hints
//!   (`reuse`/`copy`/`inplace`) and the `{Updated, Original}` non-aliasing case.
//! * **Synthetic modules** — hand-built [`Module`]s (the loader is not involved)
//!   that reach the paths a compiler-generated module guards with an
//!   `is_tagged_tuple` test: a non-tuple source, a `size`/arity mismatch and an
//!   out-of-range update offset are typed [`BeamError`]s (never a panic, never a
//!   wrong result), and an update value that aliases `dst` still reads the
//!   *original* source tuple (copy-then-patch, `i_update_record_copy`).
//!
//! Every error is a typed [`BeamError`] — no test panics on runtime data.

use std::collections::BTreeMap;

use beam_common::{BeamError, ExceptionClass};
use beam_engine::{terms, Machine, MinimalContext};
use beam_loader::{load_file, Function, Instr, Label, Module, Operand};
use beam_term::{make_atom, make_small, Atom, Eterm};

/// The committed records fixture (`erlc records.erl`).
const RECORDS_BEAM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/records.beam");

// ---------------------------------------------------------------------------
// Fixture-driven integration
// ---------------------------------------------------------------------------

fn load(path: &str) -> Module {
    load_file(path).unwrap_or_else(|e| panic!("load {path}: {e:?}"))
}

/// `module:function/arity` for an export of `module`.
fn export_atom(module: &Module, name: &str) -> Atom {
    module
        .atom_table
        .lookup(name)
        .unwrap_or_else(|| panic!("atom {name:?} not interned"))
}

/// A hook implementing `erlang:'+'/2` over small integers (the fixture's
/// `chain/1` computes `A + 1`).
fn arith_hook(
    module: &str,
    function: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm, BeamError> {
    if module != "erlang" || function != "+" || arity != 2 || args.len() != 2 {
        return Err(BeamError::BadArg);
    }
    if !beam_term::is_small(args[0]) || !beam_term::is_small(args[1]) {
        return Err(BeamError::BadArith);
    }
    Ok(make_small(
        beam_term::signed_value(args[0]) + beam_term::signed_value(args[1]),
    ))
}

/// Run an export with `args` on a fresh machine over `ctx`.
fn run_export_with(
    module: &Module,
    name: &str,
    arity: u32,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> (Result<Eterm, BeamError>, Machine) {
    let mut machine = Machine::new();
    let result = machine.run_export(module, export_atom(module, name), arity, args, ctx);
    (result, machine)
}

/// Build the `#r{}` record tuple `{r, A, B, C}` of the fixture module.
fn record(ctx: &mut MinimalContext, module: &Module, a: i64, b: i64, c: i64) -> Eterm {
    let tag = make_atom(export_atom(module, "r"));
    terms::make_tuple(ctx, &[tag, make_small(a), make_small(b), make_small(c)])
        .expect("build the record tuple")
}

/// Assert `value` is the record tuple `{r, A, B, C}`.
fn assert_record(module: &Module, value: Eterm, a: i64, b: i64, c: i64) {
    assert_eq!(terms::tuple_arity(value).unwrap(), 4, "arity of {value:?}");
    assert_eq!(
        terms::tuple_element(value, 0).unwrap(),
        make_atom(export_atom(module, "r"))
    );
    assert_eq!(terms::tuple_element(value, 1).unwrap(), make_small(a));
    assert_eq!(terms::tuple_element(value, 2).unwrap(), make_small(b));
    assert_eq!(terms::tuple_element(value, 3).unwrap(), make_small(c));
}

#[test]
fn fixture_update_a_replaces_one_field() {
    // update_a(R) -> R#r{a = 10}.   (reuse hint, offset 2)
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "update_a", 1, &[arg], &mut ctx);
    let result = result.expect("update_a({r,1,2,3})");
    assert_record(&module, result, 10, 2, 3);
    // The source tuple is a distinct term and is left untouched.
    assert_record(&module, arg, 1, 2, 3);
    assert_ne!(result, arg);
}

#[test]
fn fixture_update_b_replaces_a_later_field() {
    // update_b(R) -> R#r{b = 20}.   (reuse hint, offset 3)
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "update_b", 1, &[arg], &mut ctx);
    let result = result.expect("update_b({r,1,2,3})");
    assert_record(&module, result, 1, 20, 3);
    assert_record(&module, arg, 1, 2, 3);
}

#[test]
fn fixture_update_a_c_replaces_two_fields() {
    // update_a_c(R) -> R#r{a = 11, c = 33}.   (copy hint, offsets 2 and 4)
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "update_a_c", 1, &[arg], &mut ctx);
    let result = result.expect("update_a_c({r,1,2,3})");
    assert_record(&module, result, 11, 2, 33);
    assert_record(&module, arg, 1, 2, 3);
}

#[test]
fn fixture_chain_builds_then_updates_an_inplace_hint_record() {
    // chain(A) -> R = #r{a = A}, R#r{b = A + 1}.
    // The compiler marks the update `inplace` (R is unique/dead); the engine
    // always copies, which yields the same observable tuple. The fixture's
    // oracle value is `{r,5,6,undefined}`.
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::with_bif_hook(Box::new(arith_hook));
    let (result, _) = run_export_with(&module, "chain", 1, &[make_small(5)], &mut ctx);
    let result = result.expect("chain(5)");
    assert_eq!(terms::tuple_arity(result).unwrap(), 4);
    assert_eq!(
        terms::tuple_element(result, 0).unwrap(),
        make_atom(export_atom(&module, "r"))
    );
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(5));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(6));
    assert_eq!(
        terms::tuple_element(result, 3).unwrap(),
        make_atom(export_atom(&module, "undefined"))
    );
}

#[test]
fn fixture_keep_returns_a_fresh_tuple_and_preserves_the_source() {
    // keep(R) -> {R#r{a = x}, R}.
    // The updated record goes to a *different* register than the source, so this
    // is the explicit non-aliasing check: the second element must still be the
    // original record. The oracle value is `{{r,x,2,3},{r,1,2,3}}`.
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let arg = record(&mut ctx, &module, 1, 2, 3);
    let (result, _) = run_export_with(&module, "keep", 1, &[arg], &mut ctx);
    let result = result.expect("keep({r,1,2,3})");
    assert_eq!(terms::tuple_arity(result).unwrap(), 2);

    let updated = terms::tuple_element(result, 0).unwrap();
    assert_eq!(
        terms::tuple_element(updated, 0).unwrap(),
        make_atom(export_atom(&module, "r"))
    );
    assert_eq!(
        terms::tuple_element(updated, 1).unwrap(),
        make_atom(export_atom(&module, "x"))
    );
    assert_eq!(terms::tuple_element(updated, 2).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(updated, 3).unwrap(), make_small(3));

    let original = terms::tuple_element(result, 1).unwrap();
    assert_record(&module, original, 1, 2, 3);
    assert_ne!(updated, original);
}

#[test]
fn fixture_badrecord_tag_is_interned() {
    // badrecord_tag() -> badrecord.  The `badrecord` opcode carries no atom
    // operand, so the module needs this export for the interpreter to *name* the
    // `{badrecord, Value}` reason its `is_tagged_tuple` fallback raises.
    let module = load(RECORDS_BEAM);
    assert!(
        module.atom_table.lookup("badrecord").is_some(),
        "the fixture must intern the `badrecord` atom"
    );
    let mut ctx = MinimalContext::new();
    let (result, _) = run_export_with(&module, "badrecord_tag", 0, &[], &mut ctx);
    assert_eq!(
        result.expect("badrecord_tag()"),
        make_atom(export_atom(&module, "badrecord"))
    );
}

#[test]
fn fixture_non_record_raises_badrecord() {
    // non_record(V) -> V#r{a = 1}.  The compiler's `is_tagged_tuple` guard fails
    // for a non-record, so the generated `badrecord` raises `{badrecord, V}`:
    // the compiled `update_record` is never reached with a non-tuple source.
    let module = load(RECORDS_BEAM);
    let mut ctx = MinimalContext::new();
    let value = make_small(7);
    let (result, machine) = run_export_with(&module, "non_record", 1, &[value], &mut ctx);
    assert!(
        matches!(result, Err(BeamError::Raise(_))),
        "expected a raise, got {result:?}"
    );
    let (class, reason) = machine.pending_exception().expect("a recorded exception");
    assert_eq!(class, ExceptionClass::Error);
    assert_eq!(terms::tuple_arity(reason).unwrap(), 2);
    assert_eq!(
        terms::tuple_element(reason, 0).unwrap(),
        make_atom(export_atom(&module, "badrecord"))
    );
    assert_eq!(terms::tuple_element(reason, 1).unwrap(), value);
}

// ---------------------------------------------------------------------------
// Synthetic modules
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

    /// Append a function; returns its entry label (the lowest-numbered label).
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

/// Run `entry` with `args` on a fresh machine, over the **caller-owned** `ctx`.
fn run(
    module: &Module,
    entry: Label,
    args: &[Eterm],
    ctx: &mut MinimalContext,
) -> Result<Eterm, BeamError> {
    let mut machine = Machine::new();
    machine.run_label(module, entry, args.len() as u32, args, ctx)
}

/// Assert `result` is the engine's typed `unsupported` raise for `what`.
fn assert_unsupported(result: Result<Eterm, BeamError>, what: &str) {
    let want = format!("unsupported_opcode: {what}");
    match result {
        Err(BeamError::Raise(raise)) => assert_eq!(
            raise.reason.as_atom(),
            Some(want.as_str()),
            "unexpected unsupported reason"
        ),
        other => panic!("expected the unsupported {what:?} raise, got {other:?}"),
    }
}

/// `update_record Hint Size Src Dst Updates` as a single-instruction function.
fn update_record_fn(
    hint: Atom,
    size: u32,
    src: Operand,
    dst: Operand,
    updates: Vec<(Operand, Operand)>,
) -> Vec<Instr> {
    vec![
        Instr::Label(Label(1)),
        Instr::UpdateRecord {
            hint: Operand::Atom(hint),
            size,
            src,
            dst,
            updates,
        },
        Instr::Return,
    ]
}

#[test]
fn update_record_copies_the_source_and_replaces_a_field() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(3), Operand::Integer(99))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 3);
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(99));
    // The source is a fresh copy, not the mutated original.
    assert_ne!(result, arg);
    assert_eq!(terms::tuple_element(arg, 2).unwrap(), make_small(3));
}

#[test]
fn update_record_applies_multiple_updates() {
    // `dst` is X(1), a different register than the source: a following `move`
    // brings the result into X(0) so `run_label` returns it.
    let mut fixture = Fixture::new();
    let copy = fixture.atom("copy");
    let entry = fixture.add_function(
        "f",
        1,
        vec![
            Instr::Label(Label(1)),
            Instr::UpdateRecord {
                hint: Operand::Atom(copy),
                size: 4,
                src: Operand::X(0),
                dst: Operand::X(1),
                updates: vec![
                    (Operand::Integer(1), Operand::Atom(Atom::from_raw(0))),
                    (Operand::Integer(3), Operand::Integer(7)),
                ],
            },
            Instr::Move {
                src: Operand::X(1),
                dst: Operand::X(0),
            },
            Instr::Return,
        ],
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(
        &mut ctx,
        &[make_small(1), make_small(2), make_small(3), make_small(4)],
    )
    .unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 4);
    assert_eq!(
        terms::tuple_element(result, 0).unwrap(),
        make_atom(Atom::from_raw(0))
    );
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(7));
    assert_eq!(terms::tuple_element(result, 3).unwrap(), make_small(4));
    // The copied source operand is untouched.
    assert_eq!(terms::tuple_element(arg, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(arg, 2).unwrap(), make_small(3));
    assert_ne!(result, arg);
}

#[test]
fn update_record_value_aliasing_dst_reads_the_original_source() {
    // `src == dst == X(0)` and the single update's value is also `X(0)`: the
    // result must embed the *original* tuple (copy-then-patch), never a
    // half-updated one.
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(3), Operand::X(0))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_arity(result).unwrap(), 3);
    let nested = terms::tuple_element(result, 2).unwrap();
    assert_eq!(terms::tuple_element(nested, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(nested, 1).unwrap(), make_small(2));
    assert_eq!(terms::tuple_element(nested, 2).unwrap(), make_small(3));
}

#[test]
fn update_record_with_a_non_tuple_source_is_a_typed_error() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            2,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(2), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let result = run(&module, entry, &[make_small(5)], &mut ctx);
    assert_unsupported(result, "update_record: non-tuple source");
}

#[test]
fn update_record_with_an_out_of_range_index_is_a_typed_error() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(4), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx);
    assert_unsupported(result, "update_record: index out of range");
}

#[test]
fn update_record_with_a_zero_index_is_a_typed_error() {
    // Offset 0 would name the arityval header, never a field (`Offset` is 1-based).
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(0), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx);
    assert_unsupported(result, "update_record: index out of range");
}

#[test]
fn update_record_with_a_size_mismatch_is_a_typed_error() {
    let mut fixture = Fixture::new();
    let reuse = fixture.atom("reuse");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            reuse,
            5,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(2), Operand::Integer(9))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx);
    assert_unsupported(result, "update_record: size/arity mismatch");
}

#[test]
fn update_record_ignores_an_inplace_hint_and_still_copies() {
    // An `inplace` hint (a unique/dead source) must produce an equal tuple; the
    // source term is never mutated, so a still-referenced source stays intact.
    let mut fixture = Fixture::new();
    let inplace = fixture.atom("inplace");
    let entry = fixture.add_function(
        "f",
        1,
        update_record_fn(
            inplace,
            3,
            Operand::X(0),
            Operand::X(0),
            vec![(Operand::Integer(2), Operand::Integer(42))],
        ),
    );
    let module = fixture.finish();

    let mut ctx = MinimalContext::new();
    let arg = terms::make_tuple(&mut ctx, &[make_small(1), make_small(2), make_small(3)]).unwrap();
    let result = run(&module, entry, &[arg], &mut ctx).unwrap();
    assert_eq!(terms::tuple_element(result, 1).unwrap(), make_small(42));
    assert_eq!(terms::tuple_element(result, 0).unwrap(), make_small(1));
    assert_eq!(terms::tuple_element(result, 2).unwrap(), make_small(3));
    assert_eq!(terms::tuple_element(arg, 1).unwrap(), make_small(2));
    assert_ne!(result, arg);
}
