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

//! Oracle-pinned integration tests for `records:update/4` driven by a **hash map**
//! (a `>MAP_SMALL_MAP_LIMIT`-key HAMT) update map, through the **public** API only
//! (`default_registry()` + `BifRegistry::call`).
//!
//! `records_update_4` has two arms — a flat map (`erl_record.c:1148-1180`) and a
//! hash map (`:1181-1229`) — and this file pins that the second arm is honoured
//! with exactly the flat arm's field rules. The oracle probes that produced each
//! golden are in the tests' doc comments and are replayed live by
//! [`the_live_oracle_agrees_on_the_hamt_corpus`], which **skips loudly** when the
//! system `erl` is absent (the `tests/os_bif.rs` / `tests/match_spec.rs` pattern).
//!
//! Oracle (OTP 29.1.1 / erts-17.1), a 40-field record and a 40-entry update map:
//!
//! ```text
//! 1> F = [{list_to_atom("f" ++ integer_to_list(I)), I} || I <- lists:seq(1,40)].
//! 2> R = records:create(m, r, F, #{is_exported => true}).
//! 3> U = maps:from_list([{list_to_atom("f" ++ integer_to_list(I)), I*7}
//!                        || I <- lists:seq(1,40)]).
//! 4> maps:size(U).
//!    40                       % ⇒ U is a HAMT
//! 5> R2 = records:update(R, m, r, U).
//! 6> [records:get(list_to_atom("f" ++ integer_to_list(I)), R2) || I <- lists:seq(1,40)].
//!    [7,14,21,...,280]
//! ```
//!
//! and a `>32`-key update map whose keys name no field reports the **lowest atom
//! index** offender:
//!
//! ```text
//! 1> R = records:create(m, r, [{a,1},{b,2}], #{is_exported => true}).
//! 2> Bad = maps:from_list([{list_to_atom("g" ++ integer_to_list(I)), I}
//!                          || I <- lists:seq(1,40)]).   % interns g1 first
//! 3> maps:size(Bad).
//!    40
//! 4> try records:update(R, '_', '_', Bad) catch error:Reason -> Reason end.
//!    {badfield,{{m,r},g1}}     % g1 is the lowest-index bad key
//! ```
//!
//! (The same probe with the keys interned `g40` first reports
//! `{badfield,{{m,r},g40}}` — the C merges the update keys in
//! `erts_cmp_flatmap_keys` order, `record_compare`, `erl_record.c:892-902`.)

use std::process::Command;

use beam_alloc::Heap;
use beam_bifs::prelude::*;
use beam_common::{BeamError, Result};
use beam_process::ProcessId;
use beam_term::{AtomTable, Eterm};

/// The record has this many fields, so the update maps built below always exceed
/// `MAP_SMALL_MAP_LIMIT` (32) and are real HAMT hash maps.
const FIELD_COUNT: usize = 40;

/// The atoms every case needs: the record's module/name and boolean atoms, the
/// structured-reason tags, and the `f*` field / `g*` non-field key families (all
/// seeded, so `BifContext::atom` never misses).
fn seeded_atoms() -> AtomTable {
    let mut names: Vec<String> = [
        "true",
        "false",
        "is_exported",
        "_",
        "m",
        "r",
        "badrecord",
        "badmap",
        "badfield",
    ]
    .iter()
    .map(|name| name.to_string())
    .collect();
    for i in 1..=FIELD_COUNT {
        names.push(format!("f{i}"));
        names.push(format!("g{i}"));
    }
    let mut atoms = AtomTable::new();
    atoms.seed(names.iter().map(String::as_str));
    atoms
}

/// A heap the whole test shares (it never reallocates, so terms built through one
/// context stay valid for later ones).
fn test_heap() -> Heap {
    Heap::new(1 << 20).expect("test heap")
}

/// `beam_term::is_hashmap(x)` — that `x` really is a HAMT rather than a flat map.
fn is_hamt(term: Eterm) -> bool {
    // SAFETY: `term` is a live term this file built on `test_heap`; `is_hashmap`
    // checks `is_boxed` before it reads the header word, and it reads only that
    // word.
    unsafe { beam_term::is_hashmap(term) }
}

/// Calls a BIF through the full public registry.
fn call(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    module: &str,
    name: &str,
    arity: u32,
    args: &[Eterm],
) -> Result<Eterm> {
    reg.call(module, name, arity, ctx, args)
}

/// The `{Key, Value}` pairs of one key family, for `I` in `1..=FIELD_COUNT`.
fn key_value_pairs<F>(ctx: &mut BifContext<'_>, key: F, value: impl Fn(usize) -> i64) -> Result<Vec<(Eterm, Eterm)>>
where
    F: Fn(usize) -> String,
{
    let mut out = Vec::with_capacity(FIELD_COUNT);
    for i in 1..=FIELD_COUNT {
        out.push((ctx.atom(&key(i))?, ctx.int(value(i))?));
    }
    Ok(out)
}

/// `[{K, V}, …]` — a proper list of 2-tuples on the context's heap.
fn pair_list(ctx: &mut BifContext<'_>, pairs: &[(Eterm, Eterm)]) -> Result<Eterm> {
    let mut elements = Vec::with_capacity(pairs.len());
    for &(key, value) in pairs {
        elements.push(ctx.tuple(&[key, value])?);
    }
    ctx.list(&elements)
}

/// `#{is_exported => true}`.
fn create_opts(ctx: &mut BifContext<'_>) -> Result<Eterm> {
    let key = ctx.atom("is_exported")?;
    let value = ctx.atom("true")?;
    beam_codec::term::make_flatmap(ctx.heap(), &[(key, value)])
}

/// Builds the 40-field `records:create(m, r, [{f1,1},…,{f40,40}], #{is_exported => true})`.
fn build_record(reg: &BifRegistry, ctx: &mut BifContext<'_>) -> Result<Eterm> {
    let module = ctx.atom("m")?;
    let name = ctx.atom("r")?;
    let fields = key_value_pairs(ctx, |i| format!("f{i}"), |i| i as i64)?;
    let fields = pair_list(ctx, &fields)?;
    let opts = create_opts(ctx)?;
    call(reg, ctx, "records", "create", 4, &[module, name, fields, opts])
}

/// The value bound to `field` in a record, through the public `records:get/2`.
fn get_field(
    reg: &BifRegistry,
    ctx: &mut BifContext<'_>,
    record: Eterm,
    index: usize,
) -> Result<Eterm> {
    let key = ctx.atom(&format!("f{index}"))?;
    call(reg, ctx, "records", "get", 2, &[key, record])
}

/// The live reason term of a structured raise, asserting `Err(BeamError::Raise(..))`
/// with `reason == ExitReason::Structured` and a term in the opaque live slot.
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

/// The `Key` of the `{{Module,Name}, Key}` payload of a `badfield` reason.
///
/// The crate raises the oracle's nested 2-tuple `{badfield,{{Module,Name},Key}}`, so
/// the key is element 1 of the reason's element 1.
fn badfield_key(reason: Eterm) -> Eterm {
    let pair = beam_codec::term::tuple_element(reason, 1).expect("the {{M,N},K} pair");
    beam_codec::term::tuple_element(pair, 1).expect("the key")
}

/// The `[*_I …]` Erlang **source** for a list of `n` atoms named `{prefix}{I}`, plus
/// the same shape for the `{ {prefix}I, Value }` pair lists the oracle consumes —
/// built as text because the crate's `~w` printer cannot render every term the
/// oracle is fed (and none of these need it).
fn atom_list_src(prefix: &str, n: usize) -> String {
    let names: Vec<String> = (1..=n).map(|i| format!("{prefix}{i}")).collect();
    format!("[{}]", names.join(","))
}

/// `[{Prefix1,V1},{Prefix2,V2},…]` — the `fields`/`updates` list source.
fn pair_list_src(prefix: &str, n: usize, value: impl Fn(usize) -> i64) -> String {
    let pairs: Vec<String> = (1..=n)
        .map(|i| format!("{{{prefix}{i},{}}}", value(i)))
        .collect();
    format!("[{}]", pairs.join(","))
}
/// Runs one `-eval` statement under the system `erl`; `None` (and a loud skip)
/// when the oracle is unavailable.
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(String::from_utf8_lossy(&out.stdout).into_owned()),
        Ok(out) => {
            eprintln!(
                "SKIP: the oracle `erl` failed ({}); stderr: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
            None
        }
        Err(err) => {
            eprintln!("SKIP: no `erl` oracle on PATH ({err})");
            None
        }
    }
}

/// A 40-entry HAMT update map over the `f*` family applies every new value, keeps
/// the record's own module/name/is_exported, and works with both the explicit and
/// the `'_'` module/name.
#[test]
fn update_applies_a_hash_map_update() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let rec = build_record(&reg, &mut ctx).expect("records:create/4");
    let module = ctx.atom("m").expect("seeded");
    let name = ctx.atom("r").expect("seeded");
    let underscore = ctx.atom("_").expect("seeded");

    let updates = key_value_pairs(&mut ctx, |i| format!("f{i}"), |i| (i as i64) * 10).expect("fits");
    let updates_map =
        beam_codec::term::make_map(ctx.heap(), &updates, &atoms).expect("a HAMT update map");
    assert!(
        is_hamt(updates_map),
        "a {FIELD_COUNT}-entry map must be a HAMT, not a flat map"
    );

    for (label, module_arg, name_arg) in [
        ("explicit module/name", module, name),
        ("'_' wildcards", underscore, underscore),
    ] {
        let updated = call(
            &reg,
            &mut ctx,
            "records",
            "update",
            4,
            &[rec, module_arg, name_arg, updates_map],
        )
        .unwrap_or_else(|err| panic!("{label}: records:update/4 failed: {err:?}"));

        for i in 1..=FIELD_COUNT {
            let got = get_field(&reg, &mut ctx, updated, i)
                .unwrap_or_else(|err| panic!("{label}: f{i} get failed: {err:?}"));
            assert_eq!(
                beam_term::signed_value(got),
                (i as i64) * 10,
                "{label}: field f{i} was not updated"
            );
        }

        // The definition (module/name/is_exported) survives the rebuild.
        let got_module = call(&reg, &mut ctx, "records", "get_module", 1, &[updated]).expect("ok");
        let got_name = call(&reg, &mut ctx, "records", "get_name", 1, &[updated]).expect("ok");
        let got_exported =
            call(&reg, &mut ctx, "records", "is_exported", 1, &[updated]).expect("ok");
        assert_eq!(beam_term::atom_val(got_module), beam_term::atom_val(module));
        assert_eq!(beam_term::atom_val(got_name), beam_term::atom_val(name));
        assert_eq!(
            atoms.name(beam_term::atom_val(got_exported)).as_deref(),
            Some("true")
        );
    }
}

/// A 33-entry HAMT update map mentioning **some** of the fields leaves the rest
/// untouched (`records_update_4`'s hash-map arm starts from the old values and
/// overlays the update, `erl_record.c:1181-1229`).
#[test]
fn a_hash_map_update_that_mentions_a_subset_keeps_the_other_fields() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let rec = build_record(&reg, &mut ctx).expect("records:create/4");
    let module = ctx.atom("m").expect("seeded");
    let name = ctx.atom("r").expect("seeded");

    // 33 entries (>32 ⇒ HAMT): f1..f33, the odd-indexed ones carrying a new value.
    let all: Vec<(Eterm, Eterm)> = (1..=33)
        .map(|i| {
            let key = ctx.atom(&format!("f{i}")).expect("seeded");
            let value = if i % 2 == 0 { (i as i64) * 100 } else { i as i64 };
            (key, ctx.int(value).expect("fits"))
        })
        .collect();
    let updates_map = beam_codec::term::make_map(ctx.heap(), &all, &atoms).expect("a HAMT");
    assert!(is_hamt(updates_map), "33 entries must be a HAMT");

    let result = call(
        &reg,
        &mut ctx,
        "records",
        "update",
        4,
        &[rec, module, name, updates_map],
    )
    .expect("records:update/4");
    for i in 1..=FIELD_COUNT {
        let want = if i <= 33 && i % 2 == 0 {
            (i as i64) * 100
        } else {
            i as i64
        };
        let got = get_field(&reg, &mut ctx, result, i).expect("ok");
        assert_eq!(beam_term::signed_value(got), want, "field f{i}");
    }
}

/// An **empty** update map returns the record unchanged even though it is a flat
/// map: the emptiness check precedes both arms (`erl_record.c:1131-1133`).
#[test]
fn an_empty_update_map_returns_the_record_unchanged() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    let rec = build_record(&reg, &mut ctx).expect("records:create/4");
    let module = ctx.atom("m").expect("seeded");
    let name = ctx.atom("r").expect("seeded");
    // The empty map (`#{}`), which `write_map` lays out as a zero-pair flat map.
    let empty = beam_codec::term::make_map(ctx.heap(), &[], &atoms).expect("an empty map");
    assert_eq!(beam_term::map::map_size(empty, &atoms).expect("a map"), 0);

    let result = call(
        &reg,
        &mut ctx,
        "records",
        "update",
        4,
        &[rec, module, name, empty],
    )
    .expect("records:update/4");
    for i in 1..=FIELD_COUNT {
        let got = get_field(&reg, &mut ctx, result, i).expect("ok");
        assert_eq!(beam_term::signed_value(got), i as i64, "field f{i}");
    }
}

/// A 40-key HAMT update map naming no field reports the oracle's structured
/// `{badfield, {{Module,Name}, Key}}` with the **record's own** module/name (even
/// under the `'_'` wildcard), and the reported key is the **lowest atom index**
/// offender — the key the C's sorted merge stops on (`erl_record.c:1226`).
#[test]
fn a_hash_map_update_with_a_bad_key_reports_the_lowest_index_offender() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();
    let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

    // A two-field record: every `g*` key below names no field.
    let module = ctx.atom("m").expect("seeded");
    let name = ctx.atom("r").expect("seeded");
    let underscore = ctx.atom("_").expect("seeded");
    let f1 = ctx.atom("f1").expect("seeded");
    let f2 = ctx.atom("f2").expect("seeded");
    let one = ctx.int(1).expect("fits");
    let two = ctx.int(2).expect("fits");
    let pair_a = ctx.tuple(&[f1, one]).expect("fits");
    let pair_b = ctx.tuple(&[f2, two]).expect("fits");
    let fields = ctx.list(&[pair_a, pair_b]).expect("fits");
    let opts = create_opts(&mut ctx).expect("fits");
    let rec = call(
        &reg,
        &mut ctx,
        "records",
        "create",
        4,
        &[module, name, fields, opts],
    )
    .expect("records:create/4");

    // 40 entries, ALL naming no field. `g1` is seeded before `g2`…, so it has the
    // lowest atom index and is the key the oracle reports.
    let bad = key_value_pairs(&mut ctx, |i| format!("g{i}"), |i| i as i64).expect("fits");
    let bad_map = beam_codec::term::make_map(ctx.heap(), &bad, &atoms).expect("a HAMT");
    assert!(is_hamt(bad_map), "40 entries must be a HAMT");

    let reason = structured_reason(call(
        &reg,
        &mut ctx,
        "records",
        "update",
        4,
        &[rec, underscore, underscore, bad_map],
    ));
    // The reason is the oracle's nested 2-tuple `{badfield,{{m,r},g1}}`: the
    // `{Module,Name}` identifier nests inside a 2-tuple with the key, so the whole
    // reason has arity 2. Both the HAMT path and the flat-map path produce exactly
    // this shape (`tools/differential/src/corpus/records.rs`).
    assert_eq!(beam_codec::term::tuple_arity(reason).expect("a tuple"), 2);
    let tag = beam_codec::term::tuple_element(reason, 0).expect("tag");
    assert_eq!(atoms.name(beam_term::atom_val(tag)).as_deref(), Some("badfield"));
    let pair = beam_codec::term::tuple_element(reason, 1).expect("the {{M,N},K} pair");
    assert_eq!(
        beam_codec::term::tuple_arity(pair).expect("a tuple"),
        2,
        "the payload is the pair of the module/name identifier and the key"
    );
    let identifier = beam_codec::term::tuple_element(pair, 0).expect("the {M,N} identifier");
    assert_eq!(
        beam_term::atom_val(
            beam_codec::term::tuple_element(identifier, 0).expect("module")
        ),
        beam_term::atom_val(module),
        "the reason names the record's own module"
    );
    assert_eq!(
        beam_term::atom_val(beam_codec::term::tuple_element(identifier, 1).expect("name")),
        beam_term::atom_val(name),
        "the reason names the record's own name"
    );
    assert_eq!(
        beam_term::atom_val(badfield_key(reason)),
        beam_term::atom_val(ctx.atom("g1").expect("seeded")),
        "the reported key is the lowest atom index offender"
    );
}

/// The whole HAMT corpus, cross-checked against the system oracle by rendering the
/// updated field values and the `badfield` reason with `~w` on both sides (a
/// printable-syntax pin of the result **terms**). Skips loudly when `erl` is
/// missing.
#[test]
fn the_live_oracle_agrees_on_the_hamt_corpus() {
    let reg = default_registry();
    let mut heap = test_heap();
    let atoms = seeded_atoms();

    // Phase 1: build every term through one `BifContext` (which holds `&mut heap`).
    let values_list;
    let mine_bad_reason;
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let rec = build_record(&reg, &mut ctx).expect("records:create/4");
        let module = ctx.atom("m").expect("seeded");
        let name = ctx.atom("r").expect("seeded");
        let underscore = ctx.atom("_").expect("seeded");

        // 40 entries => a real HAMT, `fI => I*7`.
        let updates =
            key_value_pairs(&mut ctx, |i| format!("f{i}"), |i| (i as i64) * 7).expect("fits");
        let updates_map =
            beam_codec::term::make_map(ctx.heap(), &updates, &atoms).expect("a HAMT");
        assert!(is_hamt(updates_map), "40 entries must be a HAMT");
        let updated = call(
            &reg,
            &mut ctx,
            "records",
            "update",
            4,
            &[rec, module, name, updates_map],
        )
        .expect("records:update/4");

        let mut values = Vec::with_capacity(FIELD_COUNT);
        for i in 1..=FIELD_COUNT {
            values.push(get_field(&reg, &mut ctx, updated, i).expect("ok"));
        }
        values_list = ctx.list(&values).expect("fits");
        // The values are `[7,14,…]`, not the originals.
        for (index, &value) in values.iter().enumerate() {
            assert_eq!(
                beam_term::signed_value(value),
                (index as i64 + 1) * 7,
                "field {} was not updated",
                index + 1
            );
        }

        // The all-bad `g*` HAMT update map, over the 40-field record (no `g*` is a
        // field).
        let bad = key_value_pairs(&mut ctx, |i| format!("g{i}"), |i| i as i64).expect("fits");
        let bad_map = beam_codec::term::make_map(ctx.heap(), &bad, &atoms).expect("a HAMT");
        assert!(is_hamt(bad_map), "40 entries must be a HAMT");
        mine_bad_reason = structured_reason(call(
            &reg,
            &mut ctx,
            "records",
            "update",
            4,
            &[rec, underscore, underscore, bad_map],
        ));
    }

    // My side's updated values, as one integer (printer-independent).
    let mine_hash;
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));
        let hash = call(&reg, &mut ctx, "erlang", "phash2", 1, &[values_list]).expect("phash2");
        mine_hash = beam_term::signed_value(hash);
    }

    // The oracle side, fed Erlang source built as text.
    let fields_src = pair_list_src("f", FIELD_COUNT, |i| i as i64);
    let updates_src = pair_list_src("f", FIELD_COUNT, |i| (i as i64) * 7);
    let field_atoms_src = atom_list_src("f", FIELD_COUNT);
    let values_statement = format!(
        "begin \
           R = records:create(m, r, {fields_src}, #{{is_exported => true}}), \
           U = maps:from_list({updates_src}), \
           {{maps:size(U), \
             erlang:phash2([records:get(A, records:update(R, m, r, U)) || A <- {field_atoms_src}])}} \
         end"
    );
    let Some(stdout) = oracle_eval(&format!("io:format(\"~w~n\",[{values_statement}])")) else {
        eprintln!("SKIP: the oracle is unavailable");
        return;
    };
    let rendered = stdout.trim();
    assert!(
        rendered.starts_with("{40,"),
        "the oracle must build a 40-entry HAMT, got {rendered:?}"
    );
    let oracle_hash: i64 = rendered
        .trim_end_matches('}')
        .split_once(',')
        .map(|(_, hash)| hash.trim())
        .and_then(|hash| hash.parse().ok())
        .unwrap_or_else(|| panic!("unparsable oracle output {rendered:?}"));
    assert_eq!(
        mine_hash, oracle_hash,
        "the 40-entry HAMT update disagrees with the oracle"
    );

    // The reported bad key: the oracle names the record's own `f*`-less `g1`; the
    // crate must report the same key out of the same nested
    // `{badfield,{{m,r},g1}}` reason shape.
    let bad_src = pair_list_src("g", FIELD_COUNT, |i| i as i64);
    let bad_statement = format!(
        "begin \
           R = records:create(m, r, {fields_src}, #{{is_exported => true}}), \
           U = maps:from_list({bad_src}), \
           try records:update(R, '_', '_', U) \
           catch error:{{badfield,{{M,N}},K}} -> io:format(\"<~w,~w,~w>~n\",[M,N,K]) end \
         end"
    );
    let Some(stdout) = oracle_eval(&format!("io:format(\"~s\",[{bad_statement}])")) else {
        eprintln!("SKIP: the oracle is unavailable");
        return;
    };
    assert_eq!(
        stdout.trim(),
        "<m,r,g1>",
        "the oracle must name the record's own module/name and the g1 key"
    );
    let mine_key = badfield_key(mine_bad_reason);
    assert_eq!(
        atoms.name(beam_term::atom_val(mine_key)).as_deref(),
        Some("g1"),
        "the HAMT path must report the same lowest-index offender the oracle does"
    );
}
