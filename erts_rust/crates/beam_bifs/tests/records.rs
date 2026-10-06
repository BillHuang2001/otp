//! Integration tests for the native-record BIFs (`records:*`) and the native half
//! of `erlang:is_record/1,2,3`, driven through [`BifRegistry::call`] over a real
//! `beam_alloc::Heap` + seeded `AtomTable`.
//!
//! Every expected value is pinned against the OTP 29 / erts-17.1 `erl` oracle and
//! cited in a comment. Note that `is_record/2` is translated *by the compiler* for a
//! direct literal call (so a native record answers `false` there); these tests pin
//! the **BIF** behaviour (`apply(erlang, is_record, [R, Name])`), which is what this
//! crate implements:
//!
//! ```erl
//! R = records:create(m, r, [{a,1},{b,2}], #{is_exported => false}),
//! apply(erlang, is_record, [R, r])      => true
//! apply(erlang, is_record, [R, other])  => false
//! apply(erlang, is_record, [R, m, r])   => true
//! apply(erlang, is_record, [R, x, r])   => false
//! ```
//!
//! The record blocks are built through `records:create/4` itself (the option map and
//! the update map through `beam_codec::term::make_flatmap`), so the tests exercise
//! only the public API.

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry};
use beam_codec::term::{cons_head, cons_tail, make_flatmap};
use beam_common::BeamError;
use beam_process::ProcessId;
use beam_term::{atom_val, is_list, is_nil, make_small, AtomTable, Eterm, NIL};

/// The atom table the tests seed: index 0 is `false` and index 1 is `true`, the
/// standard `atom.names` seeding the record `is_exported` atom is built from
/// (`beam_term::AM_FALSE`/`AM_TRUE`).
fn seeded_atoms() -> AtomTable {
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
        "e",
        "other",
    ]);
    atoms
}

/// Runs `f` over the default registry, a real heap and the seeded atom table.
///
/// The heap is leaked on purpose: the results are `Eterm`s that point into it and
/// the tests inspect them after the calls return; freeing it here would make those
/// reads a use-after-free.
fn run<R>(f: impl FnOnce(&BifRegistry, &mut BifContext<'_>, &AtomTable) -> R) -> R {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(65_536).expect("test heap")));
    let mut ctx = BifContext::new(heap, &atoms, ProcessId::new(1, 0));
    f(&reg, &mut ctx, &atoms)
}

/// The name of the atom term `t`.
fn name_of(ctx: &BifContext<'_>, t: Eterm) -> String {
    ctx.atoms()
        .name(atom_val(t))
        .expect("an interned atom")
        .to_string()
}

/// The elements of a proper list.
fn elements(mut term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    while is_list(term) {
        out.push(cons_head(term).expect("a cons head"));
        term = cons_tail(term).expect("a cons tail");
    }
    assert!(is_nil(term), "expected a proper list");
    out
}

/// The boolean value of a `true`/`false` atom term.
fn truth(ctx: &BifContext<'_>, t: Eterm) -> bool {
    match ctx.atoms().name(atom_val(t)) {
        Some("true") => true,
        Some("false") => false,
        other => panic!("expected a boolean atom, got {other:?}"),
    }
}

/// A flat map of the precomputed `(key, value)` pairs.
fn map_of(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Eterm {
    make_flatmap(ctx.heap(), pairs).expect("the map fits")
}

/// `#{is_exported => Bool}`.
fn opts(ctx: &mut BifContext<'_>, is_exported: bool) -> Eterm {
    let key = ctx.atom("is_exported").expect("seeded");
    let value = ctx
        .atom(if is_exported { "true" } else { "false" })
        .expect("seeded");
    map_of(ctx, &[(key, value)])
}

/// `#{Field => Value}`.
fn updates(ctx: &mut BifContext<'_>, field: &str, value: i64) -> Eterm {
    let key = ctx.atom(field).expect("seeded");
    map_of(ctx, &[(key, make_small(value))])
}

/// `records:create(Module, Name, Fields, #{is_exported => Bool})`.
fn create(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    name: &str,
    fields: &[(&str, i64)],
    is_exported: bool,
) -> Eterm {
    let module_t = ctx.atom(module).expect("seeded");
    let name_t = ctx.atom(name).expect("seeded");
    let tuples: Vec<Eterm> = fields
        .iter()
        .map(|&(key, value)| {
            let key = ctx.atom(key).expect("seeded");
            let value = ctx.int(value).expect("fits");
            ctx.tuple(&[key, value]).expect("the pair fits")
        })
        .collect();
    let fields = ctx.list(&tuples).expect("the field list fits");
    let opts = opts(ctx, is_exported);
    reg.call("records", "create", 4, ctx, &[module_t, name_t, fields, opts])
        .expect("records:create/4")
}

/// `records:get_field_names/1` as atom names.
fn field_names(reg: &BifRegistry, ctx: &mut BifContext<'_>, rec: Eterm) -> Vec<String> {
    let list = reg
        .call("records", "get_field_names", 1, ctx, &[rec])
        .expect("records:get_field_names/1");
    elements(list)
        .into_iter()
        .map(|key| name_of(ctx, key))
        .collect()
}

/// A small-int field of `rec` through `records:get/2`.
fn field(reg: &BifRegistry, ctx: &mut BifContext<'_>, rec: Eterm, key: &str) -> i64 {
    let key = ctx.atom(key).expect("seeded");
    let value = reg
        .call("records", "get", 2, ctx, &[key, rec])
        .expect("records:get/2");
    beam_term::signed_value(value)
}

/// `erlang:is_record/`, returning the boolean.
fn is_record(reg: &BifRegistry, ctx: &mut BifContext<'_>, args: &[Eterm]) -> bool {
    let arity = args.len() as u32;
    let result = reg
        .call("erlang", "is_record", arity, ctx, args)
        .expect("erlang:is_record");
    truth(ctx, result)
}

#[test]
fn create_builds_a_record_the_oracle_introspects() {
    run(|reg, ctx, _| {
        let rec = create(reg, ctx, "m", "r", &[("a", 1), ("b", 2)], false);
        // Oracle: is_record(R) => true.
        assert!(is_record(reg, ctx, &[rec]));
        // Oracle: records:get_module(R) => m; records:get_name(R) => r.
        let module = reg
            .call("records", "get_module", 1, ctx, &[rec])
            .expect("get_module");
        assert_eq!(name_of(ctx, module), "m");
        let name = reg
            .call("records", "get_name", 1, ctx, &[rec])
            .expect("get_name");
        assert_eq!(name_of(ctx, name), "r");
        // Oracle: records:get_field_names(R) => [a,b] (declaration order).
        assert_eq!(field_names(reg, ctx, rec), vec!["a", "b"]);
        // Oracle: records:is_exported(R) => false.
        let exported = reg
            .call("records", "is_exported", 1, ctx, &[rec])
            .expect("is_exported");
        assert_eq!(name_of(ctx, exported), "false");
        // Oracle: records:get(a, R) => 1, records:get(b, R) => 2.
        assert_eq!(field(reg, ctx, rec, "a"), 1);
        assert_eq!(field(reg, ctx, rec, "b"), 2);
    });
}

#[test]
fn create_with_no_fields_yields_an_empty_record() {
    run(|reg, ctx, _| {
        let rec = create(reg, ctx, "m", "e", &[], false);
        // Oracle: records:create(m, e, [], #{is_exported => false}) => #m:e{}
        // and records:get_field_names(R) => [].
        assert!(is_record(reg, ctx, &[rec]));
        assert!(field_names(reg, ctx, rec).is_empty());
        // The is_exported => true flag round-trips.
        let rec_true = create(reg, ctx, "m", "e", &[], true);
        let exported = reg
            .call("records", "is_exported", 1, ctx, &[rec_true])
            .expect("is_exported");
        assert_eq!(name_of(ctx, exported), "true");
    });
}

#[test]
fn native_is_record_2_and_3_match_the_oracle() {
    run(|reg, ctx, _| {
        let rec = create(reg, ctx, "m", "r", &[("a", 1), ("b", 2)], false);
        let m = ctx.atom("m").expect("seeded");
        let r = ctx.atom("r").expect("seeded");
        let other = ctx.atom("other").expect("seeded");

        // Oracle: apply(erlang, is_record, [R, r]) => true, [R, other] => false.
        assert!(is_record(reg, ctx, &[rec, r]));
        assert!(!is_record(reg, ctx, &[rec, other]));
        // Oracle: apply(erlang, is_record, [R, m, r]) => true; [R, x, r] => false.
        assert!(is_record(reg, ctx, &[rec, m, r]));
        assert!(!is_record(reg, ctx, &[rec, other, r]));

        // A plain tuple keeps the classic tuple semantics.
        let tuple = ctx.tuple(&[r, make_small(1), make_small(2)]).expect("fits");
        // Oracle: is_record({r,1,2}, r) => true; is_record({r,1,2}, m, r) => false;
        // is_record({r,1,2}, r, 3) => true.
        assert!(is_record(reg, ctx, &[tuple, r]));
        assert!(!is_record(reg, ctx, &[tuple, m, r]));
        assert!(is_record(reg, ctx, &[tuple, r, make_small(3)]));

        // A non-atom tag is badarg.
        assert!(matches!(
            reg.call("erlang", "is_record", 2, ctx, &[rec, make_small(1)]),
            Err(BeamError::BadArg)
        ));
    });
}

#[test]
fn get_and_introspection_badarg_cases() {
    run(|reg, ctx, _| {
        let rec = create(reg, ctx, "m", "r", &[("a", 1), ("b", 2)], false);
        let a = ctx.atom("a").expect("seeded");
        let other = ctx.atom("other").expect("seeded");
        let empty_tuple = ctx.tuple(&[]).expect("fits");

        // Oracle: records:get(z, R) => badarg (no such field).
        assert!(matches!(
            reg.call("records", "get", 2, ctx, &[other, rec]),
            Err(BeamError::BadArg)
        ));
        // Oracle: records:get(1, R) => badarg (non-atom key).
        assert!(matches!(
            reg.call("records", "get", 2, ctx, &[make_small(1), rec]),
            Err(BeamError::BadArg)
        ));
        // Oracle: records:get(a, {}) => badarg (non-record).
        assert!(matches!(
            reg.call("records", "get", 2, ctx, &[a, empty_tuple]),
            Err(BeamError::BadArg)
        ));
        // Oracle: records:get_module({}) / get_name(1) / get_field_names({}) /
        // is_exported(x) => badarg (`{badrecord, _}` collapsed).
        for (name, args) in [
            ("get_module", vec![empty_tuple]),
            ("get_name", vec![make_small(1)]),
            ("get_field_names", vec![empty_tuple]),
            ("is_exported", vec![other]),
        ] {
            let arity = args.len() as u32;
            assert!(
                matches!(
                    reg.call("records", name, arity, ctx, &args),
                    Err(BeamError::BadArg)
                ),
                "records:{name}/{arity} must be badarg"
            );
        }
    });
}

#[test]
fn update_rebuilds_values_and_supports_the_wildcard_and_empty_noop() {
    run(|reg, ctx, _| {
        let rec = create(reg, ctx, "m", "r", &[("a", 1), ("b", 2)], false);
        let m = ctx.atom("m").expect("seeded");
        let r = ctx.atom("r").expect("seeded");
        let underscore = ctx.atom("_").expect("seeded");
        let u = updates(ctx, "a", 9);

        // Oracle: records:update(R, m, r, #{a => 9}) => #m:r{a = 9,b = 2}.
        let updated = reg
            .call("records", "update", 4, ctx, &[rec, m, r, u])
            .expect("records:update/4");
        assert_eq!(field(reg, ctx, updated, "a"), 9);
        assert_eq!(field(reg, ctx, updated, "b"), 2);
        // The definition is preserved.
        let module = reg
            .call("records", "get_module", 1, ctx, &[updated])
            .expect("get_module");
        assert_eq!(name_of(ctx, module), "m");

        // Oracle: records:update(R, '_', '_', #{a => 9}) => #m:r{a = 9,b = 2}.
        let wildcard = reg
            .call("records", "update", 4, ctx, &[rec, underscore, underscore, u])
            .expect("records:update/4 with the wildcard");
        assert_eq!(field(reg, ctx, wildcard, "a"), 9);

        // Oracle: records:update(R, m, r, #{}) => R (unchanged).
        let empty = map_of(ctx, &[]);
        let same = reg
            .call("records", "update", 4, ctx, &[rec, m, r, empty])
            .expect("records:update/4 with an empty map");
        assert_eq!(same, rec, "the empty update map is a no-op");
    });
}

#[test]
fn update_badarg_cases() {
    run(|reg, ctx, _| {
        let rec = create(reg, ctx, "m", "r", &[("a", 1), ("b", 2)], false);
        let m = ctx.atom("m").expect("seeded");
        let r = ctx.atom("r").expect("seeded");
        let other = ctx.atom("other").expect("seeded");
        let empty_tuple = ctx.tuple(&[]).expect("fits");
        let u = updates(ctx, "a", 9);
        let bad_key = updates(ctx, "other", 9);

        // Oracle: records:update({}, m, r, #{}) => {badrecord, {}} (collapsed).
        assert!(matches!(
            reg.call("records", "update", 4, ctx, &[empty_tuple, m, r, u]),
            Err(BeamError::BadArg)
        ));
        // Oracle: a name mismatch => {badrecord, Name} (collapsed).
        assert!(matches!(
            reg.call("records", "update", 4, ctx, &[rec, m, other, u]),
            Err(BeamError::BadArg)
        ));
        // A module mismatch likewise.
        assert!(matches!(
            reg.call("records", "update", 4, ctx, &[rec, other, r, u]),
            Err(BeamError::BadArg)
        ));
        // Non-atom module/name: {badrecord, Name} (collapsed).
        assert!(matches!(
            reg.call("records", "update", 4, ctx, &[rec, make_small(1), r, u]),
            Err(BeamError::BadArg)
        ));
        // Oracle: updates non-map => {badmap, Updates} (collapsed).
        assert!(matches!(
            reg.call("records", "update", 4, ctx, &[rec, m, r, NIL]),
            Err(BeamError::BadArg)
        ));
        // Oracle: an unknown update key => {badfield, ...} (collapsed).
        assert!(matches!(
            reg.call("records", "update", 4, ctx, &[rec, m, r, bad_key]),
            Err(BeamError::BadArg)
        ));
    });
}

#[test]
fn create_badarg_cases() {
    run(|reg, ctx, _| {
        let m = ctx.atom("m").expect("seeded");
        let r = ctx.atom("r").expect("seeded");
        let a = ctx.atom("a").expect("seeded");
        let false_atom = ctx.atom("false").expect("seeded");
        let other = ctx.atom("other").expect("seeded");
        let is_exported = ctx.atom("is_exported").expect("seeded");
        let one = make_small(1);
        let o = opts(ctx, false);

        let call = |reg: &BifRegistry,
                    ctx: &mut BifContext<'_>,
                    module: Eterm,
                    name: Eterm,
                    fields: Eterm,
                    opts: Eterm| {
            reg.call("records", "create", 4, ctx, &[module, name, fields, opts])
        };

        // Oracle: {badrecord, r} for a non-atom module; {badrecord, 1} for a name.
        assert!(matches!(call(reg, ctx, one, r, NIL, o), Err(BeamError::BadArg)));
        assert!(matches!(call(reg, ctx, m, one, NIL, o), Err(BeamError::BadArg)));
        // Oracle: {badmap, []} for a non-map opts.
        assert!(matches!(call(reg, ctx, m, r, NIL, NIL), Err(BeamError::BadArg)));
        // Oracle: badarg for an empty opts map.
        let empty_opts = map_of(ctx, &[]);
        assert!(matches!(
            call(reg, ctx, m, r, NIL, empty_opts),
            Err(BeamError::BadArg)
        ));
        // Oracle: badarg for a 2-entry opts map.
        let two = map_of(ctx, &[(is_exported, false_atom), (other, one)]);
        assert!(matches!(call(reg, ctx, m, r, NIL, two), Err(BeamError::BadArg)));
        // Oracle: badarg for an opts map without the is_exported key.
        let wrong_key = map_of(ctx, &[(other, false_atom)]);
        assert!(matches!(
            call(reg, ctx, m, r, NIL, wrong_key),
            Err(BeamError::BadArg)
        ));
        // Oracle: badarg for an is_exported value that is neither true nor false.
        let bad_value = map_of(ctx, &[(is_exported, other)]);
        assert!(matches!(
            call(reg, ctx, m, r, NIL, bad_value),
            Err(BeamError::BadArg)
        ));

        // Oracle: badarg for a non-list Fields.
        assert!(matches!(call(reg, ctx, m, r, a, o), Err(BeamError::BadArg)));
        // Oracle: badarg for an improper Fields list.
        let pair = ctx.tuple(&[a, one]).expect("fits");
        let improper = ctx.cons(pair, a).expect("fits");
        assert!(matches!(
            call(reg, ctx, m, r, improper, o),
            Err(BeamError::BadArg)
        ));
        // Oracle: badarg for a non-2-tuple element.
        let triple = ctx.tuple(&[one, one, one]).expect("fits");
        let not_a_pair = ctx.list(&[triple]).expect("fits");
        assert!(matches!(
            call(reg, ctx, m, r, not_a_pair, o),
            Err(BeamError::BadArg)
        ));
        // Oracle: {badfield, {{m,r},1}} for a non-atom key (collapsed).
        let non_atom_pair = ctx.tuple(&[one, one]).expect("fits");
        let non_atom_key = ctx.list(&[non_atom_pair]).expect("fits");
        assert!(matches!(
            call(reg, ctx, m, r, non_atom_key, o),
            Err(BeamError::BadArg)
        ));
        // Oracle: badarg for a duplicate key.
        let dup_a = ctx.tuple(&[a, one]).expect("fits");
        let dup_b = ctx.tuple(&[a, make_small(2)]).expect("fits");
        let dup = ctx.list(&[dup_a, dup_b]).expect("fits");
        assert!(matches!(call(reg, ctx, m, r, dup, o), Err(BeamError::BadArg)));
    });
}

#[test]
fn get_definition_is_a_typed_unsupported_for_atoms() {
    run(|reg, ctx, _| {
        let m = ctx.atom("m").expect("seeded");
        let r = ctx.atom("r").expect("seeded");
        // Oracle: records:get_definition(1, r) => badarg.
        assert!(matches!(
            reg.call("records", "get_definition", 2, ctx, &[make_small(1), r]),
            Err(BeamError::BadArg)
        ));
        // The (unimplementable) code-table lookup is a typed gap.
        match reg.call("records", "get_definition", 2, ctx, &[m, r]) {
            Err(BeamError::Raise(raise)) => {
                assert_eq!(raise.class, beam_common::ExceptionClass::Error);
                assert_eq!(raise.reason, beam_common::ExitReason::atom("unsupported_bif"));
            }
            other => panic!("expected unsupported_bif, got {other:?}"),
        }
    });
}

#[test]
fn the_registry_registers_every_records_bif() {
    let reg = default_registry();
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
    let records_keys: Vec<_> = reg
        .names()
        .into_iter()
        .filter(|(m, _, _)| m == "records")
        .collect();
    assert_eq!(records_keys.len(), 8, "got {records_keys:?}");
}
