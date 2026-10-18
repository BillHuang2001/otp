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
//! The **tuple / list / record** data group.
//!
//! Owns the 14 data-structure variants:
//!
//! `GetTupleElement, SetTupleElement, GetList, GetHd, GetTl, UpdateRecord,
//! IsNativeRecord, PutRecord, IsRecordAccessible, GetRecordElements,
//! GetRecordField, GetRecordElementsId, UpdateRecordId, IsAnyNativeRecord`.
//!
//! Mechanic: seed the operand registers from a `setup` prologue built out of the
//! engine's own instructions — a real tuple (`put_tuple2`), a real cons cell
//! (`put_list`) and, for the native-record family, a real record instance built by
//! `put_record`'s **create** form over a `Module::records` definition the row
//! declares — then assert the resulting register or the documented `fail` branch.
//!
//! The eight native-record opcodes are in the census's pinned forward-compat set
//! (real compiler output never emits them for this toolchain), so the census
//! prints them as `ZERO-CORPUS` even though they execute; `IsAnyNativeRecord` is
//! emitted by real code and stays `EXECUTED`. Five of them (`GetRecordElements`,
//! `GetRecordField`, `GetRecordElementsId`, `UpdateRecordId`, `IsAnyNativeRecord`)
//! used to be measured only on their `fail` branch over a non-record source; every
//! row here drives the **success** path instead.
//!
//! Every record row walks the same definition, so the fixture lives in one place
//! ([`record_fixture`]) and the live instance is built by one helper
//! ([`create_record`]) — the create form is itself one of the rows
//! ([`put_record_row`]), so its success also proves the fixture the others use.

use beam_loader::{
    GetRecordFieldArgs, Instr, Operand, PutRecordArgs, RecordDefinition, RecordField,
    UpdateRecordArgs, UpdateRecordIdArgs,
};
use beam_term::make_small;

use super::super::{Expect, Probe, ProbeAtoms, FAIL_SINK_LABEL};

/// Every converted row of this group.
pub fn rows(atoms: &ProbeAtoms) -> Vec<Probe> {
    vec![
        get_tuple_element_row(),
        set_tuple_element_row(),
        get_list_row(),
        get_hd_row(),
        get_tl_row(),
        update_record_row(),
        is_native_record_row(atoms),
        put_record_row(atoms),
        is_record_accessible_row(atoms),
        get_record_elements_row(atoms),
        get_record_field_row(atoms),
        get_record_elements_id_row(atoms),
        update_record_id_row(atoms),
        is_any_native_record_row(atoms),
    ]
}

// ---------------------------------------------------------------------------
// The shared native-record fixture
// ---------------------------------------------------------------------------

/// `#r{a = 11, b = 22}` — the definition every record row's module declares.
///
/// Both fields carry a `Some(..)` default: a field the create form does not name
/// falls back to it, while a `None` default would be the file's *absent* sentinel
/// (`-record #r{f}`, the C's `u,0`) and make an unnamed field a `novalue` failure.
/// The record is **not** exported, which is what makes
/// [`is_record_accessible_row`]'s module-scoped accessibility a real check rather
/// than the trivially-true exported one.
fn record_fixture(atoms: &ProbeAtoms) -> RecordDefinition {
    RecordDefinition {
        name: atoms.named("r"),
        is_exported: false,
        fields: vec![
            RecordField {
                name: atoms.named("a"),
                default: Some(make_small(11)),
            },
            RecordField {
                name: atoms.named("b"),
                default: Some(make_small(22)),
            },
        ],
    }
}

/// The updates that name **both** fixture fields — the create form of
/// `#r{a = 1, b = 2}`, the live instance every record row starts from.
fn both_fields(atoms: &ProbeAtoms) -> Vec<(Operand, Operand)> {
    vec![
        (Operand::Atom(atoms.named("a")), Operand::Integer(1)),
        (Operand::Atom(atoms.named("b")), Operand::Integer(2)),
    ]
}

/// The **create** form of `put_record` (`Src` is the literal `nil`), building a
/// fresh instance from the running module's own `Recs` definition into `dst`.
///
/// `Id` is the record-name **atom** (`i_create_local_native_record`, the
/// compiler's local case); a `{Module,Name}` id naming *another* module is the
/// documented typed `unsupported` and is deliberately not probed here. `live` is
/// carried by the instruction but never consulted — the build reserves its block
/// with `RootSet::ALL_LIVE`, so the whole register file is the standing root set.
fn create_record(atoms: &ProbeAtoms, dst: Operand, updates: Vec<(Operand, Operand)>) -> Instr {
    Instr::PutRecord(Box::new(PutRecordArgs {
        fail: FAIL_SINK_LABEL,
        id: Operand::Atom(atoms.named("r")),
        src: Operand::Nil,
        dst,
        live: 3,
        updates,
    }))
}

/// A probe whose module carries the shared record fixture.
fn probe_with_record(atoms: &ProbeAtoms, instr: Instr, expect: Expect) -> Probe {
    Probe::new(instr, expect).records(vec![record_fixture(atoms)])
}

// ---------------------------------------------------------------------------
// Tuples and lists
// ---------------------------------------------------------------------------

/// `get_tuple_element Source Element Dst` — the success path over a real tuple.
///
/// `Element` is the 0-based index the C reads at `tuple_val(Source) + 1 + Element`,
/// so index 1 of `{7,8}` is `8`.
fn get_tuple_element_row() -> Probe {
    Probe::new(
        Instr::GetTupleElement {
            source: Operand::X(1),
            element: 1,
            dst: Operand::X(0),
        },
        Expect::Returned(Some(make_small(8))),
    )
    .setup(vec![Instr::PutTuple2 {
        dst: Operand::X(1),
        elements: vec![Operand::Integer(7), Operand::Integer(8)],
    }])
    .note("a real 2-tuple in x(1) built by put_tuple2; 0-based element 1 is 8")
}

/// `set_tuple_element Element Tuple Offset` — the **destructive** write.
///
/// The instruction stores `Element` in the tuple's own word `Offset` (a data index,
/// 0 being the first element), so the very object built by the prologue changes:
/// `{1,2}` becomes `{99,2}` and the run returns that tuple out of `x(0)`. The only
/// rejected target is a tuple in the module's immutable literal area (a typed
/// `unsupported`), never a fresh heap tuple.
fn set_tuple_element_row() -> Probe {
    Probe::new(
        Instr::SetTupleElement {
            element: Operand::X(1),
            tuple: Operand::X(0),
            offset: 0,
        },
        Expect::Returned(None),
    )
    .setup(vec![
        Instr::PutTuple2 {
            dst: Operand::X(0),
            elements: vec![Operand::Integer(1), Operand::Integer(2)],
        },
        Instr::Move {
            src: Operand::Integer(99),
            dst: Operand::X(1),
        },
    ])
    .note(
        "the heap tuple {1,2} in x(0) is updated in place to {99,2}; a literal-area tuple is the only rejected target",
    )
}

/// `get_list Source Head Tail` — split a real cons cell.
fn get_list_row() -> Probe {
    Probe::new(
        Instr::GetList {
            source: Operand::X(1),
            head: Operand::X(0),
            tail: Operand::X(2),
        },
        Expect::Returned(Some(make_small(1))),
    )
    .setup(vec![Instr::PutList {
        head: Operand::Integer(1),
        tail: Operand::Nil,
        dst: Operand::X(1),
    }])
    .note("a real cons cell [1] built by put_list; get_list splits it, head 1 landing in x(0)")
}

/// `get_hd Source Head` — the car of a real cons cell.
fn get_hd_row() -> Probe {
    Probe::new(
        Instr::GetHd {
            source: Operand::X(1),
            head: Operand::X(0),
        },
        Expect::Returned(Some(make_small(1))),
    )
    .setup(vec![Instr::PutList {
        head: Operand::Integer(1),
        tail: Operand::Nil,
        dst: Operand::X(1),
    }])
    .note("a real cons cell [1] built by put_list; get_hd reads its head 1 into x(0)")
}

/// `get_tl Source Tail` — the cdr of a real two-cell list.
///
/// The prologue builds `[2]` and then `[1|[2]]`, i.e. `[1,2]`; the tail of the
/// outer cell is the inner *list* `[2]` (not the integer `2`), which `x(0)` returns.
fn get_tl_row() -> Probe {
    Probe::new(
        Instr::GetTl {
            source: Operand::X(1),
            tail: Operand::X(0),
        },
        Expect::Returned(None),
    )
    .setup(vec![
        Instr::PutList {
            head: Operand::Integer(2),
            tail: Operand::Nil,
            dst: Operand::X(1),
        },
        Instr::PutList {
            head: Operand::Integer(1),
            tail: Operand::X(1),
            dst: Operand::X(1),
        },
    ])
    .note("the two-cell list [1,2] in x(1); get_tl yields its tail [2] in x(0)")
}

/// `update_record Hint Size Src Dst {Index,Value}*` — the OTP-26 tagged-tuple copy.
///
/// The update index is **1-based** (`HTOP[$Offset]` with `HTOP[0]` the arityval
/// header), so `{2, 9}` replaces element 1: `{1,2}` becomes the fresh copy `{1,9}`.
/// The reuse `Hint` is intentionally `nil` — M1 never updates in place, so the
/// result is always a copy.
fn update_record_row() -> Probe {
    Probe::new(
        Instr::UpdateRecord(Box::new(UpdateRecordArgs {
            hint: Operand::Nil,
            size: 2,
            src: Operand::X(0),
            dst: Operand::X(0),
            updates: vec![(Operand::Integer(2), Operand::Integer(9))],
        })),
        Expect::Returned(None),
    )
    .setup(vec![Instr::PutTuple2 {
        dst: Operand::X(0),
        elements: vec![Operand::Integer(1), Operand::Integer(2)],
    }])
    .note("update index 2 is 1-based, so element 1 becomes 9 and the fresh copy {1,9} is returned")
}

// ---------------------------------------------------------------------------
// The native-record family (the live instance is built by the create form)
// ---------------------------------------------------------------------------

/// `is_native_record Fail Src Mod Name` — the owner + name guard, **matching**.
fn is_native_record_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        Instr::IsNativeRecord {
            fail: FAIL_SINK_LABEL,
            src: Operand::X(0),
            module: Operand::Atom(atoms.module()),
            name: Operand::Atom(atoms.named("r")),
        },
        Expect::Returned(None),
    )
    .setup(vec![create_record(atoms, Operand::X(0), both_fields(atoms))])
    .note(
        "the live record's definition is (probe, r), so the guard falls through to the success sink instead of its fail branch",
    )
}

/// `put_record Fail Id Src Dst Live {FieldName,Value}*` — the **create** form.
///
/// Only `a` is named, so `b` takes its `Some(22)` default (an absent default would
/// be the `novalue` failure) and the definition's cached hash word is computed and
/// stored, which is what makes the resulting term a well-formed record the sibling
/// rows can consume.
fn put_record_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        create_record(
            atoms,
            Operand::X(0),
            vec![(Operand::Atom(atoms.named("a")), Operand::Integer(1))],
        ),
        Expect::Returned(None),
    )
    .note(
        "the create form (Src = nil) builds #r{a = 1, b = 22}: the unnamed field b takes its Some(22) default, and the definition hash is stored",
    )
}

/// `is_record_accessible Fail Src Scope` — the module-scoped (wildcard) form.
///
/// A `Scope` that is not `external` is the running module
/// (`i_is_wildcard_record_accessible`), which succeeds for a record created there
/// even when its definition is **not** exported.
fn is_record_accessible_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        Instr::IsRecordAccessible {
            fail: FAIL_SINK_LABEL,
            src: Operand::X(0),
            scope: Operand::Atom(atoms.module()),
        },
        Expect::Returned(None),
    )
    .setup(vec![create_record(atoms, Operand::X(0), both_fields(atoms))])
    .note(
        "scope = the running module: accessible because the record was created there, even though the definition is not exported",
    )
}

/// `get_record_elements Fail Src {FieldName,Register}*` — the success path.
///
/// Field `a` (`1`) is read into `x(1)` and the instruction falls through; it used to
/// be measured only on its `fail` branch over a non-record source.
fn get_record_elements_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        Instr::GetRecordElements {
            fail: FAIL_SINK_LABEL,
            src: Operand::X(0),
            fields: vec![(Operand::Atom(atoms.named("a")), Operand::X(1))],
        },
        Expect::Returned(None),
    )
    .setup(vec![create_record(atoms, Operand::X(0), both_fields(atoms))])
    .note("field a (= 1) is read out of the live record into x(1) and the instruction falls through")
}

/// `get_record_field Fail Src Id Field Dst` — the **local** (`Id` an atom) form.
fn get_record_field_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        Instr::GetRecordField(Box::new(GetRecordFieldArgs {
            fail: FAIL_SINK_LABEL,
            src: Operand::X(0),
            id: Operand::Atom(atoms.named("r")),
            field: Operand::Atom(atoms.named("a")),
            dst: Operand::X(1),
        })),
        Expect::Returned(None),
    )
    .setup(vec![create_record(atoms, Operand::X(0), both_fields(atoms))])
    .note("the local form (Id = the record-name atom r) reads field a (= 1) into x(1)")
}

/// `get_record_elements_id Fail Id Src {FieldName,Register}*` — the "anonymous
/// match" form, which shares `get_record_elements`' implementation.
///
/// The `Id` operand is **discarded** by the loader transform
/// (`get_record_elements_id Fail _Id Src … => i_get_record_elements Fail Src …`), so
/// it is spelled as the record-name atom here purely for shape.
fn get_record_elements_id_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        Instr::GetRecordElementsId {
            fail: FAIL_SINK_LABEL,
            id: Operand::Atom(atoms.named("r")),
            src: Operand::X(0),
            fields: vec![(Operand::Atom(atoms.named("a")), Operand::X(1))],
        },
        Expect::Returned(None),
    )
    .setup(vec![create_record(atoms, Operand::X(0), both_fields(atoms))])
    .note("Id is ignored (it lowers to get_record_elements), so field a (= 1) reaches x(1) and the row falls through")
}

/// `update_record_id Fail Hint Id Src Dst Live {FieldName,Value}*` — the OTP-30
/// update, which shares `put_record`'s update path with `Hint` and `Id` discarded.
///
/// The result is a fresh copy with field `a` replaced by `42` (M1 never updates in
/// place); `x(0)` still holds the untouched source, which is what the run returns.
fn update_record_id_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        Instr::UpdateRecordId(Box::new(UpdateRecordIdArgs {
            fail: FAIL_SINK_LABEL,
            hint: Operand::Nil,
            id: Operand::Nil,
            src: Operand::X(0),
            dst: Operand::X(1),
            live: 0,
            updates: vec![(Operand::Atom(atoms.named("a")), Operand::Integer(42))],
        })),
        Expect::Returned(None),
    )
    .setup(vec![create_record(atoms, Operand::X(0), both_fields(atoms))])
    .note("Hint and Id are discarded; the copy with a = 42 lands in x(1) and the row falls through")
}

/// `is_any_native_record Fail Src` — the `erlang:is_record/1` guard over a live
/// record, i.e. the fall-through rather than the previous rows' `fail` branch.
fn is_any_native_record_row(atoms: &ProbeAtoms) -> Probe {
    probe_with_record(
        atoms,
        Instr::IsAnyNativeRecord {
            fail: FAIL_SINK_LABEL,
            src: Operand::X(0),
        },
        Expect::Returned(None),
    )
    .setup(vec![create_record(atoms, Operand::X(0), both_fields(atoms))])
    .note("a live native record in x(0), so the guard falls through to the success sink")
}
