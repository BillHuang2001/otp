//! The `ets:new/2` **creation options**: the creation-time heir
//! (`{heir, Pid}` / `{heir, Pid, Data}` / `{heir, none}`), the explicit
//! `{decentralized_counters, Bool}` option and the accessors a host option parser
//! needs — with live-oracle cross-checks of the values the oracle reports.
//!
//! The oracle is the system `erl` (`$ERTS_ORACLE` or `erl` on `$PATH`, OTP 29 /
//! erts-17.1). Every check that shells out is a **loud skip** when no Erlang is
//! installed (`BEAM_ETS_REQUIRE_ORACLE=1` turns the skip into a failure).

mod common;

use std::time::Duration;

use beam_ets::{Access, Heir, OwnerId, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;
use common::*;

/// Wall-clock bound for the oracle child, so a wedged oracle fails instead of
/// hanging the suite.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// One `ets:new/2` creation case, described in a form both sides can build: the
/// crate builds it as [`TableOptions`] and the oracle builds the equivalent
/// option list textually.
#[derive(Debug, Clone, Copy)]
struct Case {
    kind: TableKind,
    access: Access,
    write_concurrency: bool,
    decentralized_counters: Option<bool>,
}

impl Case {
    /// The oracle's argument encoding: `<kind>:<access>:<wc>:<dc>` with `wc` ∈
    /// `{0,1}` and `dc` ∈ `{-1 (omitted), 0 (false), 1 (true)}`.
    fn oracle_arg(&self) -> String {
        let dc = match self.decentralized_counters {
            None => -1,
            Some(false) => 0,
            Some(true) => 1,
        };
        format!(
            "{}:{}:{}:{}",
            self.kind.as_atom_name(),
            self.access.as_atom_name(),
            u8::from(self.write_concurrency),
            dc
        )
    }

    /// The same case as parsed options.
    fn options(&self) -> TableOptions {
        let mut opts = TableOptions::new();
        opts.set_kind(self.kind)
            .set_access(self.access)
            .set_write_concurrency(self.write_concurrency)
            .set_decentralized_counters(self.decentralized_counters);
        opts
    }
}

/// The option combinations cross-checked against the oracle: every kind with the
/// option omitted, with `{write_concurrency, true}`, with an explicit
/// `{decentralized_counters, Bool}` (alone and combined), and the `private`
/// normalization.
fn counter_cases() -> Vec<Case> {
    let kinds = [
        TableKind::Set,
        TableKind::OrderedSet,
        TableKind::Bag,
        TableKind::DuplicateBag,
    ];
    let mut cases = Vec::new();
    for kind in kinds {
        for decentralized_counters in [None, Some(false), Some(true)] {
            for write_concurrency in [false, true] {
                cases.push(Case {
                    kind,
                    access: Access::Protected,
                    write_concurrency,
                    decentralized_counters,
                });
            }
        }
    }
    // The `private` normalization: a private table never gets the fine-locked
    // flag, so `decentralized_counters` is `false` however it was requested.
    for kind in [TableKind::Set, TableKind::OrderedSet] {
        for decentralized_counters in [None, Some(true)] {
            cases.push(Case {
                kind,
                access: Access::Private,
                write_concurrency: true,
                decentralized_counters,
            });
        }
    }
    // `read_concurrency` must not affect it.
    cases.push(Case {
        kind: TableKind::OrderedSet,
        access: Access::Public,
        write_concurrency: true,
        decentralized_counters: None,
    });
    cases
}

/// The **`decentralized_counters` rule**, end to end: for each case the oracle's
/// `ets:info(T, decentralized_counters)` is compared with the crate's
/// [`TableOptions::decentralized_counters`] *and* with what `Table::info` /
/// `TableInfo::entries` report for a table built from those options.
///
/// Measured rule (C `db_new` in `../../erts/emulator/beam/erl_db.c`): the
/// observable value is `DB_FINE_LOCKED && is_decentralized_counters`; the
/// explicit `{decentralized_counters, Bool}` option overrides the kind's default
/// (which is `true` only for `ordered_set`), and `DB_FINE_LOCKED` needs an
/// effective `write_concurrency`.
#[test]
fn decentralized_counters_agrees_with_the_live_oracle() {
    let cases = counter_cases();
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "decentralized_counters_agrees_with_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let args: Vec<String> = cases.iter().map(Case::oracle_arg).collect();
    let output = oracle::run_escript(
        &escript,
        COUNTERS_ORACLE_PROGRAM,
        &args,
        "options_counters",
        ORACLE_TIMEOUT,
    );
    let oracle_values = parse_indexed_bools(&output.stdout);

    assert_eq!(
        oracle_values.len(),
        cases.len(),
        "the oracle answered {} of {} cases:\n{}",
        oracle_values.len(),
        cases.len(),
        String::from_utf8_lossy(&output.stdout)
    );

    let mut atoms = AtomTable::new();
    for (index, case) in cases.iter().enumerate() {
        let expected = oracle_values[index];
        let opts = case.options();
        assert_eq!(
            opts.decentralized_counters(),
            expected,
            "TableOptions::decentralized_counters for {case:?}"
        );

        let table = Table::new(TableId::next(), opts).expect("a fresh table");
        assert_eq!(
            table.info().decentralized_counters,
            expected,
            "Table::info for {case:?}"
        );
        assert_eq!(
            rendered_bool(&mut atoms, &table, "decentralized_counters"),
            expected,
            "TableInfo::entries for {case:?}"
        );
    }
    println!(
        "[options] decentralized_counters: {} cases agree with the live oracle",
        cases.len()
    );
}

/// A **creation-time** heir is adopted by the table: `ets:new/2` with
/// `{heir, Pid}` / `{heir, Pid, Data}` installs the heir and `{heir, none}` (or
/// no option) leaves it unset.
///
/// The oracle cross-check is the *fact* that an heir is installed
/// (`ets:info(T, heir) =/= none`); the heir *pid* itself is not renderable by
/// this stage (the documented pid gap — the crate renders `undefined`).
#[test]
fn creation_time_heir_is_adopted() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "creation_time_heir_is_adopted",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let args: Vec<String> = ["none", "arity2", "arity3", "absent"]
        .iter()
        .map(|spec| spec.to_string())
        .collect();
    let output = oracle::run_escript(
        &escript,
        HEIR_ORACLE_PROGRAM,
        &args,
        "options_heir",
        ORACLE_TIMEOUT,
    );
    let oracle_values = parse_indexed_bools(&output.stdout);
    assert_eq!(
        oracle_values,
        vec![false, true, true, false],
        "the oracle's heir-installed facts:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    let mut atoms = AtomTable::new();
    // `{heir, Pid}` (arity 2) carries **no** `Data` term at all: `data: None`.
    let arity_two = Heir {
        owner: OwnerId::new(9),
        data: None,
    };

    // (a) `{heir, Pid}`.
    let mut opts = TableOptions::new();
    opts.set_heir(Some(arity_two));
    assert_eq!(opts.heir(), Some(arity_two));
    let arity2 = Table::new(TableId::next(), opts).expect("a fresh table");
    assert_eq!(arity2.info().heir, Some(arity_two.owner), "the heir is adopted");
    assert_eq!(arity2.heir(), Some(arity_two));
    assert_eq!(
        arity2.heir_data(),
        None,
        "an arity-2 heir has no gift data at all, so no transfer message is sent"
    );
    assert_eq!(
        arity2.info().heir,
        Some(OwnerId::new(9)),
        "the gift data is not part of the introspected heir"
    );
    // The heir *pid* is a documented gap: the rendered field is `undefined`.
    assert_eq!(
        info_entry(&arity2.info().entries(&mut atoms), &atoms, "heir"),
        Some(make_atom(atoms.intern("undefined"))),
        "a set heir renders as `undefined` (no pid term)"
    );

    // (b) `{heir, Pid, Data}` — the gift data is carried on the table's heir (here
    // an immediate, which `Table::build` stores by value). The copy-into-table-
    // storage behaviour is pinned in `ets_ownership.rs`.
    let mut opts = TableOptions::new();
    let gift = Heir {
        owner: OwnerId::new(4),
        data: Some(make_small(7)),
    };
    opts.set_heir(Some(gift));
    let arity3 = Table::new(TableId::next(), opts).expect("a fresh table");
    assert_eq!(arity3.info().heir, Some(OwnerId::new(4)));
    assert_eq!(arity3.heir().and_then(|h| h.data), Some(make_small(7)));

    // (b') `{heir, Pid, undefined}` is **arity 3**: it carries the atom `undefined`
    // as its gift data, so on owner death the oracle *does* deliver
    // `{'ETS-TRANSFER', …, undefined}` — unlike the arity-2 form above, which
    // delivers nothing. The two are distinguishable in the model.
    let mut opts = TableOptions::new();
    let undefined_gift = Heir {
        owner: OwnerId::new(4),
        data: Some(interned_atom(&mut atoms, "undefined")),
    };
    opts.set_heir(Some(undefined_gift));
    let arity3_undefined = Table::new(TableId::next(), opts).expect("a fresh table");
    assert_eq!(arity3_undefined.heir_data(), undefined_gift.data);
    assert_eq!(arity3_undefined.heir(), Some(undefined_gift));
    assert_ne!(
        arity3_undefined.heir(),
        arity2.heir(),
        "the arity-2 and arity-3 (undefined) heirs differ"
    );

    // (c) `{heir, none}` and an omitted option both leave the heir unset (the
    // oracle's rendered `none`).
    let mut opts = TableOptions::new();
    opts.set_heir(None);
    let cleared = Table::new(TableId::next(), opts).expect("a fresh table");
    assert_eq!(cleared.heir(), None);
    assert_eq!(cleared.info().heir, None);
    assert_eq!(
        info_entry(&cleared.info().entries(&mut atoms), &atoms, "heir"),
        Some(make_atom(atoms.intern("none"))),
        "an unset heir renders as `none`"
    );

    let absent = Table::new(TableId::next(), TableOptions::new()).expect("a fresh table");
    assert_eq!(absent.heir(), None);
    assert_eq!(absent.info().heir, None);

    println!("[options] creation-time heir: the oracle's 4 facts agree with the crate");
}

/// Every `ets:new/2` option the crate models is reachable from the parsed option
/// set (what a host option parser builds) and is reflected by the table it
/// builds — the contract [`beam_bifs`]'s `parse_options` depends on.
#[test]
fn option_accessors_cover_the_ets_new_surface() {
    let mut atoms = AtomTable::new();
    let name = atoms.intern("accessor_table");

    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet)
        .set_keypos(2)
        .unwrap()
        .set_access(Access::Public)
        .set_named(true)
        .set_name(name)
        .set_compressed(true)
        .set_write_concurrency(true)
        .set_read_concurrency(true)
        .set_heir(Some(Heir {
            owner: OwnerId::new(3),
            data: Some(make_small(5)),
        }))
        .set_decentralized_counters(Some(true));

    // The parsed option set.
    assert_eq!(opts.kind(), TableKind::OrderedSet);
    assert_eq!(opts.keypos(), 2);
    assert_eq!(opts.access(), Access::Public);
    assert!(opts.is_named());
    assert_eq!(opts.name(), Some(name));
    assert!(opts.is_compressed());
    assert!(opts.write_concurrency());
    assert!(opts.read_concurrency());
    assert_eq!(
        opts.concurrency(),
        beam_ets::Concurrency::ReadWrite,
        "both concurrency flags are in effect"
    );
    assert_eq!(opts.heir().map(|h| h.owner), Some(OwnerId::new(3)));
    assert_eq!(opts.decentralized_counters_option(), Some(true));
    assert!(opts.decentralized_counters());

    // The table the parser builds from it agrees field for field.
    let table = Table::new(TableId::next(), opts).expect("a fresh table");
    assert_eq!(table.kind(), TableKind::OrderedSet);
    assert_eq!(table.keypos(), 2);
    assert_eq!(table.access(), Access::Public);
    assert_eq!(table.name(), Some(name));
    assert_eq!(table.heir().map(|h| h.owner), Some(OwnerId::new(3)));

    let info = table.info();
    assert_eq!(info.kind, TableKind::OrderedSet);
    assert_eq!(info.keypos, 2);
    assert_eq!(info.access, Access::Public);
    assert_eq!(info.name, Some(name));
    assert!(info.named_table);
    assert!(info.compressed);
    assert!(info.write_concurrency);
    assert!(info.read_concurrency);
    assert!(info.decentralized_counters);
    assert_eq!(info.heir, Some(OwnerId::new(3)));
}

/// The defaults a host parser starts from: `TableOptions::new` is exactly
/// `ets:new(T, [])`.
#[test]
fn defaults_are_ets_new_defaults() {
    let opts = TableOptions::new();
    assert_eq!(opts.kind(), TableKind::Set);
    assert_eq!(opts.keypos(), 1);
    assert_eq!(opts.access(), Access::Protected);
    assert!(!opts.is_named());
    assert_eq!(opts.name(), None);
    assert!(!opts.is_compressed());
    assert!(!opts.write_concurrency());
    assert!(!opts.read_concurrency());
    assert_eq!(opts.heir(), None);
    assert_eq!(opts.decentralized_counters_option(), None);
    assert!(!opts.decentralized_counters());

    let table = Table::new(TableId::next(), opts).expect("a fresh table");
    let info = table.info();
    assert_eq!(info.heir, None);
    assert!(!info.decentralized_counters);
}

/// The **explicit option wins**: `{decentralized_counters, false}` on an
/// `ordered_set` created with `{write_concurrency, true}` reports `false`
/// (measured oracle behaviour), and `{decentralized_counters, true}` does the
/// same for a `set` that asked for write concurrency.
///
/// Pinned separately from the matrix so the override itself is unmistakable.
#[test]
fn the_explicit_option_overrides_the_kind_default() {
    let ordered = {
        let mut opts = TableOptions::new();
        opts.set_kind(TableKind::OrderedSet)
            .set_write_concurrency(true)
            .set_decentralized_counters(Some(false));
        Table::new(TableId::next(), opts).expect("a fresh table")
    };
    assert!(!ordered.info().decentralized_counters);

    let plain = {
        let mut opts = TableOptions::new();
        opts.set_write_concurrency(true);
        assert!(
            !opts.decentralized_counters(),
            "a `set` with write concurrency alone reports false"
        );
        opts.set_decentralized_counters(Some(true));
        Table::new(TableId::next(), opts).expect("a fresh table")
    };
    assert!(plain.info().decentralized_counters);
}

/// The boolean an `ets:info/1` field renders (`TableInfo::entries`), read off the
/// proplist.
fn rendered_bool(atoms: &mut AtomTable, table: &Table, field: &str) -> bool {
    let entries = table.info().entries(atoms);
    let value = info_entry(&entries, atoms, field)
        .unwrap_or_else(|| panic!("`{field}` is an `/1` field and must be reported"));
    if value == make_atom(atoms.intern("true")) {
        true
    } else if value == make_atom(atoms.intern("false")) {
        false
    } else {
        panic!("`{field}` is not a boolean: {value:?}");
    }
}

/// The `true`/`false` values of the oracle's `"<index> -> <bool>"` lines, in
/// index order (`index` is 1-based, as printed).
fn parse_indexed_bools(stdout: &[u8]) -> Vec<bool> {
    let text = String::from_utf8_lossy(stdout);
    let mut lines: Vec<(usize, bool)> = Vec::new();
    for line in text.lines() {
        let Some((index, value)) = line.split_once(" -> ") else {
            continue;
        };
        let Ok(index) = index.trim().parse::<usize>() else {
            continue;
        };
        let value = value.trim();
        let parsed = match value {
            "true" => true,
            "false" => false,
            other => panic!("the oracle printed a non-boolean value `{other}`"),
        };
        lines.push((index, parsed));
    }
    lines.sort_by_key(|(index, _)| *index);
    assert_eq!(
        lines.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
        (1..=lines.len()).collect::<Vec<_>>(),
        "the oracle's answers are indexed 1..n:\n{text}"
    );
    lines.into_iter().map(|(_, value)| value).collect()
}

/// The oracle program for [`decentralized_counters_agrees_with_the_live_oracle`]:
/// one argument per case, `<kind>:<access>:<wc>:<dc>` with `wc` ∈ `{0,1}` and `dc`
/// ∈ `{-1 omitted, 0 false, 1 true}`; it builds the equivalent `ets:new/2` option
/// list, prints `"<index> -> <decentralized_counters>"` and deletes the table.
const COUNTERS_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript

main(Args) ->
    {_, _} = lists:foldl(
        fun(Arg, {Index, _}) ->
            Tab = ets:new(diff, options(Arg)),
            io:format("~w -> ~w~n", [Index, ets:info(Tab, decentralized_counters)]),
            ets:delete(Tab),
            {Index + 1, ok}
        end,
        {1, ok},
        Args),
    ok.

options(Arg) ->
    [KindS, AccessS, WcS, DcS] = string:tokens(Arg, ":"),
    Base = [list_to_atom(KindS), list_to_atom(AccessS)],
    WithWc = case WcS of
                 "1" -> Base ++ [{write_concurrency, true}];
                 "0" -> Base
             end,
    case DcS of
        "-1" -> WithWc;
        "0" -> WithWc ++ [{decentralized_counters, false}];
        "1" -> WithWc ++ [{decentralized_counters, true}]
    end.
"#;

/// The oracle program for [`creation_time_heir_is_adopted`]: one argument per
/// case (`none` / `arity2` / `arity3` / `absent`); it prints
/// `"<index> -> <ets:info(T, heir) =/= none>"`.
const HEIR_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript

main(Args) ->
    {_, _} = lists:foldl(
        fun(Spec, {Index, _}) ->
            Tab = ets:new(diff, [set | heir_options(Spec)]),
            io:format("~w -> ~w~n", [Index, ets:info(Tab, heir) =/= none]),
            ets:delete(Tab),
            {Index + 1, ok}
        end,
        {1, ok},
        Args),
    ok.

heir_options("none") -> [{heir, none}];
heir_options("arity2") -> [{heir, self()}];
heir_options("arity3") -> [{heir, self(), gift}];
heir_options("absent") -> [].
"#;
