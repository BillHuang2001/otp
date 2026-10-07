//! `put_record` (opcode 188) and `update_record_id` (opcode 193) — the two
//! native-record **update** paths.
//!
//! Both lower to `i_update_native_record` (`emu/ops.tab:1653`/`:1689`): the
//! instance is rebuilt with the listed fields replaced, every update value is
//! read before `dst` is written, and the `Id`/`Hint` operands are discarded. The
//! create form of `put_record` (`Src = nil`) needs the code server's record table
//! and is therefore a documented capability gap.

use std::collections::BTreeMap;

use super::*;

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
