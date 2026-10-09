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

//! **Moving-collector regression tests for the two native-record build paths**
//! (Handoff #11).
//!
//! [`create_native_record`](super::create_native_record) and
//! [`update_native_record`](super::update_native_record) were restructured around the
//! frozen **reserve, then read, then write** ordering: both now take ONE rooted
//! reservation *before* they read the record's field values, because the reserve can
//! run a **moving** collector
//! ([`ExecutionContext::alloc_words_rooted`](crate::context::ExecutionContext::alloc_words_rooted))
//! that relocates every block reachable from the machine's root set and rewrites
//! `machine.x`/`stack`/`fr` in place. Reading a value into a Rust local before the
//! reserve would leave a *stale* (poisoned) pointer in the freshly built record.
//!
//! These tests drive both instructions over the crate's mock semi-space collector
//! ([`MovingContext`](crate::terms::alloc_tests::MovingContext)) with the field
//! **values parked in the root set** (heap floats the collector relocates). Each test
//! asserts three things at once:
//!
//! 1. the collector really ran **and moved** the roots
//!    ([`MovingContext::moved_any`](crate::terms::alloc_tests::MovingContext::moved_any)
//!    plus a raw-address comparison before/after) — so the test is not vacuous;
//! 2. the built record's field **is** the machine's post-collection root (so the
//!    value was read *after* the reserve);
//! 3. the pre-move address no longer reads as the value it held (the collector
//!    poisoned the vacated region) — the negative control that makes assertion 2
//!    meaningful.
//!
//! The fixtures are hand-built modules and hand-carved record blocks (through the
//! family's own `raw` writers); the instructions are hand-built [`Instr`]s driven
//! straight through [`exec`]. No loader, no oracle.

use beam_loader::{
    Instr, Label, Module, Operand, PutRecordArgs, RecordDefinition, RecordField, UpdateRecordIdArgs,
    NO_FAIL,
};
use beam_term::{make_atom, make_small, Atom, Eterm};

use crate::context::{ExecutionContext, RootSet};
use crate::families::records::{exec, raw};
use crate::interpreter::LabelMap;
use crate::machine::Machine;
use crate::terms::alloc_tests::MovingContext;
use crate::terms::{float_value, make_float, reserve};
use crate::Step;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A module `m` with the two-field record `r{a, b}` defined in it, returning
/// `(module, r, a, b)`.
///
/// The definition is what the *create* form resolves out of the running module's own
/// `Recs` table; the *update* form never consults it (the source instance carries its
/// own definition).
fn module_with_record() -> (Module, Atom, Atom, Atom) {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let (a, b, r) = (
        module.atom_table.intern("a"),
        module.atom_table.intern("b"),
        module.atom_table.intern("r"),
    );
    module.records.push(RecordDefinition {
        name: r,
        is_exported: false,
        fields: vec![
            RecordField {
                name: a,
                default: Some(make_small(11)),
            },
            RecordField {
                name: b,
                default: Some(make_small(22)),
            },
        ],
    });
    (module, r, a, b)
}

/// Carve a native-record instance with `fields` into the moving region (one rooted
/// reservation, then the family's own `raw::write_record`) and return the tagged
/// **instance** root.
///
/// The caller must root the result in the machine before the next collecting
/// allocation, exactly as a real `put_record` leaves it in its destination register.
fn carve_record(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    owner: Atom,
    name: Atom,
    fields: &[(Atom, Eterm)],
) -> Eterm {
    let fields: Vec<(Eterm, Eterm)> = fields
        .iter()
        .map(|(k, v)| (make_atom(*k), *v))
        .collect();
    let hp = reserve(
        machine,
        ctx,
        beam_term::record_block_words(fields.len()),
        RootSet::ALL_LIVE,
    )
    .expect("rooted reserve");
    raw::write_record(hp, owner, name, false, &fields, make_small(0)).expect("record block")
}

/// The value of the field keyed `key` in `rec`, or `None`.
fn field(module: &Module, rec: Eterm, key: Atom) -> Option<Eterm> {
    raw::record_get(rec, key, &module.atom_table).expect("a valid record")
}

/// Drive one instruction through the records family over `machine`/`ctx`, asserting
/// the family claims it and reports no raise.
fn run(
    module: &Module,
    machine: &mut Machine,
    instr: &Instr,
    ctx: &mut dyn ExecutionContext,
) -> Step {
    exec(machine, module, &LabelMap::new(), instr, 0, 0, ctx)
        .expect("the records family reports no raise")
        .expect("the records family claims the variant")
}

// ---------------------------------------------------------------------------
// The create form
// ---------------------------------------------------------------------------

/// `put_record`'s create form reads every update value **after** its single rooted
/// reserve, so a collection that relocates the value roots cannot leave a stale
/// pointer inside the freshly built record.
#[test]
fn create_native_record_reads_its_update_values_after_the_reserve() {
    let (module, r, a, b) = module_with_record();
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    // The two update values are heap floats parked in the root set: the reserve runs
    // a collection that relocates both.
    let va = make_float(&mut machine, &mut ctx, 1.5).expect("float a");
    machine.set_x(1, va).expect("root the value");
    let vb = make_float(&mut machine, &mut ctx, 2.5).expect("float b");
    machine.set_x(2, vb).expect("root the value");
    let before = [
        machine.x(1).expect("x(1)").raw(),
        machine.x(2).expect("x(2)").raw(),
    ];

    let step = run(
        &module,
        &mut machine,
        &Instr::PutRecord(Box::new(PutRecordArgs {
            fail: NO_FAIL,
            id: Operand::Atom(r),
            src: Operand::Nil,
            dst: Operand::X(0),
            live: 3,
            updates: vec![
                (Operand::Atom(a), Operand::X(1)),
                (Operand::Atom(b), Operand::X(2)),
            ],
        })),
        &mut ctx,
    );
    assert_eq!(step, Step::Next);

    // (1) The collector ran and relocated both value roots — the test is not vacuous.
    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    assert!(ctx.collections >= 1, "a collection must have run");
    let after = [
        machine.x(1).expect("x(1)").raw(),
        machine.x(2).expect("x(2)").raw(),
    ];
    assert_ne!(after, before, "the reserve must have moved the value roots");

    // (2) Each field IS the machine's post-collection root, and holds the right value.
    let record = machine.x(0).expect("the created record");
    let fa = field(&module, record, a).expect("field a");
    let fb = field(&module, record, b).expect("field b");
    assert_eq!(fa, machine.x(1).expect("x(1)"), "field a is the moved root");
    assert_eq!(fb, machine.x(2).expect("x(2)"), "field b is the moved root");
    assert_eq!(float_value(fa).expect("float"), 1.5);
    assert_eq!(float_value(fb).expect("float"), 2.5);

    // (3) Negative control: the pre-move addresses were poisoned by the collection, so
    // assertion (2) is only satisfiable by a builder that read *after* the reserve.
    assert!(
        float_value(Eterm::from_raw(before[0])).is_err(),
        "the pre-move address must no longer read as a float"
    );
    assert!(
        float_value(Eterm::from_raw(before[1])).is_err(),
        "the pre-move address must no longer read as a float"
    );
}

/// The create form still applies a field's **default** when the update does not name
/// it, across the reserve — the defaults are immediates/literal-area terms, which no
/// collection relocates.
#[test]
fn create_native_record_keeps_an_unupdated_field_default_across_the_reserve() {
    let (module, r, a, b) = module_with_record();
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    let va = make_float(&mut machine, &mut ctx, 1.5).expect("float a");
    machine.set_x(1, va).expect("root the value");
    let before = machine.x(1).expect("x(1)").raw();

    // Only `a` is updated; `b` must take its definition default (`22`).
    let step = run(
        &module,
        &mut machine,
        &Instr::PutRecord(Box::new(PutRecordArgs {
            fail: NO_FAIL,
            id: Operand::Atom(r),
            src: Operand::Nil,
            dst: Operand::X(0),
            live: 2,
            updates: vec![(Operand::Atom(a), Operand::X(1))],
        })),
        &mut ctx,
    );
    assert_eq!(step, Step::Next);
    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    assert_ne!(
        machine.x(1).expect("x(1)").raw(),
        before,
        "the reserve must have moved the value root"
    );

    let record = machine.x(0).expect("the created record");
    assert_eq!(field(&module, record, a), Some(machine.x(1).expect("x(1)")));
    assert_eq!(field(&module, record, b), Some(make_small(22)));
}

// ---------------------------------------------------------------------------
// The update form
// ---------------------------------------------------------------------------

/// `update_record_id` (the `i_update_native_record` path `put_record`'s update form
/// shares) reads the **source record** and the update values after its single rooted
/// reserve, so the copy is built from post-collection terms.
#[test]
fn update_native_record_reads_the_source_and_its_values_after_the_reserve() {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let (a, b, r) = (
        module.atom_table.intern("a"),
        module.atom_table.intern("b"),
        module.atom_table.intern("r"),
    );
    let owner = module.name;
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    // The source record lives in the moving region, rooted at x(0).
    let source = carve_record(
        &mut machine,
        &mut ctx,
        owner,
        r,
        &[(b, make_small(22)), (a, make_small(11))],
    );
    machine.set_x(0, source).expect("root the source");

    // An update value the reserve's collection will relocate (rooted at x(1)).
    let value = make_float(&mut machine, &mut ctx, 2.5).expect("float");
    machine.set_x(1, value).expect("root the value");

    let source_before = machine.x(0).expect("x(0)").raw();
    let value_before = machine.x(1).expect("x(1)").raw();

    let step = run(
        &module,
        &mut machine,
        &Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
            fail: NO_FAIL,
            hint: Operand::Nil,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(2),
            live: 2,
            updates: vec![(Operand::Atom(a), Operand::X(1))],
        })),
        &mut ctx,
    );
    assert_eq!(step, Step::Next);

    // (1) The collector ran and relocated both the source record and the value root.
    assert!(ctx.moved_any, "the mock collector must have relocated a block");
    assert_ne!(
        machine.x(0).expect("x(0)").raw(),
        source_before,
        "the reserve must have moved the source record"
    );
    assert_ne!(
        machine.x(1).expect("x(1)").raw(),
        value_before,
        "the reserve must have moved the update value"
    );

    // (2) The copy carries the moved value and the source's untouched field.
    let record = machine.x(2).expect("the updated record");
    assert_eq!(
        field(&module, record, a),
        Some(machine.x(1).expect("x(1)")),
        "the updated field is the moved value root"
    );
    assert_eq!(
        float_value(field(&module, record, a).expect("field a")).expect("float"),
        2.5
    );
    assert_eq!(
        field(&module, record, b),
        Some(make_small(22)),
        "an unmentioned field keeps the source value"
    );
    // The source's own definition survived its copy (it is read from the moved block).
    assert_eq!(
        field(&module, machine.x(0).expect("x(0)"), a),
        Some(make_small(11))
    );

    // (3) Negative control: the pre-move value address was poisoned.
    assert!(
        float_value(Eterm::from_raw(value_before)).is_err(),
        "the pre-move address must no longer read as a float"
    );
}

/// The update path branches to `fail` for a field the record does not have — even
/// though the reserve already collected (the failure is a label jump, and `dst` keeps
/// its pre-instruction value).
#[test]
fn update_native_record_branches_to_fail_after_the_reserve() {
    let mut module = Module::default();
    module.name = module.atom_table.intern("m");
    let (a, b, r, z) = (
        module.atom_table.intern("a"),
        module.atom_table.intern("b"),
        module.atom_table.intern("r"),
        module.atom_table.intern("z"),
    );
    let owner = module.name;
    let mut machine = Machine::with_max_register(4);
    let mut ctx = MovingContext::new();

    let source = carve_record(
        &mut machine,
        &mut ctx,
        owner,
        r,
        &[(b, make_small(22)), (a, make_small(11))],
    );
    machine.set_x(0, source).expect("root the source");
    let marker = make_small(-7);
    machine.set_x(2, marker).expect("dst");

    let step = exec(
        &mut machine,
        &module,
        &LabelMap::from([(Label(7), (0, 4))]),
        &Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
            fail: Label(7),
            hint: Operand::Nil,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(2),
            live: 1,
            updates: vec![(Operand::Atom(z), Operand::Integer(9))],
        })),
        0,
        0,
        &mut ctx,
    )
    .expect("no raise")
    .expect("the records family claims the variant");
    assert_eq!(step, Step::Goto { func: 0, pc: 4 });
    assert_eq!(
        machine.x(2).expect("dst"),
        marker,
        "the destination is untouched on a fail"
    );
    assert!(
        ctx.collections >= 1,
        "the reserve still collected before the fail"
    );
}
