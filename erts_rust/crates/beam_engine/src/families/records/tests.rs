//! Per-variant unit tests for the native-record instruction family.
//!
//! Every one of the seven typed native-record [`Instr`] variants gets its own
//! group of tests covering the three decisions the family can make:
//!
//! * the **success** path (fall through, with the documented register effects),
//! * the **fail-branch** path (a record-semantics failure the C reports with
//!   `$FAIL($Fail)` / `$FAIL_HEAD_OR_BODY($Fail)` must produce
//!   `Step::Goto { func, pc }` for the resolved `fail` label), and
//! * the [`NO_FAIL`] **capability gap** (a typed `unsupported("<instr>/no_fail")`,
//!   never a fall-through with a wrong result).
//!
//! The fixtures are real native records built through [`beam_term::write_record`]
//! (reached via the family's [`raw`](super::raw) forwarder) into the
//! [`MinimalContext`] arena, so the tests exercise the frozen layout without ever
//! re-deriving it. The instructions are hand-built [`Instr`]s driven straight
//! through [`exec`](super::exec) over a [`Machine`] and the mock context — the
//! loader is not involved.
//!
//! Everything asserted here is the family's own documented decision (see
//! `mod.rs`), which in turn mirrors `../../erts/emulator/beam/erl_record.c` and
//! `emu/instrs.tab`.

use std::collections::BTreeMap;

use beam_common::BeamError;
use beam_term::{make_atom, make_small, NIL};

use super::*;
use crate::context::MinimalContext;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// A synthetic module (`name = m`) with an owned atom table.
struct Fixture {
    module: Module,
}

impl Fixture {
    fn new() -> Fixture {
        let mut module = Module::default();
        module.name = module.atom_table.intern("m");
        Fixture { module }
    }

    /// Intern `name`, returning its atom.
    fn atom(&mut self, name: &str) -> Atom {
        self.module.atom_table.intern(name)
    }

    fn module(&self) -> &Module {
        &self.module
    }
}

/// Build a native record with `fields` (`(key atom, value)` pairs, any order) and
/// return the tagged **instance** root.
///
/// The construction goes through [`beam_term::write_record`] (via the family's
/// `raw` forwarder) into the context arena, so the frozen layout stays owned by
/// `beam_term` — this test never re-derives it. The definition hash is an
/// arbitrary small integer (`beam_term` stores it verbatim; nothing in these
/// tests reads it).
fn record(
    ctx: &mut MinimalContext,
    owner: Atom,
    name: Atom,
    exported: bool,
    fields: &[(Atom, Eterm)],
) -> Eterm {
    let fields: Vec<(Eterm, Eterm)> = fields.iter().map(|(k, v)| (make_atom(*k), *v)).collect();
    let hp = ctx
        .alloc_words(beam_term::record_block_words(fields.len()))
        .expect("arena allocation");
    raw::write_record(hp, owner, name, exported, &fields, make_small(0))
        .expect("record construction")
}

/// The value of the field keyed `key` in `rec`, or `None`.
fn field(module: &Module, rec: Eterm, key: Atom) -> Option<Eterm> {
    raw::record_get(rec, key, &module.atom_table).expect("a valid record")
}

/// A label map with `label` resolved to `(func, pc)`.
fn labels_with(label: Label, func: u32, pc: u32) -> BTreeMap<Label, (u32, u32)> {
    BTreeMap::from([(label, (func, pc))])
}

/// Drive one instruction through the family over `machine`/`ctx`.
fn run(
    module: &Module,
    labels: &BTreeMap<Label, (u32, u32)>,
    machine: &mut Machine,
    instr: &Instr,
    ctx: &mut MinimalContext,
) -> beam_common::Result<Option<Step>> {
    exec(machine, module, labels, instr, 0, 0, ctx)
}

/// The `unsupported_opcode: <reason>` atom text of a typed error.
fn unsupported_reason(result: beam_common::Result<Option<Step>>) -> String {
    match result.expect_err("expected a typed error") {
        BeamError::Raise(raise) => raise
            .reason
            .as_atom()
            .expect("the reason is an atom")
            .to_string(),
        other => panic!("expected a typed raise, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// is_native_record (opcode 186)
// ---------------------------------------------------------------------------

#[test]
fn is_native_record_succeeds_on_a_matching_definition() {
    let mut fx = Fixture::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: NO_FAIL,
            src: Operand::X(0),
            module: Operand::Atom(m),
            name: Operand::Atom(r),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
}

#[test]
fn is_native_record_branches_to_fail_on_a_module_mismatch() {
    let mut fx = Fixture::new();
    let (m, r, a, other) = (fx.atom("m"), fx.atom("r"), fx.atom("a"), fx.atom("other"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    // The record lives in `m`; asking for it in `other` is `erl_is_native_record`
    // == false ⇒ `$FAIL($Fail)`.
    let step = run(
        fx.module(),
        &labels_with(Label(7), 2, 9),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: Label(7),
            src: Operand::X(0),
            module: Operand::Atom(other),
            name: Operand::Atom(r),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 2, pc: 9 }));
}

#[test]
fn is_native_record_branches_to_fail_on_a_name_mismatch() {
    let mut fx = Fixture::new();
    let (m, r, s, a) = (fx.atom("m"), fx.atom("r"), fx.atom("s"), fx.atom("a"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: Label(7),
            src: Operand::X(0),
            module: Operand::Atom(m),
            name: Operand::Atom(s),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_native_record_branches_to_fail_on_a_non_record() {
    let mut fx = Fixture::new();
    let (m, r) = (fx.atom("m"), fx.atom("r"));
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: Label(7),
            src: Operand::X(0),
            module: Operand::Atom(m),
            name: Operand::Atom(r),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_native_record_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let (m, r, other) = (fx.atom("m"), fx.atom("r"), fx.atom("other"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: NO_FAIL,
            src: Operand::X(0),
            module: Operand::Atom(other),
            name: Operand::Atom(r),
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: is_native_record/no_fail");
}

#[test]
fn is_native_record_reads_the_module_and_name_operands_as_atoms() {
    // A register `module` operand is not a compile-time atom: the family reports it
    // rather than reading an arbitrary register as an atom.
    let mut fx = Fixture::new();
    let (m, r) = (fx.atom("m"), fx.atom("r"));
    let mut ctx = MinimalContext::new();
    let rec = record(&mut ctx, m, r, false, &[]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(1, make_small(3)).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::IsNativeRecord {
            fail: NO_FAIL,
            src: Operand::X(0),
            module: Operand::X(1),
            name: Operand::Atom(r),
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: is_native_record: module");
}

// ---------------------------------------------------------------------------
// get_record_elements (opcode 187) / get_record_elements_id (opcode 192)
// ---------------------------------------------------------------------------

/// A record with fields `a = 11`, `b = 22` in module `m` named `r`.
fn two_field_record(fx: &mut Fixture, ctx: &mut MinimalContext) -> (Eterm, Atom, Atom, Atom) {
    let (m, r, a, b) = (fx.atom("m"), fx.atom("r"), fx.atom("a"), fx.atom("b"));
    let rec = record(
        ctx,
        m,
        r,
        false,
        &[(b, make_small(22)), (a, make_small(11))],
    );
    (rec, a, b, r)
}

#[test]
fn get_record_elements_writes_each_listed_field_in_list_order() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, b, _r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordElements {
            fail: NO_FAIL,
            src: Operand::X(0),
            fields: vec![
                (Operand::Atom(b), Operand::X(2)),
                (Operand::Atom(a), Operand::X(1)),
            ],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
    assert_eq!(machine.x(2), Some(make_small(22)));
}

#[test]
fn get_record_elements_branches_to_fail_after_writing_the_fields_before_the_absent_one() {
    // C fills each `PUT_TERM_REG` *before* the final `return false`, so a field the
    // record lacks fails the instruction but leaves the earlier destinations set.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z"); // never a field of this record
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 1, 3),
        &mut machine,
        &Instr::GetRecordElements {
            fail: Label(7),
            src: Operand::X(0),
            fields: vec![
                (Operand::Atom(a), Operand::X(1)),
                (Operand::Atom(z), Operand::X(2)),
            ],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 1, pc: 3 }));
    assert_eq!(machine.x(1), Some(make_small(11)), "the first field is written");
    assert_eq!(machine.x(2), Some(NIL), "the absent field's destination is untouched");
}

#[test]
fn get_record_elements_branches_to_fail_on_a_non_record_source() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let a = fx.atom("a");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordElements {
            fail: Label(7),
            src: Operand::X(0),
            fields: vec![(Operand::Atom(a), Operand::X(1))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_elements_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordElements {
            fail: NO_FAIL,
            src: Operand::X(0),
            fields: vec![(Operand::Atom(z), Operand::X(1))],
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: get_record_elements/no_fail");
}

#[test]
fn get_record_elements_id_is_get_record_elements_with_the_id_discarded() {
    // `emu/ops.tab:1643` lowers `get_record_elements_id Fail _Id Src …` to
    // `i_get_record_elements Fail Src …`: the `Id` never reaches the implementation,
    // so even a nonsense `Id` operand is harmless.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, b, _r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, make_small(99)).unwrap(); // not a record id at all

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordElementsId {
            fail: NO_FAIL,
            id: Operand::X(3),
            src: Operand::X(0),
            fields: vec![
                (Operand::Atom(a), Operand::X(1)),
                (Operand::Atom(b), Operand::X(2)),
            ],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
    assert_eq!(machine.x(2), Some(make_small(22)));
}

#[test]
fn get_record_elements_id_branches_to_fail_on_an_absent_field() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordElementsId {
            fail: Label(7),
            id: Operand::Nil,
            src: Operand::X(0),
            fields: vec![(Operand::Atom(z), Operand::X(1))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_elements_id_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordElementsId {
            fail: NO_FAIL,
            id: Operand::Nil,
            src: Operand::X(0),
            fields: vec![(Operand::Atom(z), Operand::X(1))],
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: get_record_elements_id/no_fail");
}

// ---------------------------------------------------------------------------
// put_record (opcode 188)
// ---------------------------------------------------------------------------

#[test]
fn put_record_updates_a_field_and_returns_a_fresh_copy() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, b, _r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::PutRecord {
            fail: NO_FAIL,
            id: Operand::X(3), // the update path discards the Id (`emu/ops.tab:1653`)
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(a), Operand::Integer(9))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));

    let result = machine.x(0).unwrap();
    assert_eq!(field(fx.module(), result, a), Some(make_small(9)));
    assert_eq!(field(fx.module(), result, b), Some(make_small(22)));
    assert_ne!(result, rec, "the result is a fresh copy");
    // The source instance is not mutated.
    assert_eq!(field(fx.module(), rec, a), Some(make_small(11)));
}

#[test]
fn put_record_reads_every_update_before_writing_dst() {
    // `src == dst == X(0)` and an update value aliasing `X(0)`: the copy must embed
    // the *original* record, not a half-updated one.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, b, _r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::PutRecord {
            fail: NO_FAIL,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![
                (Operand::Atom(a), Operand::X(0)),
                (Operand::Atom(b), Operand::Integer(7)),
            ],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));

    let result = machine.x(0).unwrap();
    // `a` now holds the original record itself (the aliased value).
    let embedded = field(fx.module(), result, a).unwrap();
    assert_eq!(field(fx.module(), embedded, a), Some(make_small(11)));
    assert_eq!(field(fx.module(), embedded, b), Some(make_small(22)));
    assert_eq!(field(fx.module(), result, b), Some(make_small(7)));
}

#[test]
fn put_record_branches_to_fail_on_an_absent_update_field() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::PutRecord {
            fail: Label(7),
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(z), Operand::Integer(9))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
    assert_eq!(machine.x(0), Some(rec), "the destination is untouched on a fail");
}

#[test]
fn put_record_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::PutRecord {
            fail: NO_FAIL,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(z), Operand::Integer(9))],
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: put_record/no_fail");
}

#[test]
fn put_record_create_form_is_a_documented_capability_gap() {
    // `Src = nil` is the *create* form (`emu/ops.tab:1648` →
    // `i_create_native_record`), which resolves the compile-time record definition
    // through the code server's record table — a facility this runtime does not
    // have, so the family reports it rather than guessing a definition.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let mut machine = Machine::with_max_register(4);

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::PutRecord {
            fail: NO_FAIL,
            id: Operand::Atom(r),
            src: Operand::Nil,
            dst: Operand::X(0),
            live: 0,
            updates: vec![(Operand::Atom(a), Operand::Integer(1))],
        },
        &mut ctx,
    ));
    let _ = m;
    assert_eq!(
        reason,
        "unsupported_opcode: put_record: create (record definition table unavailable)"
    );
}

// ---------------------------------------------------------------------------
// is_record_accessible (opcode 189)
// ---------------------------------------------------------------------------

#[test]
fn is_record_accessible_external_scope_accepts_an_exported_record() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, external) = (
        fx.atom("m"),
        fx.atom("r"),
        fx.atom("a"),
        fx.atom("external"),
    );
    let rec = record(&mut ctx, m, r, true, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: NO_FAIL,
            src: Operand::X(0),
            scope: Operand::Atom(external),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
}

#[test]
fn is_record_accessible_external_scope_rejects_a_non_exported_record() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, external) = (
        fx.atom("m"),
        fx.atom("r"),
        fx.atom("a"),
        fx.atom("external"),
    );
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: Label(7),
            src: Operand::X(0),
            scope: Operand::Atom(external),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_record_accessible_wildcard_scope_accepts_a_record_created_in_that_module() {
    // Any scope atom other than `external` is the `$MODULE` wildcard: exported
    // **or** created in that module.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: NO_FAIL,
            src: Operand::X(0),
            scope: Operand::Atom(m),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
}

#[test]
fn is_record_accessible_wildcard_scope_rejects_a_record_from_another_module() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, other) = (fx.atom("m"), fx.atom("r"), fx.atom("a"), fx.atom("other"));
    // Created in `other`, not exported.
    let rec = record(&mut ctx, other, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: Label(7),
            src: Operand::X(0),
            scope: Operand::Atom(m),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn is_record_accessible_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a, external) = (
        fx.atom("m"),
        fx.atom("r"),
        fx.atom("a"),
        fx.atom("external"),
    );
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(1))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::IsRecordAccessible {
            fail: NO_FAIL,
            src: Operand::X(0),
            scope: Operand::Atom(external),
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: is_record_accessible/no_fail");
}

// ---------------------------------------------------------------------------
// get_record_field (opcode 190)
// ---------------------------------------------------------------------------

#[test]
fn get_record_field_local_form_reads_a_field_without_an_export_requirement() {
    // An atom `Id` is the `i_get_local_record_field` form: the record must be named
    // that atom in the running module, but need not be exported.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, _b, r) = two_field_record(&mut fx, &mut ctx);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordField {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
}

#[test]
fn get_record_field_local_form_with_the_underscore_name_skips_the_name_test() {
    // `am_Underscore` is the wildcard name: any record's field `a` is read.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let underscore = fx.atom("_");
    let rec = record(&mut ctx, m, r, false, &[(a, make_small(11))]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordField {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::Atom(underscore),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
}

#[test]
fn get_record_field_local_form_branches_to_fail_on_a_name_mismatch() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let other = fx.atom("other");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::Atom(other),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
    assert_eq!(machine.x(1), Some(NIL), "the destination is untouched on a fail");
}

#[test]
fn get_record_field_local_form_branches_to_fail_on_an_absent_field() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(z),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_branches_to_fail_on_a_non_record_source() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (a, r) = (fx.atom("a"), fx.atom("r"));
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_foreign_form_reads_an_exported_record_from_another_module() {
    // A `{Module, Name}` pair `Id` is the `i_get_record_field` form: the definition
    // must match that pair **and** be exported.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    let rec = record(&mut ctx, other, r, true, &[(a, make_small(11))]);
    let id = terms::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordField {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::X(3),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));
    assert_eq!(machine.x(1), Some(make_small(11)));
}

#[test]
fn get_record_field_foreign_form_branches_to_fail_on_a_non_exported_record() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    let rec = record(&mut ctx, other, r, false, &[(a, make_small(11))]);
    let id = terms::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::X(3),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_foreign_form_branches_to_fail_on_a_name_mismatch() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, s, a) = (fx.atom("other"), fx.atom("r"), fx.atom("s"), fx.atom("a"));
    // The record is named `s`, but the `Id` asks for `r`.
    let rec = record(&mut ctx, other, s, true, &[(a, make_small(11))]);
    let id = terms::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();
    machine.set_x(3, id).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::GetRecordField {
            fail: Label(7),
            src: Operand::X(0),
            id: Operand::X(3),
            field: Operand::Atom(a),
            dst: Operand::X(1),
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

#[test]
fn get_record_field_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::GetRecordField {
            fail: NO_FAIL,
            src: Operand::X(0),
            id: Operand::Atom(r),
            field: Operand::Atom(z),
            dst: Operand::X(1),
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: get_record_field/no_fail");
}

// ---------------------------------------------------------------------------
// update_record_id (opcode 193)
// ---------------------------------------------------------------------------

#[test]
fn update_record_id_updates_a_field_and_ignores_the_hint() {
    // `emu/ops.tab:1689` lowers `update_record_id Fail _Hint _Id Src Dst Live …` to
    // `i_update_native_record`: same update path as `put_record`, reuse hint ignored
    // (M1 never updates in place).
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, a, b, _r) = two_field_record(&mut fx, &mut ctx);
    let hint = fx.atom("inplace");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::UpdateRecordId {
            fail: NO_FAIL,
            hint: Operand::Atom(hint),
            id: Operand::X(3),
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(a), Operand::Integer(9))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Next));

    let result = machine.x(0).unwrap();
    assert_eq!(field(fx.module(), result, a), Some(make_small(9)));
    assert_eq!(field(fx.module(), result, b), Some(make_small(22)));
    assert_ne!(result, rec, "the result is a fresh copy");
}

#[test]
fn update_record_id_branches_to_fail_on_an_absent_update_field() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let reuse = fx.atom("reuse");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::UpdateRecordId {
            fail: Label(7),
            hint: Operand::Atom(reuse),
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(z), Operand::Integer(9))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
    assert_eq!(machine.x(0), Some(rec), "the destination is untouched on a fail");
}

#[test]
fn update_record_id_with_no_fail_is_a_capability_gap() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = unsupported_reason(run(
        fx.module(),
        &BTreeMap::new(),
        &mut machine,
        &Instr::UpdateRecordId {
            fail: NO_FAIL,
            hint: Operand::Nil,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(z), Operand::Integer(9))],
        },
        &mut ctx,
    ));
    assert_eq!(reason, "unsupported_opcode: update_record_id/no_fail");
}

#[test]
fn update_record_id_branches_to_fail_on_a_non_record_source() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let a = fx.atom("a");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, make_small(5)).unwrap();

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &Instr::UpdateRecordId {
            fail: Label(7),
            hint: Operand::Nil,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(a), Operand::Integer(9))],
        },
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}

// ---------------------------------------------------------------------------
// Family membership
// ---------------------------------------------------------------------------

#[test]
fn a_variant_outside_the_records_family_is_declined() {
    let fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let mut machine = Machine::with_max_register(4);

    assert_eq!(
        run(
            fx.module(),
            &BTreeMap::new(),
            &mut machine,
            &Instr::Return,
            &mut ctx
        )
        .unwrap(),
        None
    );
}
