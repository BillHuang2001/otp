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
//! # Structured reasons collapse to `badarg`
//!
//! WHERE the oracle raises a *structured* reason — `{badrecord, Term}`,
//! `{badfield, {{Module, Name}, Key}}`, `{badmap, Map}` — this crate reports
//! `error::badarg()`. That is the documented `beam_common::ExitReason::Structured`
//! "has no term" convention (see the crate `CONTEXT.md`): the reason's payload is
//! not representable, so a fake atom is never invented. The plain `badarg` cases
//! (`badarg` in the oracle) are reproduced exactly.
//!
//! Every BIF here is registered under the `records` module from [`register`], which
//! [`crate::registry::BifRegistry::with_core_bifs`] calls.

use beam_common::Result;
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
/// The oracle's `badrecord`-for-a-non-record collapse is [`error::badarg`].
fn definition_of(term: Eterm) -> Result<Eterm> {
    if !access::is_record(term) {
        return Err(error::badarg());
    }
    access::native_record_definition(term)
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
/// non-record is `badrecord` (collapsed to `badarg`).
fn get_field_names(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let def = definition_of(args[0])?;
    let keys = access::native_record_declaration_keys(def)?;
    ctx.list(&keys)
}

/// `records:is_exported/1` (`records_is_exported_1`, `erl_record.c:1328-1340`):
/// the definition's stored `true`/`false` atom; a non-record is `badrecord`
/// (collapsed to `badarg`).
fn is_exported(_ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let def = definition_of(args[0])?;
    access::native_record_definition_is_exported(def)
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
/// `badarg` for a malformed module/name/opts/fields argument (the oracle's
/// `{badrecord, _}`/`{badmap, _}`/`{badfield, _}` reasons all collapse to
/// `badarg`; the oracle's plain `badarg` cases are reproduced as `badarg`).
fn create(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (module, name, fields, opts) = (args[0], args[1], args[2], args[3]);
    if !access::is_atom(module) || !access::is_atom(name) {
        // `{badrecord, Name}` (collapsed).
        return Err(error::badarg());
    }
    if !access::is_map(opts) {
        // `{badmap, Opts}` (collapsed).
        return Err(error::badarg());
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
/// `badarg` for a non-record, a module/name mismatch, a non-map `Updates`, or an
/// unknown update key (structured reasons collapse). A >32-entry (hash) update
/// map is [`error::system_limit`] — a typed gap (this crate builds/reads only flat
/// maps), never a wrong result.
fn update(ctx: &mut BifContext<'_>, args: &[Eterm]) -> Result<Eterm> {
    let (rec, module, name, updates) = (args[0], args[1], args[2], args[3]);
    if !access::is_record(rec) {
        // `{badrecord, Record}` (collapsed).
        return Err(error::badarg());
    }
    let def = access::native_record_definition(rec)?;
    let rec_module = access::native_record_definition_module(def)?;
    let rec_name = access::native_record_definition_name(def)?;
    let rec_exported = access::native_record_definition_is_exported(def)?;

    // `Name == '_'` skips the module/name check entirely (`records_update_4`).
    if !atom_name_is(ctx, name, "_") {
        if !access::is_atom(module) || !access::is_atom(name) {
            // `{badrecord, Name}` (collapsed).
            return Err(error::badarg());
        }
        if rec_module != module || rec_name != name {
            // A record module/name mismatch is `{badrecord, Name}` (collapsed).
            return Err(error::badarg());
        }
    }

    if !access::is_map(updates) {
        // `{badmap, Updates}` (collapsed).
        return Err(error::badarg());
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
            // `{badfield, {{Module, Name}, Key}}` (collapsed).
            return Err(error::badarg());
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
/// The oracle looks the record up in the code server's compile-time record table
/// (`erts_record_find_entry`), which this crate has no access to — a typed
/// [`error::unsupported_bif`], never a wrong result. The atom pre-checks are kept:
/// a non-atom `Module`/`Name` is the oracle's `badarg`.
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
            for result in [
                get_module(ctx, &[not_a_record]),
                get_name(ctx, &[not_a_record]),
                get_field_names(ctx, &[not_a_record]),
                is_exported(ctx, &[not_a_record]),
            ] {
                assert!(matches!(result, Err(BeamError::BadArg)));
            }
            // A bare immediate is likewise rejected.
            assert!(matches!(
                get_module(ctx, &[make_small(7)]),
                Err(BeamError::BadArg)
            ));
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

            // Non-atom module/name: `{badrecord, Name}` (collapsed).
            assert!(matches!(
                create(ctx, &[make_small(1), name, NIL, good_opts]),
                Err(BeamError::BadArg)
            ));
            assert!(matches!(
                create(ctx, &[module, make_small(1), NIL, good_opts]),
                Err(BeamError::BadArg)
            ));
            // Opts non-map: `{badmap, Opts}` (collapsed).
            assert!(matches!(
                create(ctx, &[module, name, NIL, NIL]),
                Err(BeamError::BadArg)
            ));
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
            assert!(matches!(
                create(ctx, &[module, name, key_a, good_opts]),
                Err(BeamError::BadArg)
            ));
            let pair_a = ctx.tuple(&[key_a, one]).unwrap();
            let improper = ctx.cons(pair_a, other).unwrap();
            let good_opts = opts(ctx, false);
            assert!(matches!(
                create(ctx, &[module, name, improper, good_opts]),
                Err(BeamError::BadArg)
            ));
            let triple = ctx.tuple(&[one, one, one]).unwrap();
            let not_a_pair = ctx.list(&[triple]).unwrap();
            let good_opts = opts(ctx, false);
            assert!(matches!(
                create(ctx, &[module, name, not_a_pair, good_opts]),
                Err(BeamError::BadArg)
            ));
            // A non-atom key: `{badfield, ...}` (collapsed).
            let non_atom_pair = ctx.tuple(&[one, one]).unwrap();
            let non_atom_key = ctx.list(&[non_atom_pair]).unwrap();
            let good_opts = opts(ctx, false);
            assert!(matches!(
                create(ctx, &[module, name, non_atom_key, good_opts]),
                Err(BeamError::BadArg)
            ));
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
    }

    #[test]
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

            // Non-record: `{badrecord, Record}` (collapsed).
            let not_a_record = ctx.tuple(&[]).unwrap();
            assert!(matches!(
                update(ctx, &[not_a_record, module, name, u]),
                Err(BeamError::BadArg)
            ));
            // A name mismatch: `{badrecord, Name}` (collapsed).
            assert!(matches!(
                update(ctx, &[rec, module, other, u]),
                Err(BeamError::BadArg)
            ));
            // A module mismatch too.
            assert!(matches!(
                update(ctx, &[rec, other, name, u]),
                Err(BeamError::BadArg)
            ));
            // Non-atom module/name (unless wildcard): `{badrecord, Name}` (collapsed).
            assert!(matches!(
                update(ctx, &[rec, make_small(1), name, u]),
                Err(BeamError::BadArg)
            ));
            // Updates non-map: `{badmap, Updates}` (collapsed).
            assert!(matches!(
                update(ctx, &[rec, module, name, NIL]),
                Err(BeamError::BadArg)
            ));
            // An update key that names no field: `{badfield, ...}` (collapsed).
            let bad_key = updates(ctx, "other", 9);
            assert!(matches!(
                update(ctx, &[rec, module, name, bad_key]),
                Err(BeamError::BadArg)
            ));
        });
    }

    #[test]
    fn get_definition_is_a_typed_unsupported() {
        with_ctx(|ctx, _| {
            let r = atom_of(ctx, "r");
            // Non-atom arguments are the oracle's `badarg`.
            assert!(matches!(
                get_definition(ctx, &[make_small(1), r]),
                Err(BeamError::BadArg)
            ));
            // The (unimplementable) code-table lookup is a typed gap.
            let m = atom_of(ctx, "m");
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
