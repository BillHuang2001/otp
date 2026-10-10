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
use beam_bifs::{
    default_registry, BifContext, BifRegistry, RecordDefinitionView, RecordFieldView, RecordSource,
};
use beam_codec::term::{cons_head, cons_tail, flatmap_parts, make_flatmap, tuple_arity, tuple_element};
use beam_common::{BeamError, Result};
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
        // The structured-reason tags of `records:*` (`error:{badrecord,_}`,
        // `{badmap,_}`, `{badfield,_}`).
        "badrecord",
        "badmap",
        "badfield",
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

/// Asserts `result` is the oracle's structured raise whose live reason term is
/// structurally equal to `expected`.
///
/// The full contract this crate now honours: `Err(BeamError::Raise(r))`,
/// `r.reason == ExitReason::Structured` (so `BeamError::as_atom()` is `None`) and
/// `r.live_reason()` carrying the real reason term.
#[track_caller]
fn assert_structured(result: Result<Eterm>, ctx: &BifContext<'_>, expected: Eterm) {
    match result {
        Err(BeamError::Raise(raise)) => {
            assert_eq!(raise.class, beam_common::ExceptionClass::Error, "class");
            assert_eq!(raise.reason, beam_common::ExitReason::Structured, "reason");
            assert_eq!(raise.reason.as_atom(), None, "as_atom");
            let live = *raise
                .live_reason()
                .expect("a structured reason carries its live term")
                .downcast_ref::<Eterm>()
                .expect("the payload is the live reason term");
            assert!(
                beam_term::compare::eq(live, expected, ctx.atoms()),
                "live reason differs from the expected reason tuple"
            );
        }
        other => panic!("expected a structured raise, got {other:?}"),
    }
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
        // Oracle: records:get_module({}) / records:get_name(1) => error:badarg.
        //   erl -noshell -eval 'try records:get_module({}) catch C:R ->
        //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:badarg
        for (name, args) in [
            ("get_module", vec![empty_tuple]),
            ("get_name", vec![make_small(1)]),
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
        // Oracle: records:get_field_names({}) / records:is_exported(other)
        //   => error:{badrecord, Term}.
        //   erl -noshell -eval 'try records:get_field_names({}) catch C:R ->
        //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badrecord,{}}
        let expected_empty = ctx.tuple(&[ctx.atom("badrecord").expect("seeded"), empty_tuple]).expect("fits");
        assert_structured(
            reg.call("records", "get_field_names", 1, ctx, &[empty_tuple]),
            ctx,
            expected_empty,
        );
        let expected_other = ctx.tuple(&[ctx.atom("badrecord").expect("seeded"), other]).expect("fits");
        assert_structured(
            reg.call("records", "is_exported", 1, ctx, &[other]),
            ctx,
            expected_other,
        );
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
        let badrecord = ctx.atom("badrecord").expect("seeded");
        let badmap = ctx.atom("badmap").expect("seeded");
        let badfield = ctx.atom("badfield").expect("seeded");

        // Oracle: records:update({}, m, r, #{}) => error:{badrecord, {}}.
        //   erl -noshell -eval 'try records:update({}, m, r, #{a=>9}) catch C:R ->
        //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badrecord,{}}
        let expected = ctx.tuple(&[badrecord, empty_tuple]).expect("fits");
        assert_structured(
            reg.call("records", "update", 4, ctx, &[empty_tuple, m, r, u]),
            ctx,
            expected,
        );
        // Oracle: a name mismatch => error:{badrecord, other}.
        let expected = ctx.tuple(&[badrecord, other]).expect("fits");
        assert_structured(
            reg.call("records", "update", 4, ctx, &[rec, m, other, u]),
            ctx,
            expected,
        );
        // A module mismatch reports the *name* => error:{badrecord, r}.
        //   erl -noshell -eval 'R=records:create(m,r,[{a,1}],#{is_exported=>true}),
        //     try records:update(R, other, r, #{a=>9}) catch C:T ->
        //     io:format("~p:~p~n",[C,T]) end, halt().'  =>  error:{badrecord,r}
        let expected = ctx.tuple(&[badrecord, r]).expect("fits");
        assert_structured(
            reg.call("records", "update", 4, ctx, &[rec, other, r, u]),
            ctx,
            expected,
        );
        // Non-atom module/name => error:{badrecord, r}.
        let expected = ctx.tuple(&[badrecord, r]).expect("fits");
        assert_structured(
            reg.call("records", "update", 4, ctx, &[rec, make_small(1), r, u]),
            ctx,
            expected,
        );
        // Oracle: updates non-map => error:{badmap, []}.
        let expected = ctx.tuple(&[badmap, NIL]).expect("fits");
        assert_structured(
            reg.call("records", "update", 4, ctx, &[rec, m, r, NIL]),
            ctx,
            expected,
        );
        // Oracle: an unknown update key => error:{badfield, {{m,r}, other}}.
        //   erl -noshell -eval 'R=records:create(m,r,[{a,1}],#{is_exported=>true}),
        //     try records:update(R, '"'"'_'"'"', '"'"'_'"'"', #{zz=>9}) catch C:T ->
        //     io:format("~p:~p~n",[C,T]) end, halt().'  =>  error:{badfield,{{m,r},zz}}
        let m_r = ctx.tuple(&[m, r]).expect("fits");
        let pair = ctx.tuple(&[m_r, other]).expect("fits");
        let expected = ctx.tuple(&[badfield, pair]).expect("fits");
        assert_structured(
            reg.call("records", "update", 4, ctx, &[rec, m, r, bad_key]),
            ctx,
            expected,
        );
        // Oracle: the '_' wildcard reports the RECORD's own module/name
        //   => error:{badfield, {{m,r}, other}}.
        //   erl -noshell -eval 'R=records:create(m,r,[{a,1}],#{is_exported=>true}),
        //     try records:update(R, '_', '_', #{other=>9}) catch C:T ->
        //     io:format("~p:~p~n",[C,T]) end, halt().'  =>  error:{badfield,{{m,r},other}}
        let underscore = ctx.atom("_").expect("seeded");
        let expected = ctx.tuple(&[badfield, pair]).expect("fits");
        assert_structured(
            reg.call(
                "records",
                "update",
                4,
                ctx,
                &[rec, underscore, underscore, bad_key],
            ),
            ctx,
            expected,
        );
        // Oracle: a non-atom update key => error:{badfield, {{m,r}, 1}}.
        //   erl -noshell -eval 'R=records:create(m,r,[{a,1}],#{is_exported=>true}),
        //     try records:update(R, m, r, #{1=>1}) catch C:T ->
        //     io:format("~p:~p~n",[C,T]) end, halt().'  =>  error:{badfield,{{m,r},1}}
        let non_atom_update = map_of(ctx, &[(make_small(1), make_small(1))]);
        let pair = ctx.tuple(&[m_r, make_small(1)]).expect("fits");
        let expected = ctx.tuple(&[badfield, pair]).expect("fits");
        assert_structured(
            reg.call("records", "update", 4, ctx, &[rec, m, r, non_atom_update]),
            ctx,
            expected,
        );
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

        let badrecord = ctx.atom("badrecord").expect("seeded");
        let badmap = ctx.atom("badmap").expect("seeded");
        let badfield = ctx.atom("badfield").expect("seeded");

        // Oracle: records:create(1, r, [], #{is_exported=>false}) => {badrecord, r}
        // (the reported element is the *name*, not the bad module).
        //   erl -noshell -eval 'try records:create(1, r, [], #{is_exported=>false})
        //     catch C:R -> io:format("~p:~p~n",[C,R]) end, halt().'
        //     =>  error:{badrecord,r}
        let expected = ctx.tuple(&[badrecord, r]).expect("fits");
        assert_structured(call(reg, ctx, one, r, NIL, o), ctx, expected);
        // Oracle: records:create(m, 1, [], #{is_exported=>false}) => {badrecord, 1}.
        let expected = ctx.tuple(&[badrecord, one]).expect("fits");
        assert_structured(call(reg, ctx, m, one, NIL, o), ctx, expected);
        // Oracle: records:create(m, r, [], []) => {badmap, []}.
        //   erl -noshell -eval 'try records:create(m, r, [], []) catch C:R ->
        //     io:format("~p:~p~n",[C,R]) end, halt().'  =>  error:{badmap,[]}
        let expected = ctx.tuple(&[badmap, NIL]).expect("fits");
        assert_structured(call(reg, ctx, m, r, NIL, NIL), ctx, expected);
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
        // Oracle: records:create(m, r, [{1,1}], #{is_exported=>false})
        //   => {badfield, {{m,r}, 1}}.
        //   erl -noshell -eval 'try records:create(m, r, [{a,1},{a,2},{1,3}],
        //     #{is_exported=>true}) catch C:R -> io:format("~p:~p~n",[C,R]) end,
        //     halt().'  =>  error:{badfield,{{m,r},1}}
        let non_atom_pair = ctx.tuple(&[one, one]).expect("fits");
        let non_atom_key = ctx.list(&[non_atom_pair]).expect("fits");
        let m_r = ctx.tuple(&[m, r]).expect("fits");
        let pair = ctx.tuple(&[m_r, one]).expect("fits");
        let expected = ctx.tuple(&[badfield, pair]).expect("fits");
        assert_structured(call(reg, ctx, m, r, non_atom_key, o), ctx, expected);
        // Oracle: badarg for a duplicate key.
        let dup_a = ctx.tuple(&[a, one]).expect("fits");
        let dup_b = ctx.tuple(&[a, make_small(2)]).expect("fits");
        let dup = ctx.list(&[dup_a, dup_b]).expect("fits");
        assert!(matches!(call(reg, ctx, m, r, dup, o), Err(BeamError::BadArg)));
    });
}

/// A [`RecordSource`] test double: it answers exactly one `(module, name)` pair
/// with a fixed definition and `Ok(None)` for everything else (the runtime's code
/// server for a record no loaded module defines).
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

/// Runs `f` over the default registry and a context with `source` installed as the
/// [`RecordSource`]. Like [`run`], the heap is leaked so the built terms stay valid.
fn run_with_source<R>(
    source: &mut dyn RecordSource,
    f: impl FnOnce(&BifRegistry, &mut BifContext<'_>, &AtomTable) -> R,
) -> R {
    let reg = default_registry();
    let atoms = seeded_atoms();
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(65_536).expect("test heap")));
    let mut ctx =
        BifContext::new(heap, &atoms, ProcessId::new(1, 0)).with_record_source(source);
    f(&reg, &mut ctx, &atoms)
}

/// `records:get_definition(Module, Name)` through the registry.
fn get_definition(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: Eterm,
    name: Eterm,
) -> Result<Eterm> {
    reg.call("records", "get_definition", 2, ctx, &[module, name])
}

/// `records:get_definition/2` (`records/get_definition_unknown_record` in the
/// differential corpus) is the oracle's `error:badarg` when the pair names no
/// definition — and, with no [`RecordSource`] hook installed, `badarg` too.
#[test]
fn get_definition_rejects_non_atom_and_unknown() {
    let m = |ctx: &BifContext<'_>| ctx.atom("m").expect("seeded");
    let r = |ctx: &BifContext<'_>| ctx.atom("r").expect("seeded");

    // Oracle: records:get_definition(m, 2) / (1, r) / (1, 2) => error:badarg.
    run(|reg, ctx, _| {
        assert!(matches!(
            get_definition(reg, ctx, m(ctx), make_small(2)),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            get_definition(reg, ctx, make_small(1), r(ctx)),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            get_definition(reg, ctx, make_small(1), make_small(2)),
            Err(BeamError::BadArg)
        ));
    });

    // No `RecordSource` hook: the seam answers `None` ⇒ `badarg`.
    run(|reg, ctx, _| {
        assert!(matches!(
            get_definition(reg, ctx, m(ctx), r(ctx)),
            Err(BeamError::BadArg)
        ));
    });

    // A hook that knows another pair ⇒ `badarg` for the unknown one.
    let mut source = StubRecords {
        module: "m".to_string(),
        name: "r".to_string(),
        definition: RecordDefinitionView { is_exported: false, fields: Vec::new() },
    };
    run_with_source(&mut source, |reg, ctx, _| {
        let other = ctx.atom("other").expect("seeded");
        assert!(matches!(
            get_definition(reg, ctx, m(ctx), other),
            Err(BeamError::BadArg)
        ));
    });
}

/// `records:get_definition/2` renders the oracle's exact shape.
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
        definition: RecordDefinitionView {
            is_exported: false,
            fields: vec![
                RecordFieldView { name: "a".to_string(), default: None },
                RecordFieldView { name: "b".to_string(), default: Some(make_small(5)) },
                RecordFieldView { name: "e".to_string(), default: None },
            ],
        },
    };
    run_with_source(&mut source, |reg, ctx, _| {
        let m = ctx.atom("m").expect("seeded");
        let r = ctx.atom("r").expect("seeded");
        let result = get_definition(reg, ctx, m, r).expect("the source resolves m:r");

        // `{Map, Fields}`.
        assert_eq!(tuple_arity(result).expect("a tuple"), 2, "the result is a 2-tuple");
        let map = tuple_element(result, 0).expect("element 0");
        let fields = elements(tuple_element(result, 1).expect("element 1"));

        // The map is exactly `#{is_exported => false}`.
        let (keys, values) = flatmap_parts(map).expect("a flat map");
        assert_eq!(keys.len(), 1, "the map has a single entry");
        assert_eq!(name_of(ctx, keys[0]), "is_exported");
        assert_eq!(name_of(ctx, values[0]), "false");

        // The fields are `[a, {b,5}, e]` in declaration order: a default-less field
        // is its bare name atom, a defaulted field is `{Name, Default}`.
        assert_eq!(fields.len(), 3, "fields: {fields:?}");
        assert_eq!(
            fields[0],
            ctx.atom("a").expect("seeded"),
            "a default-less field is the bare name atom"
        );
        let b = fields[1];
        assert_eq!(tuple_arity(b).expect("a tuple"), 2, "a defaulted field is a 2-tuple");
        assert_eq!(tuple_element(b, 0).expect("name"), ctx.atom("b").expect("seeded"));
        assert_eq!(tuple_element(b, 1).expect("default"), make_small(5));
        assert_eq!(
            fields[2],
            ctx.atom("e").expect("seeded"),
            "the fields keep declaration order"
        );
    });
}

/// An exported record renders `#{is_exported => true}`.
///
/// Oracle: `-record #exp{x = 1, y}.` + `-export_record([exp]).` makes
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
    run_with_source(&mut source, |reg, ctx, _| {
        let m = ctx.atom("m").expect("seeded");
        let r = ctx.atom("r").expect("seeded");
        let result = get_definition(reg, ctx, m, r).expect("the source resolves m:r");
        let map = tuple_element(result, 0).expect("element 0");
        let (keys, values) = flatmap_parts(map).expect("a flat map");
        assert_eq!(name_of(ctx, keys[0]), "is_exported");
        assert_eq!(name_of(ctx, values[0]), "true");
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
