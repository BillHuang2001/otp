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
//! `ets:info(Tab, stats)` against the **live** system-`erl` oracle.
//!
//! Every expectation in this file is *asked of the oracle at run time* — nothing is
//! hard-coded — so the file cannot drift from the runtime it is measured against.
//! Both sides build the same table configurations and print one line per case:
//!
//! ```text
//! <tag> <arity> <element-type-signature> <kept_items> <nlocks>   % a hash table
//! <tag> 3 iii                                                    % a catree
//! <tag> false                                                    % no statistics
//! ```
//!
//! so the comparison covers the whole shape contract at once: the arity (8 / 3 /
//! the atom `false`), the **element order and types** (`i`nteger / `f`loat / `a`tom —
//! the three derived chain figures are floats and every count is an integer), and the
//! two elements that agree with the oracle **by value**:
//!
//! * `kept_items` (the seventh element) — the fixed table's retained deleted
//!   objects, `0` unfixed;
//! * `nlocks` (the eighth) — the effective `{write_concurrency, …}` mode's lock
//!   count (`1` / `64` / the normalized fixed count, and `1` for a `private` table).
//!
//! The chain/occupancy metrics (elements 1–6) are **not** compared: they measure this
//! crate's own interiors (a CHAMP index / a chained bucket array / a K-ary B+-tree)
//! against ERTS's bucket-chained hash table and catree, so their values legitimately
//! differ. Their invariants — `sum(chain_len)` counts every object once,
//! `min <= avg <= max`, the binomial `std_dev_expected`, and the C's `float`
//! precision model (which *is* reproduced exactly) — are pinned in
//! `beam_ets/tests/ets_stats.rs` and `beam_ets/src/stats.rs`.
//!
//! Every atom of the probe is interned before the run, so no name is resolved from a
//! cold table; the table handles are ordinary unnamed tables (`ets:new(t, Opts)` is
//! not `named_table`, so the repeated name is not a conflict).
use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_process::ProcessId;
use beam_term::{atom_val, signed_value, AtomTable, Eterm};
use std::path::{Path, PathBuf};
use std::process::Command;

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// Every atom the table configurations, the destructuring BIFs and the rendering
/// below resolve by name.
///
/// The first name is a harmless sentinel: the `ets:new/2` option parser classifies an
/// option *value* with `atoms.name(atom_val(value))`, and `atom_val` maps a small
/// integer's raw word down to index `0`, so index `0` must not be a legal option
/// value (see `tests/ets_options.rs` for the full explanation).
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        // Sentinel (atom index 0, never a legal option value).
        "sentinel",
        // Table kinds and protection modes.
        "set",
        "ordered_set",
        "bag",
        "duplicate_bag",
        "public",
        "private",
        // Option names and values.
        "write_concurrency",
        "debug_hash_fixed_number_of_locks",
        "auto",
        "true",
        "false",
        // The `ets:info/2` item.
        "stats",
        // The table name (never `named_table`).
        "t",
    ]);
    atoms
}

/// The `{Key, Value}` object `key` → `0` on the current heap.
fn object(ctx: &mut BifContext<'_>, key: i64) -> Eterm {
    let k = ctx.int(key).expect("fits");
    let v = ctx.int(0).expect("fits");
    ctx.tuple(&[k, v]).expect("fits")
}

/// The bare option atom `name`.
fn bare(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("seeded")
}

/// The 2-tuple option `{key, value}`.
fn pair(ctx: &mut BifContext<'_>, key: &str, value: Eterm) -> Eterm {
    let key = bare(ctx, key);
    ctx.tuple(&[key, value]).expect("fits")
}

/// The hidden `{write_concurrency, {debug_hash_fixed_number_of_locks, N}}` option.
fn fixed_locks(ctx: &mut BifContext<'_>, locks: i64) -> Eterm {
    let tag = bare(ctx, "debug_hash_fixed_number_of_locks");
    let locks = ctx.int(locks).expect("fits");
    let inner = ctx.tuple(&[tag, locks]).expect("fits");
    pair(ctx, "write_concurrency", inner)
}

// ---------------------------------------------------------------------------
// One case: the configuration, applied to both sides
// ---------------------------------------------------------------------------

/// How a case populates its table after `ets:new/2`.
#[derive(Clone, Copy)]
enum Populate {
    /// Nothing inserted.
    Empty,
    /// `{1,0} … {n,0}` — `n` distinct keys.
    Distinct(i64),
    /// The single object `{1,0}` inserted `n` times.
    DuplicateKey(i64),
    /// `n` distinct keys, then `safe_fixtable(T, true)` and `ets:delete(T, 1)` — the
    /// one configuration whose `kept_items` is non-zero.
    FixedWithOneDeleted(i64),
}

/// One `ets:info(Tab, stats)` configuration, run identically on both sides.
struct Case {
    /// The tag both transcripts print.
    tag: &'static str,
    /// The `ets:new/2` option atoms (built by [`options`]).
    options: fn(&mut BifContext<'_>) -> Vec<Eterm>,
    /// How the table is populated.
    populate: Populate,
}

/// The full configuration matrix, in the order both transcripts print it.
///
/// The `nlocks` figures and the `false`/3-tuple answers were measured on the system
/// `erl` first (OTP 29.1.1 / erts-17.1); the live probe re-measures them on every run.
const CASES: &[Case] = &[
    Case {
        tag: "set_empty",
        options: |ctx| vec![bare(ctx, "set")],
        populate: Populate::Empty,
    },
    Case {
        tag: "set_5",
        options: |ctx| vec![bare(ctx, "set")],
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "bag_5",
        options: |ctx| vec![bare(ctx, "bag")],
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "dupbag_5",
        options: |ctx| vec![bare(ctx, "duplicate_bag")],
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "dupbag_one_key_5",
        options: |ctx| vec![bare(ctx, "duplicate_bag")],
        populate: Populate::DuplicateKey(5),
    },
    Case {
        tag: "set_2",
        options: |ctx| vec![bare(ctx, "set")],
        populate: Populate::Distinct(2),
    },
    Case {
        tag: "set_10000",
        options: |ctx| vec![bare(ctx, "set")],
        populate: Populate::Distinct(10_000),
    },
    Case {
        tag: "wc_true",
        options: |ctx| {
            let t = bare(ctx, "true");
            vec![bare(ctx, "set"), pair(ctx, "write_concurrency", t)]
        },
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "wc_auto",
        options: |ctx| {
            let a = bare(ctx, "auto");
            vec![bare(ctx, "set"), pair(ctx, "write_concurrency", a)]
        },
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "wc_fixed_4",
        options: |ctx| vec![bare(ctx, "set"), fixed_locks(ctx, 4)],
        populate: Populate::Empty,
    },
    Case {
        tag: "wc_fixed_100",
        options: |ctx| vec![bare(ctx, "set"), fixed_locks(ctx, 100)],
        populate: Populate::Empty,
    },
    Case {
        tag: "wc_fixed_1024",
        options: |ctx| vec![bare(ctx, "set"), fixed_locks(ctx, 1024)],
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "wc_fixed_32768",
        options: |ctx| vec![bare(ctx, "set"), fixed_locks(ctx, 32768)],
        populate: Populate::Empty,
    },
    Case {
        tag: "wc_private",
        options: |ctx| {
            let t = bare(ctx, "true");
            vec![
                bare(ctx, "set"),
                bare(ctx, "private"),
                pair(ctx, "write_concurrency", t),
            ]
        },
        populate: Populate::Empty,
    },
    Case {
        tag: "fixed_one_of_five_deleted",
        options: |ctx| vec![bare(ctx, "set")],
        populate: Populate::FixedWithOneDeleted(5),
    },
    Case {
        tag: "oset_plain_empty",
        options: |ctx| vec![bare(ctx, "ordered_set")],
        populate: Populate::Empty,
    },
    Case {
        tag: "oset_plain_5",
        options: |ctx| vec![bare(ctx, "ordered_set")],
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "oset_plain_1000",
        options: |ctx| vec![bare(ctx, "ordered_set")],
        populate: Populate::Distinct(1_000),
    },
    Case {
        tag: "oset_wc_5",
        options: |ctx| {
            let t = bare(ctx, "true");
            vec![bare(ctx, "ordered_set"), pair(ctx, "write_concurrency", t)]
        },
        populate: Populate::Distinct(5),
    },
    Case {
        tag: "oset_wc_1000",
        options: |ctx| {
            let t = bare(ctx, "true");
            vec![bare(ctx, "ordered_set"), pair(ctx, "write_concurrency", t)]
        },
        populate: Populate::Distinct(1_000),
    },
    Case {
        tag: "oset_wc_auto",
        options: |ctx| {
            let a = bare(ctx, "auto");
            vec![bare(ctx, "ordered_set"), pair(ctx, "write_concurrency", a)]
        },
        populate: Populate::Empty,
    },
    Case {
        tag: "oset_wc_private",
        options: |ctx| {
            let t = bare(ctx, "true");
            vec![
                bare(ctx, "ordered_set"),
                bare(ctx, "private"),
                pair(ctx, "write_concurrency", t),
            ]
        },
        populate: Populate::Empty,
    },
];

// ---------------------------------------------------------------------------
// The oracle side
// ---------------------------------------------------------------------------

/// The oracle transcript: one line per [`CASES`] entry, in the same order.
///
/// The probe is a single `-eval` expression that mirrors [`Populate`] exactly. It
/// prints `<tag> <arity> <signature> [<kept_items> <nlocks>]` (see the module docs).
const ORACLE_EVAL: &str = concat!(
    "Ty = fun(E) -> if is_integer(E) -> $i; is_float(E) -> $f; true -> $a end end, ",
    "Emit = fun(Tag, V) -> ",
    "case V of ",
    "false -> io:format(\"~s false~n\", [Tag]); ",
    "Sz -> Es = tuple_to_list(Sz), ",
    "Tail = case length(Es) of ",
    "8 -> io_lib:format(\" ~w ~w\", [element(7,Sz), element(8,Sz)]); ",
    "_ -> \"\" end, ",
    "io:format(\"~s ~w ~s~s~n\", [Tag, length(Es), [Ty(E) || E <- Es], Tail]) ",
    "end end, ",
    "Run = fun(Tag, Opts, Pop) -> ",
    "T = ets:new(t, Opts), ",
    "case Pop of ",
    "empty -> ok; ",
    "{distinct, N} -> [ets:insert(T, {K, 0}) || K <- lists:seq(1, N)]; ",
    "{dup, N} -> [ets:insert(T, {1, 0}) || _ <- lists:seq(1, N)]; ",
    "{fixed_del, N} -> [ets:insert(T, {K, 0}) || K <- lists:seq(1, N)], ",
    "ets:safe_fixtable(T, true), ets:delete(T, 1) ",
    "end, ",
    "Emit(Tag, ets:info(T, stats)), ",
    "ets:delete(T) end, ",
    "Run(\"set_empty\", [set], empty), ",
    "Run(\"set_5\", [set], {distinct, 5}), ",
    "Run(\"bag_5\", [bag], {distinct, 5}), ",
    "Run(\"dupbag_5\", [duplicate_bag], {distinct, 5}), ",
    "Run(\"dupbag_one_key_5\", [duplicate_bag], {dup, 5}), ",
    "Run(\"set_2\", [set], {distinct, 2}), ",
    "Run(\"set_10000\", [set], {distinct, 10000}), ",
    "Run(\"wc_true\", [set, {write_concurrency, true}], {distinct, 5}), ",
    "Run(\"wc_auto\", [set, {write_concurrency, auto}], {distinct, 5}), ",
    "Run(\"wc_fixed_4\", [set, {write_concurrency, ",
    "{debug_hash_fixed_number_of_locks, 4}}], empty), ",
    "Run(\"wc_fixed_100\", [set, {write_concurrency, ",
    "{debug_hash_fixed_number_of_locks, 100}}], empty), ",
    "Run(\"wc_fixed_1024\", [set, {write_concurrency, ",
    "{debug_hash_fixed_number_of_locks, 1024}}], {distinct, 5}), ",
    "Run(\"wc_fixed_32768\", [set, {write_concurrency, ",
    "{debug_hash_fixed_number_of_locks, 32768}}], empty), ",
    "Run(\"wc_private\", [set, private, {write_concurrency, true}], empty), ",
    "Run(\"fixed_one_of_five_deleted\", [set], {fixed_del, 5}), ",
    "Run(\"oset_plain_empty\", [ordered_set], empty), ",
    "Run(\"oset_plain_5\", [ordered_set], {distinct, 5}), ",
    "Run(\"oset_plain_1000\", [ordered_set], {distinct, 1000}), ",
    "Run(\"oset_wc_5\", [ordered_set, {write_concurrency, true}], {distinct, 5}), ",
    "Run(\"oset_wc_1000\", [ordered_set, {write_concurrency, true}], {distinct, 1000}), ",
    "Run(\"oset_wc_auto\", [ordered_set, {write_concurrency, auto}], empty), ",
    "Run(\"oset_wc_private\", [ordered_set, private, {write_concurrency, true}], empty), ",
    "ok",
);

/// `$ERTS_ORACLE` when it names an existing executable, else `erl` on `$PATH` — the
/// same resolution `tools/differential` and the sibling integration files use.
fn resolve_oracle() -> Option<PathBuf> {
    if let Some(value) = std::env::var_os("ERTS_ORACLE") {
        let program = PathBuf::from(&value);
        return is_executable(&program).then_some(program);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join("erl"))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && path
            .metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The oracle's transcript, one line per case.
///
/// # Panics
///
/// When no oracle is available or it fails — a missing oracle is **not** a pass
/// (the crate's fail-loud convention for live-oracle integration files).
fn oracle_lines() -> Vec<String> {
    let Some(oracle) = resolve_oracle() else {
        panic!(
            "no system `erl` oracle found ($ERTS_ORACLE unset or not executable, and no \
             `erl` on $PATH): this file's live cross-check cannot run. Install Erlang/OTP \
             or set $ERTS_ORACLE."
        );
    };
    let output = Command::new(&oracle)
        .args(["-noshell", "-eval", ORACLE_EVAL, "-s", "init", "stop"])
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn the oracle '{}': {e}", oracle.display()));
    assert!(
        output.status.success(),
        "the oracle '{}' exited with {:?}\n--- stderr ---\n{}\n--- eval ---\n{ORACLE_EVAL}",
        oracle.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stderr),
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

// ---------------------------------------------------------------------------
// Our side
// ---------------------------------------------------------------------------

/// The fixture: a heap, the seeded atom table and a table registry.
fn parts() -> (Heap, AtomTable, TableRegistry) {
    (
        Heap::new(1 << 22).expect("test heap"),
        seeded_atoms(),
        TableRegistry::new(),
    )
}

/// `ets:new/2` for the case, then the case's population.
fn build_case(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    case: &Case,
) -> Eterm {
    let name = bare(ctx, "t");
    let options = (case.options)(ctx);
    let options = ctx.list(&options).expect("fits");
    let handle = bifs
        .call("ets", "new", 2, ctx, &[name, options])
        .expect("ets:new");

    let mut insert = |ctx: &mut BifContext<'_>, key: i64| {
        let obj = object(ctx, key);
        let out = bifs
            .call("ets", "insert", 2, ctx, &[handle, obj])
            .expect("ets:insert");
        let is_true = out == ctx.atom("true").expect("seeded");
        assert!(is_true, "ets:insert answered a non-true atom");
    };

    match case.populate {
        Populate::Empty => {}
        Populate::Distinct(n) => {
            for key in 1..=n {
                insert(ctx, key);
            }
        }
        Populate::DuplicateKey(n) => {
            for _ in 0..n {
                insert(ctx, 1);
            }
        }
        Populate::FixedWithOneDeleted(n) => {
            for key in 1..=n {
                insert(ctx, key);
            }
            let t = bare(ctx, "true");
            bifs.call("ets", "safe_fixtable", 2, ctx, &[handle, t])
                .expect("ets:safe_fixtable");
            let key = ctx.int(1).expect("fits");
            bifs.call("ets", "delete", 2, ctx, &[handle, key])
                .expect("ets:delete");
        }
    }
    handle
}

/// The `i`nteger/`f`loat/`a`tom signature of `term`, resolved through the
/// registered BIFs (so no term is inspected by hand).
fn type_signature(bifs: &BifRegistry, ctx: &mut BifContext<'_>, term: Eterm) -> char {
    for (name, tag) in [
        ("is_integer", 'i'),
        ("is_float", 'f'),
        ("is_atom", 'a'),
    ] {
        let out = bifs
            .call("erlang", name, 1, ctx, &[term])
            .expect("type test BIF");
        if ctx.atoms().name(atom_val(out)) == Some("true") {
            return tag;
        }
    }
    '?'
}

/// The transcript line for one case, in exactly the oracle's format.
fn case_line(bifs: &BifRegistry, ctx: &mut BifContext<'_>, handle: Eterm, tag: &str) -> String {
    let item = bare(ctx, "stats");
    let stats = bifs
        .call("ets", "info", 2, ctx, &[handle, item])
        .expect("ets:info/2 stats");
    if ctx.atoms().name(atom_val(stats)) == Some("false") {
        return format!("{tag} false");
    }
    let arity = bifs
        .call("erlang", "tuple_size", 1, ctx, &[stats])
        .expect("erlang:tuple_size");
    let arity = signed_value(arity);
    let mut signature = String::new();
    let mut elements = Vec::new();
    for index in 1..=arity {
        let pos = ctx.int(index).expect("fits");
        let element = bifs
            .call("erlang", "element", 2, ctx, &[pos, stats])
            .expect("erlang:element");
        signature.push(type_signature(bifs, ctx, element));
        elements.push(element);
    }
    if arity == 8 {
        let kept_items = signed_value(elements[6]);
        let nlocks = signed_value(elements[7]);
        format!("{tag} {arity} {signature} {kept_items} {nlocks}")
    } else {
        format!("{tag} {arity} {signature}")
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

/// Every configuration's `ets:info(Tab, stats)` agrees with the live oracle on the
/// arity, the element order and types, `kept_items` and `nlocks`.
#[test]
fn info_stats_agrees_with_the_live_oracle() {
    let oracle = oracle_lines();
    assert_eq!(
        oracle.len(),
        CASES.len(),
        "the probe printed {} lines for {} cases: {oracle:#?}",
        oracle.len(),
        CASES.len()
    );

    let bifs = default_registry();
    let (mut heap, atoms, mut tables) = parts();
    let mut ctx =
        BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0)).with_tables(&mut tables);

    for (case, expected) in CASES.iter().zip(&oracle) {
        let handle = build_case(&bifs, &mut ctx, case);
        let actual = case_line(&bifs, &mut ctx, handle, case.tag);
        assert_eq!(
            &actual, expected,
            "{}: our stats line differs from the oracle\n  ours:   {actual}\n  oracle: {expected}",
            case.tag
        );
    }
}

/// The three answers the oracle gives, pinned by tag so a shape regression is named
/// rather than merely "some line differs": eight elements for a hash table, three for
/// a fine-locked `ordered_set` and the atom `false` for a plain one.
///
/// These are read off the *oracle's* transcript (not hard-coded), so they also prove
/// the probe itself exercised all three arms.
#[test]
fn the_oracle_transcript_covers_all_three_answers() {
    let oracle = oracle_lines();
    let line_for = |tag: &str| -> String {
        oracle
            .iter()
            .find(|line| line.split_whitespace().next() == Some(tag))
            .unwrap_or_else(|| panic!("no oracle line for {tag:?}: {oracle:#?}"))
            .clone()
    };
    for tag in ["set_5", "bag_5", "dupbag_5", "set_10000", "wc_true"] {
        let line = line_for(tag);
        let arity = line.split_whitespace().nth(1);
        assert_eq!(arity, Some("8"), "{tag}: a hash table is an 8-tuple: {line:?}");
    }
    for tag in ["oset_wc_5", "oset_wc_1000", "oset_wc_auto"] {
        let line = line_for(tag);
        let arity = line.split_whitespace().nth(1);
        assert_eq!(arity, Some("3"), "{tag}: a catree is a 3-tuple: {line:?}");
    }
    for tag in [
        "oset_plain_empty",
        "oset_plain_5",
        "oset_plain_1000",
        "oset_wc_private",
    ] {
        let line = line_for(tag);
        assert_eq!(line, format!("{tag} false"), "{tag}: no statistics");
    }
    // `kept_items` is non-zero only for the fixed table with a deleted object, and
    // `nlocks` is `1` / `64` / the normalized fixed count / `1` for `private`.
    assert_eq!(
        line_for("fixed_one_of_five_deleted")
            .split_whitespace()
            .nth(6),
        Some("1"),
        "kept_items of the fixed, one-deleted table"
    );
    for (tag, nlocks) in [
        ("set_5", "1"),
        ("wc_true", "64"),
        ("wc_auto", "64"),
        ("wc_fixed_4", "64"),
        ("wc_fixed_100", "64"),
        ("wc_fixed_1024", "1024"),
        ("wc_fixed_32768", "32768"),
        ("wc_private", "1"),
    ] {
        assert_eq!(
            line_for(tag).split_whitespace().nth(7),
            Some(nlocks),
            "{tag}: nlocks"
        );
    }
}
