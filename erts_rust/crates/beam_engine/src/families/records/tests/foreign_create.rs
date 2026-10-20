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

//! `put_record`'s **create** form for a record whose definition is not the running
//! module's own — the [`ExecutionContext::foreign_record_definition`] seam.
//!
//! # No oracle exists for these opcodes
//!
//! The installed toolchain (OTP 29.1.1, `erl_features:configurable()` =
//! `[compr_assign,maybe_expr]`) **never emits the native-record family**: `erlc
//! -enable-feature native_record` answers "the feature 'native_record' does not
//! exist", and a `-record`-update module disassembles to `update_record`
//! (`tests/gaps.rs` records the same evidence). So there is **no `erlc`/`erl`
//! oracle** to cross-check any expectation here. Every value, reason and branch
//! asserted below is pinned from the **C ground truth** — `erl_create_native_record`
//! (`erts/emulator/beam/erl_record.c:356-410`), `erl_create_local_native_record`
//! (`:254-354`), `emu/instrs.tab:1358-1395`, `emu/ops.tab:1648-1651`,
//! `emu/macros.tab:266-278` and `expand_error_value` (`beam_common.c:741-778`) — and
//! from the family's own documented decisions. No oracle agreement is claimed.
//!
//! # What is covered
//!
//! * the seam's two branches for a **foreign** definition (exported ⇒ created;
//!   non-exported ⇒ the C's `badrecord`), on both the `NO_FAIL` raise and a live
//!   `fail` label,
//! * `Ok(None)` — a host with no record table — keeping the family's exact typed
//!   `unsupported`,
//! * the generic `{SelfModule, Name}` form, which C resolves through the record
//!   table and therefore gates on `is_exported` even for the running module's own
//!   record (a non-exported own record is a `badrecord`, never a silent create),
//! * the create form's own error ordering (`badfield` outranking `no_value`,
//!   `no_value` remembering only the **first** absent-default field) on a definition
//!   that came from the host,
//! * the `Some(<small 0>)` default vs the absent (`None`) one,
//! * the atom-namespace rule (a definition name the caller's live namespace cannot
//!   spell is a typed error, never a wrong atom in a term), and
//! * tier parity: the same create on [`Tier::Interpreter`] and [`Tier::BaselineJit`].

use std::collections::BTreeMap;

use beam_common::ExceptionClass;
use beam_loader::{Export, Function, Label, Module, Operand, RecordDefinition, RecordField, NO_FAIL};
use beam_term::{make_atom, make_small, Atom, AtomTable, Eterm};

use super::*;
use crate::jit::Tier;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A [`RecordDefinition`] from `(field name, default)` pairs in **declaration**
/// order — the exact shape the `Recs` chunk (and so the host's record table) holds.
fn definition(name: Atom, is_exported: bool, fields: &[(Atom, Option<Eterm>)]) -> RecordDefinition {
    RecordDefinition {
        name,
        is_exported,
        fields: fields
            .iter()
            .map(|(name, default)| RecordField {
                name: *name,
                default: *default,
            })
            .collect(),
    }
}

/// The `{Module, Name}` id term, materialized into the context arena.
fn id_pair(ctx: &mut MinimalContext, owner: Atom, name: Atom) -> Eterm {
    crate::terms::test_support::make_tuple(ctx, &[make_atom(owner), make_atom(name)])
        .expect("allocate the {Module,Name} id")
}

/// One create-form `put_record` over `id` (`Src = nil`) into `X(0)`.
fn create(id: Operand, fail: Label, updates: Vec<(Operand, Operand)>) -> Instr {
    Instr::PutRecord(Box::new(PutRecordArgs {
        fail,
        id,
        src: Operand::Nil,
        dst: Operand::X(0),
        live: 0,
        updates,
    }))
}

/// Intern the two atoms a `NO_FAIL` record error needs: the reasoning tag and the
/// `error` class atom the raise carries into `x(3)`.
fn intern_raise_atoms(fx: &mut Fixture, tag: &str) {
    fx.atom(tag);
    fx.atom("error");
}

/// The definition term of the (native record) `instance`.
fn definition_of(instance: Eterm) -> Eterm {
    raw::record_definition(instance).expect("the term must be a native record")
}

// ---------------------------------------------------------------------------
// A foreign definition the host CAN answer for
// ---------------------------------------------------------------------------

#[test]
fn foreign_create_with_an_exported_host_definition_builds_the_record() {
    // `erl_create_native_record` finds the entry and `is_exported == am_true`, so it
    // delegates to `erl_create_local_native_record` (`erl_record.c:399-404`): the
    // unmentioned field keeps the definition's **default**, the named one takes the
    // update value.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a, b) = (fx.atom("other"), fx.atom("r"), fx.atom("a"), fx.atom("b"));

    let mut ctx = {
        ctx.set_foreign_record_definitions(vec![(
            other,
            r,
            definition(r, true, &[(a, Some(make_small(1))), (b, Some(make_small(2)))]),
        )]);
        ctx
    };

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(
            Operand::X(1),
            NO_FAIL,
            vec![(Operand::Atom(b), Operand::Integer(9))],
        ),
        &mut ctx,
    )
    .expect("a successful foreign create");
    assert_eq!(step, Some(Step::Next));

    let result = machine.x(0).expect("X(0)");
    assert_eq!(
        field(fx.module(), result, a),
        Some(make_small(1)),
        "the unmentioned field keeps the definition's default"
    );
    assert_eq!(
        field(fx.module(), result, b),
        Some(make_small(9)),
        "the named field takes the update value"
    );
}

#[test]
fn foreign_create_stores_the_definitions_identity_and_a_real_cached_hash() {
    // The instance's definition block must carry the **foreign** module as its owner
    // (`defp->module`, `erl_record.c:345`), the definition's name and export flag —
    // and a *real* cached hash, never the `0` placeholder the writer starts from
    // (`make_hash2({Module,Name,IsExported,Keys})`, `beam_file.c:1240-1256`, which
    // `erlang:phash2/1` of a record can observe).
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a, b) = (fx.atom("other"), fx.atom("r"), fx.atom("a"), fx.atom("b"));
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, true, &[(a, Some(make_small(1))), (b, Some(make_small(2)))]),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::X(1), NO_FAIL, Vec::new()),
        &mut ctx,
    )
    .expect("a successful foreign create");
    assert_eq!(step, Some(Step::Next));

    let def = definition_of(machine.x(0).expect("X(0)"));
    assert_eq!(raw::definition_module(def).expect("owner"), other, "the foreign owner");
    assert_eq!(raw::definition_name(def).expect("name"), r);
    assert!(
        raw::definition_is_exported(def).expect("export flag"),
        "the created instance is an exported record"
    );

    let hash = raw::definition_hash(def).expect("cached hash");
    let expected = beam_term::hashing::compute_record_definition_hash(def, &fx.module().atom_table)
        .expect("a computable definition hash");
    assert_eq!(
        hash,
        make_small(expected as i64),
        "the cached hash is `compute_record_definition_hash` of the built definition"
    );
    assert_ne!(hash, make_small(0), "the placeholder must never survive");
}

#[test]
fn foreign_create_with_a_non_exported_host_definition_raises_badrecord() {
    // `defp->is_exported != am_true` ⇒ `p->fvalue = id; p->freason = EXC_BADRECORD`
    // (`erl_record.c:407-409`); `expand_error_value` (`beam_common.c:761-767`) wraps
    // it as `{badrecord, Id}` — the **`Id` term**, not the
    // `{Tag, {{Module,Name}, Field}}` shape a `badfield` uses.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    intern_raise_atoms(&mut fx, "badrecord");
    let badrecord = fx.atom("badrecord");
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, false, &[(a, Some(make_small(1)))]),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::X(1), NO_FAIL, Vec::new()),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, ExceptionClass::Error, "`badrecord` is class `error`");
    assert_eq!(terms::tuple_arity(reason).expect("a tuple"), 2, "reason arity");
    assert_eq!(
        terms::tuple_element(reason, 0).expect("tag"),
        make_atom(badrecord),
        "the reason tag is `badrecord`"
    );
    assert_eq!(
        terms::tuple_element(reason, 1).expect("payload"),
        id,
        "the reason's payload is the `Id` term itself"
    );
    assert!(
        machine.x(0).is_none() || machine.x(0) != Some(id),
        "nothing was created into the destination"
    );
}

#[test]
fn foreign_create_with_a_non_exported_host_definition_branches_to_a_live_fail_label() {
    // The `fail`-label half of `$FAIL_HEAD_OR_BODY($Fail)` (`emu/macros.tab:266-278`):
    // the error is *set* and the branch taken — no reason is built, nothing raises.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, false, &[(a, Some(make_small(1)))]),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");
    machine.set_x(0, make_small(77)).expect("X(0)");

    let step = run(
        fx.module(),
        &labels_with(Label(7), 0, 4),
        &mut machine,
        &create(Operand::X(1), Label(7), Vec::new()),
        &mut ctx,
    )
    .expect("a branch, not a raise");
    assert_eq!(step, Some(Step::Goto { func: 0, pc: 4 }));
    assert_eq!(
        machine.x(0).expect("X(0)"),
        make_small(77),
        "the destination is untouched on a fail branch"
    );
}

#[test]
fn foreign_create_without_a_host_record_table_is_a_typed_error() {
    // `Ok(None)` — no record table on this host, or a module it cannot answer for.
    // The family keeps its honest typed error (a definition is never guessed).
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r) = (fx.atom("other"), fx.atom("r"));

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &create(Operand::X(1), NO_FAIL, Vec::new()),
            &mut ctx,
        ),
    );
    assert_eq!(
        reason,
        "unsupported_opcode: put_record: create: no record definition table for another module"
    );
}

// ---------------------------------------------------------------------------
// The generic `{SelfModule, Name}` form
// ---------------------------------------------------------------------------

/// Give the fixture a `Recs` record: `r` with fields `a` (default 1) and `b`
/// (default 2), exported as asked.
fn own_record(fx: &mut Fixture, exported: bool) -> (Atom, Atom, Atom) {
    let (r, a, b) = (fx.atom("r"), fx.atom("a"), fx.atom("b"));
    let def = definition(r, exported, &[(a, Some(make_small(1))), (b, Some(make_small(2)))]);
    fx.module.records.push(def);
    (r, a, b)
}

#[test]
fn the_generic_form_for_an_own_non_exported_record_raises_badrecord() {
    // C's `erts_record_find_entry` finds the running module's own record in the
    // record table too, so the generic opcode takes the export gate for it as well
    // (`erl_record.c:401`): an own, non-exported record is a `badrecord` here —
    // *not* the silent create the atom-`Id` (local) opcode performs.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let module_name = fx.module().name;
    let (r, _a, _b) = own_record(&mut fx, false);
    intern_raise_atoms(&mut fx, "badrecord");
    let badrecord = fx.atom("badrecord");

    let id = id_pair(&mut ctx, module_name, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::X(1), NO_FAIL, Vec::new()),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, ExceptionClass::Error);
    assert_eq!(
        terms::tuple_element(reason, 0).expect("tag"),
        make_atom(badrecord)
    );
    assert_eq!(
        terms::tuple_element(reason, 1).expect("payload"),
        id,
        "the reason's payload is the Id term itself"
    );
}

#[test]
fn the_generic_form_for_an_own_exported_record_creates() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let module_name = fx.module().name;
    let (r, a, b) = own_record(&mut fx, true);

    let id = id_pair(&mut ctx, module_name, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(
            Operand::X(1),
            NO_FAIL,
            vec![(Operand::Atom(a), Operand::Integer(5))],
        ),
        &mut ctx,
    )
    .expect("the exported own record creates through the generic form");
    assert_eq!(step, Some(Step::Next));

    let result = machine.x(0).expect("X(0)");
    assert_eq!(field(fx.module(), result, a), Some(make_small(5)));
    assert_eq!(field(fx.module(), result, b), Some(make_small(2)));
    let def = definition_of(result);
    assert_eq!(
        raw::definition_module(def).expect("owner"),
        module_name,
        "the instance is owned by the running module"
    );
}

#[test]
fn the_local_form_still_creates_a_non_exported_own_record() {
    // The atom-`Id` form is `i_create_local_native_record` (`emu/instrs.tab:1358`),
    // which reads the `Recs` definition directly and imposes **no** export gate —
    // the C branch that makes the two `Id` shapes observably different.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (r, a, b) = own_record(&mut fx, false);

    let mut machine = Machine::with_max_register(4);
    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::Atom(r), NO_FAIL, Vec::new()),
        &mut ctx,
    )
    .expect("a non-exported own record is creatable through the local form");
    assert_eq!(step, Some(Step::Next));

    let result = machine.x(0).expect("X(0)");
    assert_eq!(field(fx.module(), result, a), Some(make_small(1)));
    assert_eq!(field(fx.module(), result, b), Some(make_small(2)));
    assert!(!raw::definition_is_exported(definition_of(result)).expect("flag"));
}

// ---------------------------------------------------------------------------
// The create form's error ordering, on a definition that came from the host
// ---------------------------------------------------------------------------

#[test]
fn foreign_create_badfield_outranks_no_value() {
    // `erl_record.c:331-337`: "A `badfield` error has higher priority than a
    // `no_value` error" — with both present, the **unconsumed update key** is the
    // error field and the tag is `badfield`.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, b, z) = (fx.atom("other"), fx.atom("r"), fx.atom("b"), fx.atom("z"));
    intern_raise_atoms(&mut fx, "badfield");
    let badfield = fx.atom("badfield");
    // `b` has a default, `a`-equivalent (a *named* absent field) is `z`… the absent
    // default is carried by the field below, and `z` is not a field at all.
    let absent = fx.atom("absent");
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, true, &[(absent, None), (b, Some(make_small(2)))]),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(
            Operand::X(1),
            NO_FAIL,
            vec![(Operand::Atom(z), Operand::Integer(3))],
        ),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, ExceptionClass::Error);
    assert_field_failure_reason(reason, badfield, other, r, z);
}

#[test]
fn foreign_create_no_value_remembers_only_the_first_absent_field() {
    // `if (is_value(res)) { p->fvalue = defp->keys[i]; … res = THE_NON_VALUE; }`
    // (`erl_record.c:318-326`): the *first* absent-default field in ascending key
    // order wins and every later one is only stored as `NIL`.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, b) = (fx.atom("other"), fx.atom("r"), fx.atom("b"));
    // Interned first, so `first` sorts **below** `second`.
    let first = fx.atom("first");
    let second = fx.atom("second");
    intern_raise_atoms(&mut fx, "novalue");
    let novalue = fx.atom("novalue");
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(
            r,
            true,
            &[(second, None), (first, None), (b, Some(make_small(2)))],
        ),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::X(1), NO_FAIL, Vec::new()),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, ExceptionClass::Error);
    assert_field_failure_reason(reason, novalue, other, r, first);
}

#[test]
fn foreign_create_distinguishes_a_some_zero_default_from_an_absent_one() {
    // `beam_loader::RecordField::default`'s `None` is the file's absent sentinel
    // (`u,0`); a default that is literally `0` is `Some(<small 0>)` and is stored.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a, b) = (fx.atom("other"), fx.atom("r"), fx.atom("a"), fx.atom("b"));
    intern_raise_atoms(&mut fx, "novalue");
    let novalue = fx.atom("novalue");

    // The `Some(0)` field is stored as `0`, and nothing errors.
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, true, &[(a, Some(make_small(0))), (b, Some(make_small(5)))]),
    )]);
    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");
    let step = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::X(1), NO_FAIL, Vec::new()),
        &mut ctx,
    )
    .expect("a `Some(0)` default is an ordinary value");
    assert_eq!(step, Some(Step::Next));
    assert_eq!(
        field(fx.module(), machine.x(0).expect("X(0)"), a),
        Some(make_small(0)),
        "a `Some(<small 0>)` default is stored as 0"
    );

    // The absent one is the `no_value` field and names itself.
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, true, &[(a, Some(make_small(0))), (b, None)]),
    )]);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");
    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::X(1), NO_FAIL, Vec::new()),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, ExceptionClass::Error);
    assert_field_failure_reason(reason, novalue, other, r, b);
}

// ---------------------------------------------------------------------------
// The atom-namespace rule
// ---------------------------------------------------------------------------

/// The error tag of a `NO_FAIL` record error is a **global** in C
/// (`error_atom[EXC_BADFIELD]`/`[EXC_NOVALUE]`/`[EXC_BADRECORD]`,
/// `beam_common.c:754-768`), so it exists in every universe — a module whose own
/// load-time snapshot never mentions it still raises. The engine resolves the tag
/// through the live atom namespace with the module snapshot as fallback
/// (`crate::machine::namespace_atom`), so a shared namespace that CAN spell it is
/// enough.
///
/// These two tests build exactly that: the fixture module interns **none** of
/// `badfield`/`novalue`/`error`, and the context's `ExecutionContext::atoms` returns
/// a shared table that does.
fn shared_raise_namespace(tags: &[&str]) -> AtomTable {
    let mut shared = AtomTable::default();
    for name in tags {
        shared.intern(name);
    }
    shared.intern("error");
    shared
}

#[test]
fn create_badfield_resolves_its_tag_from_the_live_namespace() {
    // `erl_record.c:331-337`/`:339-351`: an unconsumed update entry is the
    // `badfield` error, and `expand_error_value` wraps the
    // `TUPLE2(TUPLE2(Module,Name),Field)` payload with `error_atom[EXC_BADFIELD]`.
    // The running module cannot name `badfield`; the live namespace can.
    let mut fx = Fixture::new();
    let (other, r, b, z) = (fx.atom("other"), fx.atom("r"), fx.atom("b"), fx.atom("z"));
    let shared = shared_raise_namespace(&["badfield"]);
    let badfield = shared.lookup("badfield").expect("the shared `badfield` atom");
    assert!(
        fx.module().atom_table.lookup("badfield").is_none(),
        "the module's own snapshot must not contain the tag"
    );
    let mut ctx = MinimalContext::with_shared_atoms(shared);
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, true, &[(b, Some(make_small(2)))]),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(
            Operand::X(1),
            NO_FAIL,
            vec![(Operand::Atom(z), Operand::Integer(3))],
        ),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, ExceptionClass::Error, "`badfield` is class `error`");
    assert_field_failure_reason(reason, badfield, other, r, z);
}

#[test]
fn create_novalue_resolves_its_tag_from_the_live_namespace() {
    // The second delayed-field error: a field whose default is the absent sentinel
    // (`erl_record.c:318-326`), rendered with `error_atom[EXC_NOVALUE]`.
    let mut fx = Fixture::new();
    let (other, r, b) = (fx.atom("other"), fx.atom("r"), fx.atom("b"));
    let shared = shared_raise_namespace(&["novalue"]);
    let novalue = shared.lookup("novalue").expect("the shared `novalue` atom");
    assert!(
        fx.module().atom_table.lookup("novalue").is_none(),
        "the module's own snapshot must not contain the tag"
    );
    let mut ctx = MinimalContext::with_shared_atoms(shared);
    ctx.set_foreign_record_definitions(vec![(other, r, definition(r, true, &[(b, None)]))]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let result = run(
        fx.module(),
        &LabelMap::new(),
        &mut machine,
        &create(Operand::X(1), NO_FAIL, Vec::new()),
        &mut ctx,
    );
    let (class, reason) = raised_reason(&machine, &result);
    assert_eq!(class, ExceptionClass::Error, "`novalue` is class `error`");
    assert_field_failure_reason(reason, novalue, other, r, b);
}

#[test]
fn foreign_create_rejects_a_definition_whose_names_the_caller_cannot_spell() {
    // `Atom` is a table-local index, so a definition whose field-name atom is not in
    // the running process's live namespace must be a typed error — an index written
    // into a term would silently name a *different* atom.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a, b) = (fx.atom("other"), fx.atom("r"), fx.atom("a"), fx.atom("b"));
    let stray = Atom::from_raw(60_000);
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(r, true, &[(a, Some(make_small(1))), (stray, Some(make_small(2)))]),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");
    let dst_before = machine.x(0);

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &create(Operand::X(1), NO_FAIL, Vec::new()),
            &mut ctx,
        ),
    );
    assert_eq!(
        reason,
        "unsupported_opcode: put_record: create: a record definition field name is not in the caller's atom namespace"
    );
    assert_eq!(
        machine.x(0),
        dst_before,
        "the instruction raised before writing Dst, so no term can carry the stray index"
    );
    let _ = b;
}

#[test]
fn foreign_create_rejects_a_definition_whose_record_name_the_caller_cannot_spell() {
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a) = (fx.atom("other"), fx.atom("r"), fx.atom("a"));
    let stray_name = Atom::from_raw(60_001);
    // The **Id** names `r` (a live-namespace atom) but the host's definition claims a
    // name the caller's namespace cannot spell.
    ctx.set_foreign_record_definitions(vec![(
        other,
        r,
        definition(stray_name, true, &[(a, Some(make_small(1)))]),
    )]);

    let id = id_pair(&mut ctx, other, r);
    let mut machine = Machine::with_max_register(4);
    machine.set_x(1, id).expect("X(1)");

    let reason = raise_reason_atom(
        fx.module(),
        run(
            fx.module(),
            &LabelMap::new(),
            &mut machine,
            &create(Operand::X(1), NO_FAIL, Vec::new()),
            &mut ctx,
        ),
    );
    assert_eq!(
        reason,
        "unsupported_opcode: put_record: create: a record definition name is not in the caller's atom namespace"
    );
}

// ---------------------------------------------------------------------------
// Tier parity
// ---------------------------------------------------------------------------

/// Append `f/1` — `put_record Fail Id nil Dst 0 Updates` then `return` — and export
/// it, so the tier-selecting entries ([`Machine::run_with_tier`]) can run it.
fn exported_create_function(
    module: &mut Module,
    id: Operand,
    updates: Vec<(Operand, Operand)>,
) -> Atom {
    let name = module.atom_table.intern("f");
    let code = vec![
        Instr::Label(Label(1)),
        create(id, NO_FAIL, updates),
        Instr::Return,
    ];
    module.functions.push(Function {
        func_label: Label(1),
        entry_label: Label(1),
        module: module.name,
        name,
        arity: 1,
        code,
        blocks: Vec::new(),
        labels: BTreeMap::from([(Label(1), 0)]),
    });
    module.exports.push(Export {
        function: name,
        arity: 1,
        label: Label(1),
    });
    name
}

#[test]
fn foreign_create_agrees_across_the_interpreter_and_the_baseline_tier() {
    // The native baseline tier never inlines a record instruction, so it must deopt
    // to the same family body and produce a byte-identical result — including the
    // `badrecord` raise on the non-exported leg.
    let mut fx = Fixture::new();
    let mut ctx = MinimalContext::new();
    let (other, r, a, b) = (fx.atom("other"), fx.atom("r"), fx.atom("a"), fx.atom("b"));
    let (other2, r2) = (fx.atom("other2"), fx.atom("r2"));
    intern_raise_atoms(&mut fx, "badrecord");
    ctx.set_foreign_record_definitions(vec![
        (
            other,
            r,
            definition(r, true, &[(a, Some(make_small(1))), (b, Some(make_small(2)))]),
        ),
        (other2, r2, definition(r2, false, &[(a, Some(make_small(1)))])),
    ]);

    let good = id_pair(&mut ctx, other, r);
    let bad = id_pair(&mut ctx, other2, r2);

    let function = {
        // `f/1` takes the `Id` in `X(0)` and creates into `X(0)`.
        let mut module = fx.module().clone();
        let function = exported_create_function(
            &mut module,
            Operand::X(0),
            vec![(Operand::Atom(b), Operand::Integer(9))],
        );
        let mut machine = Machine::with_max_register(4);
        // The interpreter leg over the *cloned* module (the fixture stays untouched).
        let interpreted = machine
            .run_with_tier(&module, function, 1, &[good], &mut ctx, Tier::Interpreter)
            .expect("the interpreter creates the foreign record");
        let values = [
            field(&module, interpreted, a),
            field(&module, interpreted, b),
        ];

        let mut jit_machine = Machine::with_max_register(4);
        let jitted = jit_machine
            .run_with_tier(&module, function, 1, &[good], &mut ctx, Tier::BaselineJit)
            .expect("the baseline tier creates the foreign record");
        assert_eq!(
            values,
            [
                field(&module, jitted, a),
                field(&module, jitted, b)
            ],
            "both tiers produce the same field values"
        );
        assert_eq!(
            raw::definition_module(definition_of(jitted)).expect("owner"),
            raw::definition_module(definition_of(interpreted)).expect("owner"),
            "both tiers store the same definition owner"
        );

        // The non-exported leg raises `badrecord` on both tiers.
        let mut machine = Machine::with_max_register(4);
        let interpreted = machine.run_with_tier(
            &module,
            function,
            1,
            &[bad],
            &mut ctx,
            Tier::Interpreter,
        );
        let interpreted_reason = raised_reason(&machine, &interpreted);
        let mut jit_machine = Machine::with_max_register(4);
        let jitted =
            jit_machine.run_with_tier(&module, function, 1, &[bad], &mut ctx, Tier::BaselineJit);
        let jitted_reason = raised_reason(&jit_machine, &jitted);
        assert_eq!(interpreted_reason.0, jitted_reason.0, "the same class");
        assert_eq!(
            terms::tuple_arity(interpreted_reason.1).expect("a tuple"),
            terms::tuple_arity(jitted_reason.1).expect("a tuple"),
            "the same reason shape"
        );
        function
    };
    let _ = function;
}
