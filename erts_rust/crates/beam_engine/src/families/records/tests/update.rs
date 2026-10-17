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

//! `put_record` (opcode 188) and `update_record_id` (opcode 193) — the two
//! native-record **update** paths.
//!
//! Both lower to `i_update_native_record` (`emu/ops.tab:1653`/`:1689`): the
//! instance is rebuilt with the listed fields replaced, every update value is
//! read before `dst` is written, and the `Id`/`Hint` operands are discarded. The
//! create form of `put_record` (`Src = nil`) resolves the definition out of the
//! running module's own `Recs` table; an id naming **another** module's record (or
//! a name the module does not define) stays a documented typed `unsupported`.
//!
//! The create form's semantics get the full structural coverage (defaults,
//! overrides, `badfield`/`novalue`) in the integration suite `tests/records.rs`,
//! which can load the committed `records_fixture.beam` and read its definitions.

use crate::interpreter::LabelMap;

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
        &LabelMap::new(),
        &mut machine,
        &Instr::PutRecord(Box::new(PutRecordArgs {
            fail: NO_FAIL,
            id: Operand::X(3), // the update path discards the Id (`emu/ops.tab:1653`)
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(a), Operand::Integer(9))],
        })),
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
        &LabelMap::new(),
        &mut machine,
        &Instr::PutRecord(Box::new(PutRecordArgs {
            fail: NO_FAIL,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![
                (Operand::Atom(a), Operand::X(0)),
                (Operand::Atom(b), Operand::Integer(7)),
            ],
        })),
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
        &Instr::PutRecord(Box::new(PutRecordArgs {
            fail: Label(7),
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(z), Operand::Integer(9))],
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
    assert_eq!(machine.x(0), Some(rec), "the destination is untouched on a fail");
}

#[test]
fn put_record_with_no_fail_raises_function_clause() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    fx.atom("function_clause");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &Instr::PutRecord(Box::new(PutRecordArgs {
                fail: NO_FAIL,
                id: Operand::Nil,
                src: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                updates: vec![(Operand::Atom(z), Operand::Integer(9))],
            })),
            &mut ctx,
        ),
    );
    assert_eq!(reason, "function_clause");
}

#[test]
fn put_record_create_with_an_unknown_record_name_is_a_typed_error() {
    // `Src = nil` is the *create* form (`emu/ops.tab:1648` →
    // `i_create_native_record`). The definition comes from the running module's own
    // `Recs` table (see `tests/records.rs` for its full semantics); a name the
    // module does not define is reported rather than guessed.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (m, r, a) = (fx.atom("m"), fx.atom("r"), fx.atom("a"));
    let mut machine = Machine::with_max_register(4);

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &Instr::PutRecord(Box::new(PutRecordArgs {
                fail: NO_FAIL,
                id: Operand::Atom(r),
                src: Operand::Nil,
                dst: Operand::X(0),
                live: 0,
                updates: vec![(Operand::Atom(a), Operand::Integer(1))],
            })),
            &mut ctx,
        ),
    );
    let _ = m;
    assert_eq!(
        reason,
        "unsupported_opcode: put_record: create: the module defines no record named 'r'"
    );
}

#[test]
fn put_record_create_for_another_module_is_a_typed_error() {
    // A `{Module,Name}` id whose module is not the running one needs the code
    // server's record table (`erts_record_find_entry`) or a `code:ensure_loaded/1`
    // trap (`erl_create_native_record_jit`) — a facility this runtime does not have,
    // so it is reported rather than guessed.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (r, a, other) = (fx.atom("r"), fx.atom("a"), fx.atom("other"));
    // `{other, r}` in X(1); the create form's `dst` is X(0).
    let id = crate::terms::test_support::make_tuple(&mut ctx, &[make_atom(other), make_atom(r)]).unwrap();
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).unwrap();

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &Instr::PutRecord(Box::new(PutRecordArgs {
                fail: NO_FAIL,
                id: Operand::X(1),
                src: Operand::Nil,
                dst: Operand::X(0),
                live: 0,
                updates: vec![(Operand::Atom(a), Operand::Integer(1))],
            })),
            &mut ctx,
        ),
    );
    assert_eq!(
        reason,
        "unsupported_opcode: put_record: create: no record definition table for another module"
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
        &LabelMap::new(),
        &mut machine,
        &Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
            fail: NO_FAIL,
            hint: Operand::Atom(hint),
            id: Operand::X(3),
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(a), Operand::Integer(9))],
        })),
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
        &Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
            fail: Label(7),
            hint: Operand::Atom(reuse),
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(z), Operand::Integer(9))],
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
    assert_eq!(machine.x(0), Some(rec), "the destination is untouched on a fail");
}

#[test]
fn update_record_id_with_no_fail_raises_function_clause() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (rec, _a, _b, _r) = two_field_record(&mut fx, &mut ctx);
    let z = fx.atom("z");
    fx.atom("function_clause");
    let mut machine = Machine::with_max_register(4);
    machine.set_x(0, rec).unwrap();

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
                fail: NO_FAIL,
                hint: Operand::Nil,
                id: Operand::Nil,
                src: Operand::X(0),
                dst: Operand::X(0),
                live: 1,
                updates: vec![(Operand::Atom(z), Operand::Integer(9))],
            })),
            &mut ctx,
        ),
    );
    assert_eq!(reason, "function_clause");
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
        &Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
            fail: Label(7),
            hint: Operand::Nil,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(0),
            live: 1,
            updates: vec![(Operand::Atom(a), Operand::Integer(9))],
        })),
        &mut ctx,
    )
    .unwrap();
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
}
