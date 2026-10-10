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

//! The `records` module BIFs — native records (`records:create/4` family).
//!
//! OTP 29 introduced **native records** (`-record` as a first-class runtime value,
//! produced by this BIF family) alongside the classic "tagged tuple" records. The
//! frozen `ErtsRecordDefinition`/`ErtsRecordInstance` layout is owned by
//! [`beam_term::record`] (the lowest common ancestor for term layouts, workspace
//! `CONTEXT.md` §2); this module is the BIF surface over it.
//!
//! Ground truth (read-only): `../../erts/emulator/beam/erl_record.c`
//! (`records_create_4` `:904-1085`, `records_update_4` `:1087-1245`,
//! `records_get_2` `:1247-1274`, `records_get_module_1` `:1276-1285`,
//! `records_get_name_1` `:1287-1296`, `records_get_field_names_1` `:1298-1326`,
//! `records_is_exported_1` `:1328-1340`, `records_get_definition_2` `:1342-1365`)
//! and the native-record half of `is_record/2,3`
//! (`../../erts/emulator/beam/erl_bif_op.c:293-350`).
//!
//! # Structured reasons carry their term
//!
//! WHERE the oracle raises a *structured* reason — `{badrecord, Term}`,
//! `{badfield, {{Module, Name}, Key}}`, `{badmap, Map}` — this crate raises the same
//! tuple as the live reason term of an
//! [`ExitReason::Structured`](beam_common::ExitReason::Structured) raise
//! ([`error::structured_error`]; the payload rides the opaque
//! [`Raise::live_reason`](beam_common::Raise::live_reason) slot, because
//! `beam_common::ExitReason` holds no term). The plain `badarg` cases (`badarg` in the
//! oracle: a missing field key, `records:get_module/1`/`get_name/1` of a non-record,
//! malformed `create/4` options/fields) are reproduced exactly.
//!
//! Every BIF here is registered under the `records` module from [`register`], which
//! [`crate::registry::BifRegistry::with_core_bifs`] calls.

use beam_common::{BeamError, Result};
use beam_term::compare::{cmp_flatmap_keys, eq};
use beam_term::{atom_val, Eterm};

use crate::access;
use crate::context::BifContext;
use crate::error;
use crate::registry::BifRegistry;

/// Whether the atom term `t` is the atom named `name` (resolved through the
/// context's atom table).
///
/// Used for the two atoms this module must recognize by name: the `is_exported`
/// option key and the `'_'` update wildcard (which has no fixed index in a table
/// seeded ad hoc, unlike `AM_TRUE`/`AM_FALSE`).
fn atom_name_is(ctx: &BifContext<'_>, t: Eterm, name: &str) -> bool {
    access::is_atom(t) && ctx.atoms().name(atom_val(t)) == Some(name)
}

/// The definition term of `term`, requiring `term` to be a native record.
///
/// Both callers (`records:get_module/1`, `records:get_name/1`) answer the oracle's
/// atom `badarg` for a non-record, which is what this reports. The introspection BIFs
/// that raise the structured `{badrecord, Term}` use
/// [`definition_or_badrecord`] instead.
fn definition_of(term: Eterm) -> Result<Eterm> {
    if !access::is_record(term) {
        return Err(error::badarg());
    }
    access::native_record_definition(term)
}

/// As [`definition_of`], but raises the oracle's structured `error:{badrecord, Term}`
/// for a non-record (`records:get_field_names/1`, `records:is_exported/1`).
///
/// Pinned by the oracle:
/// `erl -noshell -eval 'try records:get_field_names(5) catch C:R ->
/// io:format("~p:~p~n",[C,R]) end, halt().'` ⇒ `error:{badrecord,5}`; the same for
/// `records:is_exported(5)`.
fn definition_or_badrecord(ctx: &mut BifContext<'_>, term: Eterm) -> Result<Eterm> {
    if !access::is_record(term) {
        return Err(error::structured_error(ctx, "badrecord", &[term])?);
    }
    access::native_record_definition(term)
}

/// The oracle's `error:{badfield, {{Module, Name}, Key}}` — the reason of an update or
/// create whose field key names no field of the record (or, for `create/4`, is not an
/// atom at all). Pinned by the oracle:
/// `erl -noshell -eval 'R = records:create(m,r,[{a,1}],#{is_exported=>true}),
/// try records:update(R,m,r,#{zz=>9}) catch C:E -> io:format("~p:~p~n",[C,E]) end,
/// halt().'` ⇒ `error:{badfield,{{m,r},zz}}` (with the record's own module/name, even
/// when the caller passed the `'_'` wildcard).
fn badfield(ctx: &mut BifContext<'_>, module: Eterm, name: Eterm, key: Eterm) -> Result<BeamError> {
    let identifier = ctx.tuple(&[module, name])?;
    error::structured_error(ctx, "badfield", &[identifier, key])
}

/// `records:get/2` (`records_get_2`, `erl_record.c:1247-1274`): a native record
/// and an atom key select a field value; anything else — a non-record, a non-atom
/// key, or a key with no matching field — is `badarg`.
fn get(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (key, rec) = (args[0], args[1]);
    if !access::is_atom(key) || !access::is_record(rec) {
        return Err(error::badarg());
    }
    match access::native_record_get(rec, key, ctx.atoms())? {
        Some(value) => Ok(value),
        None => Err(error::badarg()),
    }
}

/// `records:get_module/1` (`records_get_module_1`, `erl_record.c:1276-1285`): the
/// definition's `module` atom; a non-record is `badarg`.
fn get_module(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let def = definition_of(args[0])?;
    access::native_record_definition_module(def)
}

/// `records:get_name/1` (`records_get_name_1`, `erl_record.c:1287-1296`): the
/// definition's `name` atom; a non-record is `badarg`.
fn get_name(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let def = definition_of(args[0])?;
    access::native_record_definition_name(def)
}

/// `records:get_field_names/1` (`records_get_field_names_1`,
/// `erl_record.c:1298-1326`): the field names in **declaration** order; a
/// non-record is the structured `error:{badrecord, Term}`.
fn get_field_names(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let def = definition_or_badrecord(ctx, args[0])?;
    let keys = access::native_record_declaration_keys(def)?;
    ctx.list(&keys)
}

/// `records:is_exported/1` (`records_is_exported_1`, `erl_record.c:1328-1340`):
/// the definition's stored `true`/`false` atom; a non-record is the structured
/// `error:{badrecord, Term}`.
///
/// # The stored flag is a *frozen* atom index, not a runtime-table name
///
/// `beam_term::write_record` stores the flag word as the frozen
/// [`AM_FALSE`](beam_term::AM_FALSE)/[`AM_TRUE`](beam_term::AM_TRUE) atom
/// **index** (`0`/`1`, the legacy `atom.names` positions), so the word must be
/// decoded back to a `bool` and re-materialized through *this* context's atom
/// table. Returning the raw word instead hands the caller whatever name happens
/// to sit at index `0`/`1` here: the runtime seeds the platform atoms first
/// (`unix`, `linux` — `beam_rt::boot::CORE_ATOMS`), so an exported record reads
/// back as `linux` and a non-exported one as `unix` instead of `true`/`false`.
/// This is the same decode `records:update/4` already performs.
fn is_exported(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let def = definition_or_badrecord(ctx, args[0])?;
    let flag = access::native_record_definition_is_exported(def)?;
    ctx.boolean(atom_val(flag) == beam_term::AM_TRUE)
}

/// The `(key, value)` pairs of the `records:create/4` `Fields` argument.
///
/// `fields` must be a proper list of 2-tuples; anything else — a non-list, an
/// improper tail, a non-2-tuple element — is the oracle's `badarg`. The key-atom
/// and duplicate-key checks are performed by [`access::native_record_write`]
/// (`beam_term::write_record`), matching the C's post-sort duplicate scan.
fn field_pairs(fields: Eterm) -> Result<Vec<(Eterm, Eterm)>> {
    let mut out = Vec::new();
    let mut cur = fields;
    loop {
        if access::is_nil(cur) {
            return Ok(out);
        }
        if !access::is_list(cur) {
            // A non-list, or an improper list tail.
            return Err(error::badarg());
        }
        let pair = access::car(cur);
        if !access::is_tuple(pair) || access::tuple_size(pair) != 2 {
            return Err(error::badarg());
        }
        out.push((
            access::tuple_element(pair, 0),
            access::tuple_element(pair, 1),
        ));
        cur = access::cdr(cur);
    }
}

/// `records:create/4` (`records_create_4`, `erl_record.c:904-1085`).
///
/// `Module`/`Name` must be atoms, `Opts` a 1-entry map containing `is_exported`
/// with a `true`/`false` value, and `Fields` a proper list of `{Key, Value}`
/// 2-tuples with atom keys (no duplicates). The result is the frozen
/// `[field_order][definition][instance]` block; its cached definition hash is
/// `make_small(0)` (this crate cannot compute the C `make_hash2` — see
/// `beam_term::record`).
///
/// # Errors
///
/// The oracle's own reasons, in its own check order (verified with
/// `erl -noshell -eval 'try records:create(...) catch C:E -> io:format("~p:~p~n",[C,E])
/// end, halt().'`):
///
/// * `{badrecord, Name}` for a non-atom `Module` or `Name`
///   (`records:create(1, 2, [], #{is_exported=>true})` ⇒ `error:{badrecord,2}`);
/// * `{badmap, Opts}` for a non-map `Opts` — checked **before** `Fields`
///   (`records:create(m, r, 5, 5)` ⇒ `error:{badmap,5}`);
/// * `{badfield, {{Module, Name}, Key}}` for a field key that is not an atom —
///   checked **before** the duplicate-key scan
///   (`records:create(m, r, [{a,1},{a,2},{1,3}], #{is_exported=>true})`
///   ⇒ `error:{badfield,{{m,r},1}}`);
/// * the plain atom `badarg` for everything else the oracle reports as `badarg`
///   (a malformed `Opts` map, a malformed `Fields` list, a duplicate key).
fn create(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (module, name, fields, opts) = (args[0], args[1], args[2], args[3]);
    if !access::is_atom(module) || !access::is_atom(name) {
        return Err(error::structured_error(ctx, "badrecord", &[name])?);
    }
    if !access::is_map(opts) {
        return Err(error::structured_error(ctx, "badmap", &[opts])?);
    }
    // `Opts` must hold exactly one entry (`is_exported`), so it is always a flat
    // map; `flatmap_pairs` reads it directly and rejects any other shape (a map
    // of another size is a typed `badarg`, matching the C's exactly-1 check).
    let (opt_keys, opt_values) = access::flatmap_pairs(opts)?;
    if opt_keys.len() != 1 || !atom_name_is(ctx, opt_keys[0], "is_exported") {
        return Err(error::badarg());
    }
    let is_exported = match ctx.atoms().name(atom_val(opt_values[0])) {
        Some("true") => true,
        Some("false") => false,
        _ => return Err(error::badarg()),
    };
    let pairs = field_pairs(fields)?;
    // A non-atom key is `{badfield, {{Module, Name}, Key}}` and is checked before
    // `beam_term::write_record`'s duplicate scan, exactly as the C does.
    for &(key, _) in &pairs {
        if !access::is_atom(key) {
            return Err(badfield(ctx, module, name, key)?);
        }
    }
    access::native_record_write(
        ctx.heap(),
        atom_val(module),
        atom_val(name),
        is_exported,
        &pairs,
    )
}

/// `records:update/4` (`records_update_4`, `erl_record.c:1087-1245`).
///
/// `Record` must be a native record. `Name == '_'` skips the module/name check;
/// otherwise `Module`/`Name` must be atoms matching the record's own definition.
/// `Updates` must be a map of `Field => NewValue`; an empty map returns the
/// original record unchanged, and every update key must name an existing field.
/// The result shares the record's definition (observationally identical to the C,
/// which copies the def pointer).
///
/// # Both map shapes are accepted
///
/// `Updates` may be a flat map **or** a hash map (a `>MAP_SMALL_MAP_LIMIT`-key
/// HAMT): the C has a separate arm for each (`erl_record.c:1148-1180` and
/// `:1181-1229`), and the field rules are identical. The update keys are ordered
/// by the C's `erts_cmp_flatmap_keys` (`record_compare`, `erl_record.c:892-902`)
/// — the order a flat map already stores its keys in, and the order the C sorts a
/// hash map's keys into before merging (`erl_record.c:1203-1204`) — so a map
/// carrying several non-field keys reports the same one the oracle does.
///
/// Verified against the oracle (erts-17.1) with a 40-field record:
/// `records:update(R, m, r, U)` with a 40-entry `U` updates every field, and the
/// same `U` with the module/name wildcards returns the record unchanged for
/// fields it does not mention. A >32-entry update map whose keys name no field
/// reports the **lowest-atom-index** offender as
/// `error:{badfield,{{Module,Name},Key}}`.
///
/// # Errors
///
/// The oracle's own reasons (verified with
/// `erl -noshell -eval 'R = records:create(m,r,[{a,1}],#{is_exported=>true}),
/// try records:update(...) catch C:E -> io:format("~p:~p~n",[C,E]) end, halt().'`):
///
/// * `{badrecord, Record}` for a non-`Record` first argument;
/// * `{badrecord, Name}` for a non-atom or mismatching `Module`/`Name`
///   (`records:update(R, other, r, #{a=>9})` ⇒ `error:{badrecord,r}`);
/// * `{badmap, Updates}` for a non-map `Updates`;
/// * `{badfield, {{Module, Name}, Key}}` for an update key naming no field — with the
///   **record's own** module/name, even when the caller passed the `'_'` wildcard
///   (`records:update(R, '_', '_', #{zz=>9})` ⇒ `error:{badfield,{{m,r},zz}}`). Which
///   key is reported for several bad keys follows the update map's stored order (OTP
///   documents map order as unspecified).
fn update(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (rec, module, name, updates) = (args[0], args[1], args[2], args[3]);
    if !access::is_record(rec) {
        return Err(error::structured_error(ctx, "badrecord", &[rec])?);
    }
    let def = access::native_record_definition(rec)?;
    let rec_module = access::native_record_definition_module(def)?;
    let rec_name = access::native_record_definition_name(def)?;
    let rec_exported = access::native_record_definition_is_exported(def)?;

    // `Name == '_'` skips the module/name check entirely (`records_update_4`).
    if !atom_name_is(ctx, name, "_") {
        if !access::is_atom(module) || !access::is_atom(name) {
            return Err(error::structured_error(ctx, "badrecord", &[name])?);
        }
        if rec_module != module || rec_name != name {
            return Err(error::structured_error(ctx, "badrecord", &[name])?);
        }
    }

    if !access::is_map(updates) {
        return Err(error::structured_error(ctx, "badmap", &[updates])?);
    }
    if access::map_size(updates)? == 0 {
        // The oracle returns the record unchanged for an empty update map
        // (`records_update_4`, `erl_record.c:1131-1133`).
        return Ok(rec);
    }

    // The update keys, ordered as the C's merge sees them (`record_compare`,
    // `erl_record.c:892-902`) — see the function docs. A flat map's stored order
    // already is that order; a hash map's is not, so sort either way.
    let mut update_keys: Vec<Eterm> = access::map_pairs_storage_order(updates)?
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    update_keys.sort_by(|a, b| cmp_flatmap_keys(*a, *b, ctx.atoms()));

    let keys = access::native_record_declaration_keys(def)?;
    for &update_key in &update_keys {
        if !keys.iter().any(|&key| eq(update_key, key, ctx.atoms())) {
            return Err(badfield(ctx, rec_module, rec_name, update_key)?);
        }
    }

    // Rebuild in declaration order, overlaying the update map's values.
    let mut pairs = Vec::with_capacity(keys.len());
    for &key in &keys {
        let value = match access::map_get(updates, key, ctx.atoms())? {
            Some(value) => value,
            None => access::native_record_get(rec, key, ctx.atoms())?.ok_or_else(error::badarg)?,
        };
        pairs.push((key, value));
    }
    // Preserve the record's own definition (module/name/is_exported).
    let is_exported = atom_val(rec_exported) == beam_term::AM_TRUE;
    access::native_record_write(
        ctx.heap(),
        atom_val(rec_module),
        atom_val(rec_name),
        is_exported,
        &pairs,
    )
}

/// `records:get_definition/2` (`records_get_definition_2`,
/// `erl_record.c:1342-1403`).
///
/// `Module`/`Name` must be atoms (a non-atom is `error:badarg`, exactly the
/// oracle's pre-check). The pair is resolved through the code server's
/// record-definition table (`erts_record_find_entry`, `erl_record.c:163-170`),
/// which a `.beam` populates from its `Recs` chunk at load time
/// (`erts_record_put`, `erl_record.c:173-178`; from `jit/asm_load.c:1246-1260`
/// and `emu/emu_load.c:735`). This crate reaches that table through the
/// [`RecordSource`](crate::context::RecordSource) hook — the runtime's code
/// server resolves the module's atoms and hands back a name-resolved
/// [`RecordDefinitionView`](crate::context::RecordDefinitionView).
///
/// A resolved definition renders the oracle's shape: the 2-tuple
/// `{#{is_exported => Bool}, Fields}`, where `Fields` is the field list in
/// **declaration** order — a field without a default contributes its bare name
/// atom, and a field with a default contributes `{Name, Default}`. With no
/// definition for the pair (and when no `RecordSource` hook is installed, which
/// answers `Ok(None)`) the result is `error:badarg`, exactly the oracle's answer
/// for a record no *loaded* module defines.
///
/// Verified against the oracle (erts-17.1) with a module compiled from
/// `-record #rec{a, b = 5, c}.` + `-record #exp{x = 1, y}.` +
/// `-export_record([exp]).`:
/// `records:get_definition(recmod, rec)` ⇒ `{#{is_exported => false},[a,{b,5},c]}`;
/// `records:get_definition(recmod, exp)` ⇒ `{#{is_exported => true},[{x,1},y]}`;
/// `records:get_definition(recmod, nope)` ⇒ `error:badarg`.
fn get_definition(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_atom(args[0]) || !access::is_atom(args[1]) {
        return Err(error::badarg());
    }
    // An atom absent from this context's table resolves to no name and therefore
    // names no loaded definition — the oracle answers `badarg` for an unknown pair.
    let module = ctx
        .atoms()
        .name(atom_val(args[0]))
        .ok_or_else(error::badarg)?
        .to_string();
    let name = ctx
        .atoms()
        .name(atom_val(args[1]))
        .ok_or_else(error::badarg)?
        .to_string();
    let definition = match ctx.record_definition(&module, &name)? {
        Some(definition) => definition,
        None => return Err(error::badarg()),
    };

    let key = ctx.intern_atom("is_exported")?;
    let value = ctx.boolean(definition.is_exported)?;
    let map = access::make_flatmap(ctx.heap(), &[(key, value)])?;

    let mut fields = Vec::with_capacity(definition.fields.len());
    for field in &definition.fields {
        let field_atom = ctx.intern_atom(&field.name)?;
        let element = match field.default {
            None => field_atom,
            Some(default) => ctx.tuple(&[field_atom, default])?,
        };
        fields.push(element);
    }
    let fields = ctx.list(&fields)?;
    ctx.tuple(&[map, fields])
}

/// Registers the `records` BIFs.
pub(crate) fn register(reg: &mut BifRegistry) {
    reg.register("records", "get", 2, get);
    reg.register("records", "get_module", 1, get_module);
    reg.register("records", "get_name", 1, get_name);
    reg.register("records", "get_field_names", 1, get_field_names);
    reg.register("records", "is_exported", 1, is_exported);
    reg.register("records", "create", 4, create);
    reg.register("records", "update", 4, update);
    reg.register("records", "get_definition", 2, get_definition);
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_alloc::Heap;
    use beam_codec::term::{cons_head, cons_tail};
    use beam_common::BeamError;
    use beam_process::ProcessId;
    use beam_term::{is_list, is_nil, make_small, AtomTable, NIL};

    use crate::context::{RecordDefinitionView, RecordFieldView, RecordSource};

    /// The atom table the tests seed: index 0 is `false` and index 1 is `true`,
    /// the standard `atom.names` seeding the record `is_exported` atom is built
    /// from (`beam_term::AM_FALSE`/`AM_TRUE`).
    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "false",
            "true",
            "is_exported",
            "_",
            "a",
            "b",
            "m",
            "r",
            "other",
            // The structured-reason tag atoms the introspection/`create`/`update`
            // failures build (`error:{badrecord,_}`, `{badmap,_}`, `{badfield,_}`).
            "badrecord",
            "badmap",
            "badfield",
        ]);
        atoms
    }

    /// Runs `f` over a fresh context with the seeded atom table.
    fn with_ctx<R>(f: impl FnOnce(&mut BifContext<'_>, &AtomTable) -> R) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx, &atoms)
    }

    /// An atom table seeded the way the **runtime** seeds its global table
    /// (`beam_rt::boot::CORE_ATOMS`): the `system_info/1` platform atoms come
    /// first, so the boolean atoms are *not* at indices 0/1 (`false` is 5, `true`
    /// is 4). A native-record definition always stores its `is_exported` flag as
    /// the *frozen* `AM_FALSE`/`AM_TRUE` index (0/1), so an `is_exported/1` that
    /// returned that raw word would hand the caller `unix`/`linux` here — the exact
    /// runtime divergence this table exists to pin.
    fn runtime_atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed([
            "unix",
            "linux",
            "win32",
            "nt",
            "true",
            "false",
            "is_exported",
            "_",
            "a",
            "b",
            "m",
            "r",
            "other",
            "badrecord",
            "badmap",
            "badfield",
        ]);
        atoms
    }

    /// As [`with_ctx`], but over the [`runtime_atoms`] table.
    fn with_runtime_ctx<R>(f: impl FnOnce(&mut BifContext<'_>, &AtomTable) -> R) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = runtime_atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        f(&mut ctx, &atoms)
    }

    /// The live reason term of a structured raise, asserting the full contract:
    /// `Err(BeamError::Raise(r))`, `r.reason == ExitReason::Structured` (so
    /// `BeamError::as_atom()` is `None`) and `r.live_reason()` carrying the real
    /// reason term.
    #[track_caller]
    fn structured_reason(result: Result<Eterm>) -> Eterm {
        match result {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error, "class");
                assert_eq!(raise.reason, beam_common::ExitReason::Structured, "reason");
                assert_eq!(raise.reason.as_atom(), None, "as_atom");
                *raise
                    .live_reason()
                    .expect("a structured reason carries its live term")
                    .downcast_ref::<Eterm>()
                    .expect("the payload is the live reason term")
            }
            other => panic!("expected a structured raise, got {other:?}"),
        }
    }

    /// Asserts `result` is the structured `error:{Tag, Elements…}` raise.
    #[track_caller]
    fn assert_structured(result: Result<Eterm>, ctx: &BifContext<'_>, tag: &str, elements: &[Eterm]) {
        let term = structured_reason(result);
        assert_eq!(
            access::tuple_size(term),
            elements.len() + 1,
            "{tag} reason arity"
        );
        assert_eq!(
            access::tuple_element(term, 0),
            ctx.atom(tag).expect("a seeded tag atom"),
            "{tag} reason tag"
        );
        for (i, &expected) in elements.iter().enumerate() {
            assert!(
                eq(access::tuple_element(term, i + 1), expected, ctx.atoms()),
                "{tag} reason element {i}"
            );
        }
    }
    fn atom_of(ctx: &BifContext<'_>, name: &str) -> Eterm {
        ctx.atom(name).expect("a seeded atom")
    }

    /// `#{is_exported => Bool}` built with `access`'s flat-map writer.
    fn opts(ctx: &mut BifContext<'_>, is_exported: bool) -> Eterm {
        let key = atom_of(ctx, "is_exported");
        let value = atom_of(ctx, if is_exported { "true" } else { "false" });
        access::make_flatmap(ctx.heap(), &[(key, value)]).expect("the option map fits")
    }

    /// `#{Field => Value}`.
    fn updates(ctx: &mut BifContext<'_>, field: &str, value: i64) -> Eterm {
        let key = atom_of(ctx, field);
        access::make_flatmap(ctx.heap(), &[(key, make_small(value))]).expect("fits")
    }

    /// `records:create(m, r, [{a,1},{b,2}], #{is_exported => false})`.
    fn sample(ctx: &mut BifContext<'_>) -> Eterm {
        let module = atom_of(ctx, "m");
        let name = atom_of(ctx, "r");
        let pair_a = ctx
            .tuple(&[atom_of(ctx, "a"), make_small(1)])
            .expect("fits");
        let pair_b = ctx
            .tuple(&[atom_of(ctx, "b"), make_small(2)])
            .expect("fits");
        let pairs = ctx.list(&[pair_a, pair_b]).expect("fits");
        let opts = opts(ctx, false);
        create(ctx, &[module, name, pairs, opts]).expect("records:create/4")
    }

    fn list_elements(mut term: Eterm) -> Vec<Eterm> {
        let mut out = Vec::new();
        while is_list(term) {
            out.push(cons_head(term).expect("head"));
            term = cons_tail(term).expect("tail");
        }
        assert!(is_nil(term), "expected a proper list");
        out
    }

    /// The name of the atom term `t`.
    fn name_of(atoms: &AtomTable, t: Eterm) -> String {
        atoms.name(atom_val(t)).expect("an interned atom").to_string()
    }

    #[test]
    fn create_builds_a_record_with_the_oracle_shape() {
        with_ctx(|ctx, atoms| {
            let rec = sample(ctx);
            assert!(access::is_record(rec));
            // `records:get_module(R)` => m; `records:get_name(R)` => r.
            assert_eq!(name_of(atoms, get_module(ctx, &[rec]).unwrap()), "m");
            assert_eq!(name_of(atoms, get_name(ctx, &[rec]).unwrap()), "r");
            // `records:get_field_names(R)` => [a,b] (declaration order).
            let names: Vec<String> = list_elements(get_field_names(ctx, &[rec]).unwrap())
                .into_iter()
                .map(|k| name_of(atoms, k))
                .collect();
            assert_eq!(names, vec!["a", "b"]);
            // `records:is_exported(R)` => false.
            assert_eq!(name_of(atoms, is_exported(ctx, &[rec]).unwrap()), "false");
            // `records:get(a, R)` => 1; `records:get(b, R)` => 2.
            let key_a = atom_of(ctx, "a");
            let key_b = atom_of(ctx, "b");
            assert_eq!(get(ctx, &[key_a, rec]).unwrap(), make_small(1));
            assert_eq!(get(ctx, &[key_b, rec]).unwrap(), make_small(2));

            // The is_exported => true flag round-trips, and an empty record has no
            // field names.
            let module = atom_of(ctx, "m");
            let name = atom_of(ctx, "r");
            let true_opts = opts(ctx, true);
            let rec2 = create(ctx, &[module, name, NIL, true_opts]).unwrap();
            assert_eq!(name_of(atoms, is_exported(ctx, &[rec2]).unwrap()), "true");
            assert!(list_elements(get_field_names(ctx, &[rec2]).unwrap()).is_empty());
        });
    }

    #[test]
    fn is_exported_materializes_true_and_false_even_when_they_are_not_at_indices_0_1() {
        // A record definition's flag word holds the *frozen* `AM_FALSE`/`AM_TRUE`
        // index (`0`/`1`), but the runtime's atom table puts `unix`/`linux` there
        // (`beam_rt::boot::CORE_ATOMS`), so `is_exported/1` must resolve the
        // boolean back through this context's table rather than return the raw
        // word. This table is seeded runtime-style precisely so it *would* catch a
        // regression to that raw word (which read back as `unix`/`linux`).
        with_runtime_ctx(|ctx, atoms| {
            // `records:create(m, r, …, #{is_exported => false})`.
            let not_exported = sample(ctx);
            let term = is_exported(ctx, &[not_exported]).unwrap();
            assert_eq!(name_of(atoms, term), "false");
            assert_eq!(term, ctx.boolean(false).unwrap());

            // The `is_exported => true` flag of a field-less record.
            let module = atom_of(ctx, "m");
            let name = atom_of(ctx, "r");
            let true_opts = opts(ctx, true);
            let exported = create(ctx, &[module, name, NIL, true_opts]).unwrap();
            let term = is_exported(ctx, &[exported]).unwrap();
            assert_eq!(name_of(atoms, term), "true");
            assert_eq!(term, ctx.boolean(true).unwrap());

            // The flag survives a `records:update/4` that rebuilds the instance
            // (the definition, and so the exported flag, is preserved).
            let update_map = updates(ctx, "a", 9);
            let updated =
                update(ctx, &[not_exported, module, name, update_map]).expect("records:update/4");
            assert_eq!(
                name_of(atoms, is_exported(ctx, &[updated]).unwrap()),
                "false"
            );
        });
    }

    #[test]
    fn get_rejects_missing_keys_non_atoms_and_non_records() {
        with_ctx(|ctx, _| {
            let rec = sample(ctx);
            let missing = atom_of(ctx, "other");
            // `records:get(z, R)` => badarg (no such field).
            assert!(matches!(get(ctx, &[missing, rec]), Err(BeamError::BadArg)));
            // `records:get(1, R)` => badarg (non-atom key).
            assert!(matches!(
                get(ctx, &[make_small(1), rec]),
                Err(BeamError::BadArg)
            ));
            // `records:get(a, {})` => badarg (non-record; `{}` is a 0-arity tuple).
            let empty_tuple = ctx.tuple(&[]).unwrap();
            let key_a = atom_of(ctx, "a");
            assert!(matches!(
                get(ctx, &[key_a, empty_tuple]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn introspection_rejects_non_records() {
        with_ctx(|ctx, _| {
            let not_a_record = ctx.tuple(&[make_small(1), make_small(2)]).unwrap();
            // Oracle: records:get_module({1,2}) / records:get_name({1,2}) => error:badarg.
            //   erl -noshell -eval 'try records:get_module({1,2}) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:badarg
            assert!(matches!(get_module(ctx, &[not_a_record]), Err(BeamError::BadArg)));
            assert!(matches!(get_name(ctx, &[not_a_record]), Err(BeamError::BadArg)));
            // Oracle: records:get_field_names({1,2}) / records:is_exported({1,2}) =>
            //   error:{badrecord,{1,2}}.
            //   erl -noshell -eval 'try records:get_field_names({1,2}) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badrecord,{1,2}}
            assert_structured(
                get_field_names(ctx, &[not_a_record]),
                ctx,
                "badrecord",
                &[not_a_record],
            );
            assert_structured(
                is_exported(ctx, &[not_a_record]),
                ctx,
                "badrecord",
                &[not_a_record],
            );
            // A bare immediate is likewise rejected.
            // Oracle: records:get_module(5) => error:badarg; records:get_field_names(5)
            // / records:is_exported(5) => error:{badrecord,5}.
            //   erl -noshell -eval 'try records:get_field_names(5) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badrecord,5}
            assert!(matches!(get_module(ctx, &[make_small(5)]), Err(BeamError::BadArg)));
            assert_structured(
                get_field_names(ctx, &[make_small(5)]),
                ctx,
                "badrecord",
                &[make_small(5)],
            );
            assert_structured(
                is_exported(ctx, &[make_small(5)]),
                ctx,
                "badrecord",
                &[make_small(5)],
            );
        });
    }
    #[test]
    fn create_rejects_malformed_arguments() {
        with_ctx(|ctx, _| {
            let module = atom_of(ctx, "m");
            let name = atom_of(ctx, "r");
            let false_atom = atom_of(ctx, "false");
            let other = atom_of(ctx, "other");
            let is_exported_key = atom_of(ctx, "is_exported");
            let one = make_small(1);

            let good_opts = opts(ctx, false);

            // Oracle: records:create(1, r, [], #{is_exported=>false}) => {badrecord, r}
            // (the reported element is the *name*, not the bad module).
            //   erl -noshell -eval 'try records:create(1, r, [], #{is_exported=>false})
            //     catch C:R -> io:format("~p:~p~n",[C,R]) end, halt().'
            //     =>  error:{badrecord,r}
            assert_structured(
                create(ctx, &[one, name, NIL, good_opts]),
                ctx,
                "badrecord",
                &[name],
            );
            // Oracle: records:create(m, 1, [], #{is_exported=>false}) => {badrecord, 1}.
            assert_structured(
                create(ctx, &[module, one, NIL, good_opts]),
                ctx,
                "badrecord",
                &[one],
            );
            // Oracle: records:create(m, r, [], []) => {badmap, []}.
            //   erl -noshell -eval 'try records:create(m, r, [], []) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,[]}
            assert_structured(
                create(ctx, &[module, name, NIL, NIL]),
                ctx,
                "badmap",
                &[NIL],
            );
            // Empty opts map: badarg.
            let empty_opts = access::make_flatmap(ctx.heap(), &[]).expect("fits");
            assert!(matches!(
                create(ctx, &[module, name, NIL, empty_opts]),
                Err(BeamError::BadArg)
            ));
            // A 2-entry opts map: badarg.
            let two = access::make_flatmap(ctx.heap(), &[(is_exported_key, false_atom), (other, one)])
                .expect("fits");
            assert!(matches!(
                create(ctx, &[module, name, NIL, two]),
                Err(BeamError::BadArg)
            ));
            // A 1-entry opts map whose key is not `is_exported`: badarg.
            let wrong_key =
                access::make_flatmap(ctx.heap(), &[(other, false_atom)]).expect("fits");
            assert!(matches!(
                create(ctx, &[module, name, NIL, wrong_key]),
                Err(BeamError::BadArg)
            ));
            // An `is_exported` value that is neither `true` nor `false`: badarg.
            let bad_value =
                access::make_flatmap(ctx.heap(), &[(is_exported_key, other)]).expect("fits");
            assert!(matches!(
                create(ctx, &[module, name, NIL, bad_value]),
                Err(BeamError::BadArg)
            ));

            // Fields must be a proper list of 2-tuples.
            let good_opts = opts(ctx, false);
            let key_a = atom_of(ctx, "a");
            // Oracle: records:create(m, r, a, #{is_exported=>false}) => badarg.
            assert!(matches!(
                create(ctx, &[module, name, key_a, good_opts]),
                Err(BeamError::BadArg)
            ));
            let pair_a = ctx.tuple(&[key_a, one]).unwrap();
            let improper = ctx.cons(pair_a, other).unwrap();
            let good_opts = opts(ctx, false);
            // Oracle: records:create(m, r, [{a,1}|a], #{is_exported=>false}) => badarg.
            assert!(matches!(
                create(ctx, &[module, name, improper, good_opts]),
                Err(BeamError::BadArg)
            ));
            let triple = ctx.tuple(&[one, one, one]).unwrap();
            let not_a_pair = ctx.list(&[triple]).unwrap();
            let good_opts = opts(ctx, false);
            // Oracle: records:create(m, r, [{1,2,3}], #{is_exported=>false}) => badarg.
            assert!(matches!(
                create(ctx, &[module, name, not_a_pair, good_opts]),
                Err(BeamError::BadArg)
            ));
            // Oracle: records:create(m, r, [{1,1}], #{is_exported=>false})
            //   => {badfield, {{m,r}, 1}}.
            //   erl -noshell -eval 'try records:create(m, r, [{a,1},{a,2},{1,3}],
            //     #{is_exported=>true}) catch C:R -> io:format("~p:~p~n",[C,R]) end,
            //     halt().'  =>  error:{badfield,{{m,r},1}}
            let non_atom_pair = ctx.tuple(&[one, one]).unwrap();
            let non_atom_key = ctx.list(&[non_atom_pair]).unwrap();
            let good_opts = opts(ctx, false);
            let identifier = ctx.tuple(&[module, name]).unwrap();
            assert_structured(
                create(ctx, &[module, name, non_atom_key, good_opts]),
                ctx,
                "badfield",
                &[identifier, one],
            );
            // A duplicate key: badarg.
            let dup_a = ctx.tuple(&[key_a, one]).unwrap();
            let dup_b = ctx.tuple(&[key_a, make_small(2)]).unwrap();
            let dup = ctx.list(&[dup_a, dup_b]).unwrap();
            let good_opts = opts(ctx, false);
            assert!(matches!(
                create(ctx, &[module, name, dup, good_opts]),
                Err(BeamError::BadArg)
            ));
        });
    }    #[test]
    fn update_rebuilds_with_the_new_values() {
        with_ctx(|ctx, atoms| {
            let rec = sample(ctx);
            let module = atom_of(ctx, "m");
            let name = atom_of(ctx, "r");
            let u = updates(ctx, "a", 9);
            // Oracle: records:update(R, m, r, #{a => 9}) => #m:r{a = 9,b = 2}.
            let updated = update(ctx, &[rec, module, name, u]).unwrap();
            assert!(access::is_record(updated));
            let key_a = atom_of(ctx, "a");
            let key_b = atom_of(ctx, "b");
            assert_eq!(get(ctx, &[key_a, updated]).unwrap(), make_small(9));
            assert_eq!(get(ctx, &[key_b, updated]).unwrap(), make_small(2));
            // The definition is preserved (same module/name/is_exported).
            assert_eq!(name_of(atoms, get_module(ctx, &[updated]).unwrap()), "m");
            assert_eq!(name_of(atoms, is_exported(ctx, &[updated]).unwrap()), "false");
        });
    }

    #[test]
    fn update_wildcard_and_empty_map() {
        with_ctx(|ctx, _| {
            let rec = sample(ctx);
            let underscore = atom_of(ctx, "_");
            let u = updates(ctx, "a", 9);
            // `Name = '_'` skips the module/name check.
            let wildcard = update(ctx, &[rec, underscore, underscore, u]).unwrap();
            let key_a = atom_of(ctx, "a");
            assert_eq!(get(ctx, &[key_a, wildcard]).unwrap(), make_small(9));

            // An empty update map returns the original record unchanged.
            let empty = access::make_flatmap(ctx.heap(), &[]).expect("fits");
            let same = update(ctx, &[rec, atom_of(ctx, "m"), atom_of(ctx, "r"), empty]).unwrap();
            assert_eq!(same, rec, "the empty map is a no-op");
        });
    }

    #[test]
    fn update_rejects_bad_arguments() {
        with_ctx(|ctx, _| {
            let rec = sample(ctx);
            let module = atom_of(ctx, "m");
            let name = atom_of(ctx, "r");
            let other = atom_of(ctx, "other");
            let u = updates(ctx, "a", 9);

            // Oracle: records:update({}, m, r, #{a=>9}) => {badrecord, {}}.
            //   erl -noshell -eval 'try records:update({}, m, r, #{a=>9}) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badrecord,{}}
            let not_a_record = ctx.tuple(&[]).unwrap();
            assert_structured(
                update(ctx, &[not_a_record, module, name, u]),
                ctx,
                "badrecord",
                &[not_a_record],
            );
            // Oracle: a name mismatch => {badrecord, other}.
            assert_structured(
                update(ctx, &[rec, module, other, u]),
                ctx,
                "badrecord",
                &[other],
            );
            // A module mismatch reports the *name* => {badrecord, r}.
            //   erl -noshell -eval 'R = records:create(m,r,[{a,1}],#{is_exported=>true}),
            //     try records:update(R, other, r, #{a=>9}) catch C:T ->
            //     io:format("~p:~p~n",[C,T]) end, halt().'  =>  error:{badrecord,r}
            assert_structured(update(ctx, &[rec, other, name, u]), ctx, "badrecord", &[name]);
            // Non-atom module/name (unless wildcard) => {badrecord, r}.
            assert_structured(
                update(ctx, &[rec, make_small(1), name, u]),
                ctx,
                "badrecord",
                &[name],
            );
            // Oracle: updates non-map => {badmap, []}.
            assert_structured(
                update(ctx, &[rec, module, name, NIL]),
                ctx,
                "badmap",
                &[NIL],
            );
            // Oracle: an update key that names no field
            //   => {badfield, {{m,r}, other}}.
            //   erl -noshell -eval 'R = records:create(m,r,[{a,1}],#{is_exported=>true}),
            //     try records:update(R, '"'"'_'"'"', '"'"'_'"'"', #{zz=>9}) catch C:T ->
            //     io:format("~p:~p~n",[C,T]) end, halt().'  =>  error:{badfield,{{m,r},zz}}
            let bad_key = updates(ctx, "other", 9);
            let identifier = ctx.tuple(&[module, name]).unwrap();
            assert_structured(
                update(ctx, &[rec, module, name, bad_key]),
                ctx,
                "badfield",
                &[identifier, other],
            );
        });
    }
    #[test]
    fn get_definition_rejects_non_atom_arguments() {
        with_ctx(|ctx, _| {
            let m = atom_of(ctx, "m");
            let r = atom_of(ctx, "r");
            // Oracle: records:get_definition(1, 2) / (m, 2) / (1, r) => error:badarg.
            //   erl -noshell -eval 'try records:get_definition(m, 2) catch C:R ->
            //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:badarg
            assert!(matches!(
                get_definition(ctx, &[make_small(1), make_small(2)]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                get_definition(ctx, &[m, make_small(2)]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                get_definition(ctx, &[make_small(1), r]),
                Err(BeamError::BadArg)
            ));
        });
    }

    /// A [`RecordSource`] test double: it answers exactly one `(module, name)` pair
    /// with a fixed definition and `Ok(None)` for everything else — the same
    /// contract as the runtime's code server for a record no loaded module defines.
    struct StubRecords {
        module: String,
        name: String,
        definition: RecordDefinitionView,
    }

    impl RecordSource for StubRecords {
        fn record_definition(
            &mut self,
            module: &str,
            name: &str,
        ) -> Result<Option<RecordDefinitionView>> {
            if module == self.module && name == self.name {
                Ok(Some(self.definition.clone()))
            } else {
                Ok(None)
            }
        }
    }

    /// A definition of `m:r` with the fields of the oracle probe below.
    fn sample_definition() -> RecordDefinitionView {
        RecordDefinitionView {
            is_exported: false,
            fields: vec![
                RecordFieldView { name: "a".to_string(), default: None },
                RecordFieldView { name: "b".to_string(), default: Some(make_small(5)) },
                RecordFieldView { name: "other".to_string(), default: None },
            ],
        }
    }

    /// Runs `f` over a fresh context (the seeded [`atoms`]) with `source` installed
    /// as the [`RecordSource`].
    fn with_source_ctx<R>(
        source: &mut dyn RecordSource,
        f: impl FnOnce(&mut BifContext<'_>, &AtomTable) -> R,
    ) -> R {
        let mut heap = Heap::new(4096).expect("test heap");
        let atoms = atoms();
        let mut ctx =
            BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_record_source(source);
        f(&mut ctx, &atoms)
    }

    /// `records:get_definition/2` renders the oracle's exact shape: the 2-tuple
    /// `{#{is_exported => Bool}, Fields}` with the fields in declaration order —
    /// a default-less field as its bare name atom, a defaulted field as `{Name, Default}`.
    ///
    /// Oracle (erts-17.1), a module compiled from `-record #rec{a, b = 5, c}.`:
    ///   erl -noshell -eval 'code:load_file(recmod),
    ///     io:format("~p~n",[records:get_definition(recmod, rec)]), halt().'
    ///   =>  {#{is_exported => false},[a,{b,5},c]}
    #[test]
    fn get_definition_renders_the_oracle_shape() {
        let mut source = StubRecords {
            module: "m".to_string(),
            name: "r".to_string(),
            definition: sample_definition(),
        };
        with_source_ctx(&mut source, |ctx, atoms| {
            let m = atom_of(ctx, "m");
            let r = atom_of(ctx, "r");
            let result = get_definition(ctx, &[m, r]).expect("the source resolves m:r");

            // `{Map, Fields}`.
            assert_eq!(access::tuple_size(result), 2, "the result is a 2-tuple");
            let map = access::tuple_element(result, 0);
            let fields = access::tuple_element(result, 1);

            // The map is a flat map with the single key `is_exported => false`.
            assert!(access::is_flatmap(map), "the definition map is a flat map");
            let (keys, values) = access::flatmap_pairs(map).expect("a flat map");
            assert_eq!(keys.len(), 1, "a single is_exported entry");
            assert_eq!(name_of(atoms, keys[0]), "is_exported");
            assert_eq!(name_of(atoms, values[0]), "false");

            // The fields are `[a, {b,5}, other]` in declaration order.
            let fields = list_elements(fields);
            assert_eq!(fields.len(), 3);
            assert_eq!(name_of(atoms, fields[0]), "a", "a default-less field is a bare atom");
            assert!(access::is_atom(fields[0]));

            let b = fields[1];
            assert!(access::is_tuple(b), "a defaulted field is a 2-tuple");
            assert_eq!(access::tuple_size(b), 2);
            assert_eq!(name_of(atoms, access::tuple_element(b, 0)), "b");
            assert_eq!(access::tuple_element(b, 1), make_small(5));

            assert_eq!(name_of(atoms, fields[2]), "other", "declaration order");
        });
    }

    /// The map's `is_exported` value tracks the definition: an exported record
    /// renders the atom `true`.
    ///
    /// Oracle: `-export_record([exp]).` + `-record #exp{x = 1, y}.` makes
    /// `records:get_definition(recmod, exp)` return `{#{is_exported => true},[{x,1},y]}`.
    #[test]
    fn get_definition_renders_is_exported_true() {
        let mut source = StubRecords {
            module: "m".to_string(),
            name: "r".to_string(),
            definition: RecordDefinitionView {
                is_exported: true,
                fields: vec![
                    RecordFieldView { name: "a".to_string(), default: Some(make_small(1)) },
                    RecordFieldView { name: "b".to_string(), default: None },
                ],
            },
        };
        with_source_ctx(&mut source, |ctx, atoms| {
            let result = get_definition(ctx, &[atom_of(ctx, "m"), atom_of(ctx, "r")]).expect("m:r");
            let map = access::tuple_element(result, 0);
            let (keys, values) = access::flatmap_pairs(map).expect("a flat map");
            assert_eq!(name_of(atoms, keys[0]), "is_exported");
            assert_eq!(name_of(atoms, values[0]), "true");
        });
    }

    /// An unknown `(module, name)` pair (the source answers `None`) is the oracle's
    /// `error:badarg` — the differential gate's `records/get_definition_unknown_record`.
    ///
    /// Oracle: after loading a module defining only `rec`/`exp`,
    /// `records:get_definition(recmod, nope)` ⇒ `error:badarg`; likewise for a module
    /// that defines no such record at all.
    #[test]
    fn get_definition_unknown_record_is_badarg() {
        let mut source = StubRecords {
            module: "m".to_string(),
            name: "r".to_string(),
            definition: sample_definition(),
        };
        with_source_ctx(&mut source, |ctx, _| {
            // Both atoms are seeded and resolvable, but the pair names no definition.
            assert!(matches!(
                get_definition(ctx, &[atom_of(ctx, "m"), atom_of(ctx, "other")]),
                Err(BeamError::BadArg)
            ));
            // A module with no definitions at all.
            assert!(matches!(
                get_definition(ctx, &[atom_of(ctx, "other"), atom_of(ctx, "r")]),
                Err(BeamError::BadArg)
            ));
        });
    }

    /// With no [`RecordSource`] installed, [`BifContext::record_definition`] answers
    /// `Ok(None)` ⇒ `badarg` (never a fabricated definition).
    #[test]
    fn get_definition_without_a_source_is_badarg() {
        with_ctx(|ctx, _| {
            assert!(matches!(
                get_definition(ctx, &[atom_of(ctx, "m"), atom_of(ctx, "r")]),
                Err(BeamError::BadArg)
            ));
        });
    }

    /// A field-name atom the context cannot intern (no `AtomInterner`, name not in
    /// the table) propagates the interner's typed `TermNotFound`, never a fabricated atom.
    #[test]
    fn get_definition_propagates_an_uninternable_field_atom() {
        let mut source = StubRecords {
            module: "m".to_string(),
            name: "r".to_string(),
            definition: RecordDefinitionView {
                is_exported: false,
                fields: vec![RecordFieldView {
                    // Not seeded in [`atoms`] and no interner is installed.
                    name: "not_seeded".to_string(),
                    default: None,
                }],
            },
        };
        with_source_ctx(&mut source, |ctx, _| {
            assert!(matches!(
                get_definition(ctx, &[atom_of(ctx, "m"), atom_of(ctx, "r")]),
                Err(BeamError::TermNotFound)
            ));
        });
    }    #[test]
    fn register_installs_every_records_bif() {
        let mut reg = BifRegistry::new();
        register(&mut reg);
        for (name, arity) in [
            ("get", 2),
            ("get_module", 1),
            ("get_name", 1),
            ("get_field_names", 1),
            ("is_exported", 1),
            ("create", 4),
            ("update", 4),
            ("get_definition", 2),
        ] {
            assert!(reg.contains("records", name, arity), "records:{name}/{arity}");
        }
        assert_eq!(reg.len(), 8);
    }
}
