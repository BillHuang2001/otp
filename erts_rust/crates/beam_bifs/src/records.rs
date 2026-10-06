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
use beam_term::compare::eq;
use beam_term::{atom_val, AtomTable, Eterm};

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
    // A flat map is the only map shape this crate builds/reads (see the crate
    // `CONTEXT.md`); a >32-entry (hash) map also fails the exactly-1 check.
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

/// The update-map value for `key`, matched by exact (`=:=`) equality — the C
/// `records_update_4` key comparison.
fn update_value<'a>(
    update_keys: &[Eterm],
    update_values: &'a [Eterm],
    key: Eterm,
    atoms: &AtomTable,
) -> Option<&'a Eterm> {
    update_keys
        .iter()
        .position(|&k| eq(k, key, atoms))
        .map(|i| &update_values[i])
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
///
/// A >32-entry (hash) update map is [`error::system_limit`] — a typed gap (this crate
/// builds/reads only flat maps), never a wrong result.
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
    if access::is_hashmap(updates) {
        // >32 update keys: a hash map, which this crate neither builds nor reads
        // (the crate `CONTEXT.md` documents the flat-map-only scope).
        return Err(error::system_limit());
    }
    let (update_keys, update_values) = access::flatmap_pairs(updates)?;
    if update_keys.is_empty() {
        // The oracle returns the record unchanged for an empty update map.
        return Ok(rec);
    }

    let keys = access::native_record_declaration_keys(def)?;
    for &update_key in &update_keys {
        if !keys.iter().any(|&key| eq(update_key, key, ctx.atoms())) {
            return Err(badfield(ctx, rec_module, rec_name, update_key)?);
        }
    }

    // Rebuild in declaration order, overlaying the update map's values.
    let mut pairs = Vec::with_capacity(keys.len());
    for &key in &keys {
        let value = match update_value(&update_keys, &update_values, key, ctx.atoms()) {
            Some(&value) => value,
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
/// `erl_record.c:1342-1365`).
///
/// The oracle resolves the pair through the code server's record-definition table
/// (`erts_record_find_entry`, `erl_record.c:163-170`; the BIF itself is
/// `erl_record.c:1342-1403`): an entry with a live definition yields the
/// `[…{Key, Value}… | #{is_exported => Bool}]` list, and *everything else* —
/// a non-atom argument, no entry at all, or an entry whose definition slot for the
/// active code index is `THE_NON_VALUE` — is `error:badarg`.
///
/// # Why this stays a typed gap
///
/// That table is **not** compile-time-only: a `.beam` carries its native-record
/// definitions in the `Recs` chunk, and the *loader* registers them
/// (`erts_record_put(stp->module, rec.records[i].name)` from
/// `../../erts/emulator/beam/jit/asm_load.c:1246-1260` and
/// `../../erts/emulator/beam/emu/emu_load.c:735`, out of `stp->beam.record`; the
/// chunk is parsed by `beam_file.c:1077-1350`, `RECORD_CHUNK = 15`). The compiler
/// puts *every* native record of a module there, exported or not
/// (`lib/compiler/src/beam_asm.erl:577-592`, `beam_core_to_ssa.erl:222-232`).
/// Verified against the oracle: compiling `-record #rec{a, b}.` and loading that
/// module makes `records:get_definition(mod, rec)` return
/// `{#{is_exported => false},[a,b]}` — `badarg` only for a record no *loaded*
/// module defines.
///
/// This runtime loads `.beam` files (contract A.9) but neither parses the `Recs`
/// chunk nor keeps a per-module definition table (`Recs` is captured as opaque
/// bytes in `beam_loader::chunk::misc`), so it cannot tell an *unknown* record
/// from a *known* one. Raising `badarg` unconditionally would therefore be
/// **wrong** for a record a loaded module defines — the honest answer is the typed
/// [`error::unsupported_bif`], never a wrong result and never a fabricated
/// definition. The oracle's atom pre-checks are kept verbatim: a non-atom
/// `Module`/`Name` is `badarg`.
///
/// Closing the gap needs two things outside this crate: `beam_loader` must
/// materialize the `Recs` chunk's definitions, and the runtime must expose a
/// per-module definition registry (the counterpart of `erts_record_put` /
/// `erts_record_module_delete`, `beam_bif_load.c:2451`) for this BIF to consult.
fn get_definition(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    if !access::is_atom(args[0]) || !access::is_atom(args[1]) {
        return Err(error::badarg());
    }
    Err(error::unsupported_bif())
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

    /// `records:get_definition/2` (`records/get_definition_unknown_record` in the
    /// differential corpus) can only be answered honestly as a typed gap, not as a
    /// `badarg`.
    ///
    /// The oracle's table is populated by the **loader** out of a `.beam`'s `Recs`
    /// chunk (`erts_record_put`, `erl_record.c:173-178`; called from
    /// `jit/asm_load.c:1246-1260` and `emu/emu_load.c:735`), so a *loaded* module's
    /// record is resolvable — verified against the oracle: after loading a module
    /// compiled from `-record #rec{a, b}.`, `records:get_definition(Mod, rec)`
    /// returns `{#{is_exported => false},[a,b]}`. This runtime keeps no such table
    /// (`Recs` is opaque bytes in `beam_loader::chunk::misc`), so it cannot tell an
    /// *unknown* record from a *known* one: an unconditional `badarg` would be a
    /// **wrong** result for a loaded module's record. The probe catches this typed
    /// signal ⇒ `unsupported`, never a mismatch.
    #[test]
    fn get_definition_is_a_typed_unsupported() {
        with_ctx(|ctx, _| {
            let m = atom_of(ctx, "m");
            let r = atom_of(ctx, "r");
            // Both atoms, but no loaded module defines `m:r`; the oracle's own answer
            // for that case is `badarg`, which this runtime must not guess at.
            match get_definition(ctx, &[m, r]) {
                Err(BeamError::Raise(raise)) => {
                    assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                    assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
                }
                other => panic!("expected unsupported_bif, got {other:?}"),
            }
        });
    }
    #[test]
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
