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

//! Integration tests for the **adopted** `ets:*` verbs — `ets:all/0`,
//! `ets:setopts/2`, `ets:give_away/3`, `ets:foldl/3` and `ets:foldr/3` — plus the
//! typed-gap contract for the sub-option and the verbs that stay honest gaps.
//!
//! Every behaviour asserted here was probed against the system `erl`
//! (OTP 29.1.1 / erts-17.1) first; the probes are quoted beside the assertions.
//! Terms are compared through the context's atom table ([`beam_term::atom_val`] +
//! [`AtomTable::name`]) and through the registered `erlang`/`lists` BIFs, so the
//! suite uses only the public API.
//!
//! The probes (`erl -noshell`, erts-17.1):
//!
//! ```text
//! 1> T = ets:new(t, [set]), ets:all().
//! [#Ref<...>, ..., code_server, ..., logger, ac_tab]   % T's tid is a ref
//! 2> N = ets:new(ntab, [named_table, set]), lists:member(ntab, ets:all()).
//! true                                                  % a named table by name atom
//! 3> ets:setopts(T, [{heir, self(), 7}]), ets:info(T, heir).
//! {true, <0.80.0>}
//! 4> ets:setopts(T, [{heir, none}]), ets:info(T, heir).
//! {true, none}
//! 5> ets:setopts(T, {heir, self()}).                    % a bare tuple is accepted
//! true
//! 6> ets:setopts(T, [{heir, none}, {heir, none}]).
//! ** exception error: bad argument
//! 7> ets:setopts(T, [{protection, public}]).            % accepted by the oracle
//! true
//! 8> ets:setopts(T, [{heir, 5}]).
//! ** exception error: bad argument
//! 9> ets:give_away(T, self(), x).
//! ** exception error: bad argument                      % the caller's own pid
//! 10> P = spawn(fun() -> receive _ -> ok end end),
//!     ets:give_away(T, P, x), ets:info(T, owner).
//! {true, <0.83.0>}
//! 11> ets:give_away(T, P, x).                           % a non-owner now
//! ** exception error: bad argument
//! 12> ets:foldl(notfun, [], ets:new(e, [set])).         % empty: no fun call
//! []
//! 13> ets:foldl(fun({_,V}, A) -> [V|A] end, [], T2).    % forward traversal
//! 14> ets:foldr(fun({_,V}, A) -> [V|A] end, [], T2).    % the exact reverse
//! ```

use beam_alloc::Heap;
use beam_bifs::{default_registry, BifContext, BifRegistry, TableRegistry};
use beam_common::{BeamError, ExitReason};
use beam_process::ProcessId;
use beam_term::{atom_val, AtomTable, Eterm};

/// The atom names these tests need.
fn seeded_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed([
        "true",
        "false",
        "undefined",
        "set",
        "named_table",
        "public",
        "protected",
        "private",
        "heir",
        "none",
        "protection",
        "owner",
        "section",
        "ntab",
        "justname",
        "nosuch",
        "e",
        "k",
        "a",
        "notfun",
    ]);
    atoms
}

/// The pid `ets:new/2` records as the owner of a table this suite creates.
fn self_pid() -> ProcessId {
    ProcessId::new(1, 0)
}

/// `ets:new(Name, Options)` through the registry (`Options` a list of atom names).
fn new_table(
    bifs: &BifRegistry,
    ctx: &mut BifContext<'_>,
    name: &str,
    options: &[&str],
) -> Result<Eterm, BeamError> {
    let name = ctx.atom(name)?;
    let mut opts = Vec::with_capacity(options.len());
    for option in options {
        opts.push(ctx.atom(option)?);
    }
    let opts = ctx.list(&opts)?;
    bifs.call("ets", "new", 2, ctx, &[name, opts])
}

/// The one-element option list holding `elems` as its single tuple option.
fn opt_list(ctx: &mut BifContext<'_>, elems: &[Eterm]) -> Eterm {
    let opt = ctx.tuple(elems).expect("option tuple");
    ctx.list(&[opt]).expect("option list")
}

/// The atom `name` as a term (the name is seeded).
fn atom(ctx: &mut BifContext<'_>, name: &str) -> Eterm {
    ctx.atom(name).expect("seeded")
}

/// `ets:info(Tab, Item)` through the registry.
fn info(bifs: &BifRegistry, ctx: &mut BifContext<'_>, table: Eterm, item: Eterm) -> Eterm {
    bifs.call("ets", "info", 2, ctx, &[table, item])
        .expect("ets:info/2")
}

/// Asserts `term` is the atom `expected`.
fn assert_atom(ctx: &BifContext<'_>, term: Eterm, expected: &str) {
    assert_eq!(
        ctx.atoms().name(atom_val(term)),
        Some(expected),
        "expected the atom {expected}, got {term:?}"
    );
}

/// Asserts the call returned the atom `true`.
fn assert_true(ctx: &BifContext<'_>, term: Eterm) {
    assert_atom(ctx, term, "true");
}

/// Asserts `err` is the oracle's `badarg`.
fn assert_badarg(err: BeamError) {
    assert!(
        matches!(err, BeamError::BadArg),
        "expected badarg, got {err:?}"
    );
}

/// Asserts `err` is this crate's typed capability gap.
fn assert_unsupported(err: BeamError) {
    match err {
        BeamError::Raise(raise) => assert_eq!(
            raise.reason,
            ExitReason::atom("unsupported_bif"),
            "expected the typed unsupported_bif gap"
        ),
        other => panic!("expected unsupported_bif, got {other:?}"),
    }
}

// ------------------------------------------------------------------- all/0

/// `ets:all/0` answers the **name atom** of a `named_table` and the **handle
/// reference** of an unnamed one; every registered table is a member (probes 1, 2).
#[test]
fn all_0_lists_the_named_atom_and_the_handle_ref() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let unnamed = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let named = new_table(&bifs, &mut ctx, "ntab", &["named_table", "set"]).expect("ets:new");
    assert_eq!(
        named,
        atom(&mut ctx, "ntab"),
        "a named table's handle is its name atom"
    );

    let all = bifs.call("ets", "all", 0, &mut ctx, &[]).expect("ets:all");
    let len = bifs
        .call("erlang", "length", 1, &mut ctx, &[all])
        .expect("length");
    assert_eq!(len, ctx.int(2).expect("small int"), "both tables are listed");
    for member in [unnamed, named] {
        let found = bifs
            .call("lists", "member", 2, &mut ctx, &[member, all])
            .expect("lists:member");
        assert_true(&ctx, found);
    }
}

/// `ets:all/0` on a registry with no tables is the empty list.
#[test]
fn all_0_is_empty_without_tables() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let all = bifs.call("ets", "all", 0, &mut ctx, &[]).expect("ets:all");
    let len = bifs
        .call("erlang", "length", 1, &mut ctx, &[all])
        .expect("length");
    assert_eq!(len, ctx.int(0).expect("small int"));
}

// ---------------------------------------------------------------- setopts/2

/// The `{heir, …}` options install and clear the heir; both the list and the
/// bare-tuple argument shapes are accepted (probes 3, 4, 5).
#[test]
fn setopts_installs_and_clears_the_heir() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let heir_key = atom(&mut ctx, "heir");
    let target = ProcessId::new(9, 0).to_eterm();
    let data = ctx.int(7).expect("small int");

    // A fresh table's heir is `none`.
    let info = bifs
        .call("ets", "info", 2, &mut ctx, &[table, heir_key])
        .expect("ets:info");
    assert_atom(&ctx, info, "none");

    // `[{heir, Pid, Data}]` → true and `info(T, heir)` is the pid (probe 3).
    let opts = opt_list(&mut ctx, &[heir_key, target, data]);
    let ret = bifs
        .call("ets", "setopts", 2, &mut ctx, &[table, opts])
        .expect("ets:setopts");
    assert_true(&ctx, ret);
    let info = bifs
        .call("ets", "info", 2, &mut ctx, &[table, heir_key])
        .expect("ets:info");
    assert_eq!(info, target, "the heir is the installed pid");

    // A bare tuple `{heir, Pid}` is accepted too (probe 5).
    let opt = ctx.tuple(&[heir_key, target]).expect("option");
    let ret = bifs
        .call("ets", "setopts", 2, &mut ctx, &[table, opt])
        .expect("ets:setopts");
    assert_true(&ctx, ret);

    // `[{heir, none}]` clears it (probe 4).
    let none = atom(&mut ctx, "none");
    let opts = opt_list(&mut ctx, &[heir_key, none]);
    let ret = bifs
        .call("ets", "setopts", 2, &mut ctx, &[table, opts])
        .expect("ets:setopts");
    assert_true(&ctx, ret);
    let info = bifs
        .call("ets", "info", 2, &mut ctx, &[table, heir_key])
        .expect("ets:info");
    assert_atom(&ctx, info, "none");

    // An empty option list is accepted (and leaves the heir unset).
    let ret = bifs
        .call("ets", "setopts", 2, &mut ctx, &[table, beam_term::NIL])
        .expect("ets:setopts");
    assert_true(&ctx, ret);
}

/// Every malformed option shape the C rejects is `badarg` (probes 6, 8), as is an
/// unknown table.
#[test]
fn setopts_rejects_the_malformed_options() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let heir_key = atom(&mut ctx, "heir");
    let none = atom(&mut ctx, "none");
    let unknown_tag = atom(&mut ctx, "k");
    let target = ProcessId::new(9, 0).to_eterm();
    let data = ctx.int(7).expect("small int");
    let small = ctx.int(5).expect("small int");

    // `{heir, 5}`: a non-pid, non-`none` heir.
    let bad_pid = opt_list(&mut ctx, &[heir_key, small]);
    // `{heir, none, Data}`: `none` is only valid at arity 2.
    let bad_heir_none = opt_list(&mut ctx, &[heir_key, none, data]);
    // A duplicate `{heir, _}`.
    let dup_heir = {
        let opt = ctx.tuple(&[heir_key, target]).expect("tuple");
        ctx.list(&[opt, opt]).expect("list")
    };
    // An unknown tag `{k, 1}`.
    let bad_tag = opt_list(&mut ctx, &[unknown_tag, small]);
    // A non-atom option tag.
    let no_tag = ctx.tuple(&[small, small]).expect("tuple");
    // A too-short tuple.
    let short = ctx.tuple(&[heir_key]).expect("tuple");
    // An improper list.
    let improper = ctx.cons(target, data).expect("cons");

    for bad in [bad_pid, bad_heir_none, dup_heir, bad_tag, no_tag, short, improper] {
        let err = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, bad])
            .expect_err("badarg");
        assert_badarg(err);
    }
    // A bare non-tuple option and a bare atom are `badarg` too.
    for bad in [small, unknown_tag] {
        let err = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, bad])
            .expect_err("badarg");
        assert_badarg(err);
    }
    // An unknown table is `badarg`.
    let unknown = atom(&mut ctx, "nosuch");
    let opts = opt_list(&mut ctx, &[heir_key, target]);
    let err = bifs
        .call("ets", "setopts", 2, &mut ctx, &[unknown, opts])
        .expect_err("badarg");
    assert_badarg(err);
}

/// `{protection, P}` is a **supported** option: `ets:setopts/2` applies the access
/// mode through the owner-gated `Table::setopts` and returns `true`, and
/// `ets:info(T, protection)` reflects it immediately. All three modes, the
/// bare-tuple form and the `{heir, …}` combination are applied; a duplicate
/// `{protection, _}`, an unknown mode and a 3-tuple `{protection, P, X}` are the
/// oracle's `badarg`, and a non-owner is `badarg` leaving the protection
/// unchanged.
///
/// Probes (`erl -noshell`, erts-17.1):
///
/// ```text
/// 1> T = ets:new(t, [set]),
///    {ets:setopts(T, [{protection, public}]), ets:info(T, protection)}.
/// {true, public}
/// 2> {ets:setopts(T, [{protection, private}]), ets:info(T, protection)}.
/// {true, private}
/// 3> {ets:setopts(T, [{heir, none}, {protection, protected}]), ets:info(T, protection)}.
/// {true, protected}
/// 4> ets:setopts(T, {protection, public}).               % a bare tuple is accepted
/// true
/// 5> ets:setopts(T, [{protection, public}, {protection, private}]).
/// ** exception error: bad argument
/// 6> ets:setopts(T, {protection, bogus}).
/// ** exception error: bad argument
/// 7> ets:setopts(T, {protection, public, 7}).
/// ** exception error: bad argument
/// 8> T2 = ets:new(p, [set, public]),                     % a non-owner of a public table
///    catch ets:setopts(T2, [{protection, private}]).     % (a spawned child process)
/// {'EXIT', {badarg, …}}                                  % info(T2, protection) stays public
/// ```
#[test]
fn setopts_protection_is_supported() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let owner = self_pid();
    let other = ProcessId::new(2, 0);

    // The owner drives the whole `{protection, _}` matrix on a `public` table
    // (so a non-owner would reach the owner check rather than the write gate).
    let (table, protection, private) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, owner).with_tables(&mut tables);
        let table = new_table(&bifs, &mut ctx, "justname", &["set", "public"]).expect("ets:new");
        let protection = atom(&mut ctx, "protection");
        let public = atom(&mut ctx, "public");
        let private = atom(&mut ctx, "private");
        let protected = atom(&mut ctx, "protected");
        let bogus = atom(&mut ctx, "k");
        let heir = atom(&mut ctx, "heir");
        let none = atom(&mut ctx, "none");

        // `[{protection, public}]` ⇒ `true`, `info(T, protection)` ⇒ `public` (probe 1).
        let opts = opt_list(&mut ctx, &[protection, public]);
        let ret = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, opts])
            .expect("ets:setopts");
        assert_true(&ctx, ret);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "public");

        // `[{protection, private}]` ⇒ `true`, `info` ⇒ `private` (probe 2).
        let opts = opt_list(&mut ctx, &[protection, private]);
        let ret = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, opts])
            .expect("ets:setopts");
        assert_true(&ctx, ret);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "private");

        // `[{heir, none}, {protection, protected}]` ⇒ `true`, `info` ⇒ `protected`
        // (probe 3) — the `{heir, …}` and `{protection, …}` options compose.
        let heir_opt = ctx.tuple(&[heir, none]).expect("option");
        let prot_opt = ctx.tuple(&[protection, protected]).expect("option");
        let opts = ctx.list(&[heir_opt, prot_opt]).expect("option list");
        let ret = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, opts])
            .expect("ets:setopts");
        assert_true(&ctx, ret);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "protected");

        // A bare `{protection, public}` tuple is accepted ⇒ `true`, `info` ⇒ `public`
        // (probe 4).
        let bare = ctx.tuple(&[protection, public]).expect("bare option");
        let ret = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, bare])
            .expect("ets:setopts");
        assert_true(&ctx, ret);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "public");

        // A duplicate `{protection, _}` is `badarg` and leaves the mode untouched
        // (probe 5).
        let dup = {
            let one = ctx.tuple(&[protection, public]).expect("option");
            let two = ctx.tuple(&[protection, private]).expect("option");
            ctx.list(&[one, two]).expect("option list")
        };
        let err = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, dup])
            .expect_err("badarg");
        assert_badarg(err);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "public");

        // An unknown mode `{protection, bogus}` is `badarg` (unchanged, probe 6).
        let bad = opt_list(&mut ctx, &[protection, bogus]);
        let err = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, bad])
            .expect_err("badarg");
        assert_badarg(err);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "public");

        // A 3-tuple `{protection, public, 7}` is `badarg` (unchanged, probe 7).
        let seven = ctx.int(7).expect("small int");
        let three = ctx.tuple(&[protection, public, seven]).expect("option");
        let err = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, three])
            .expect_err("badarg");
        assert_badarg(err);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "public");

        (table, protection, private)
    };

    // A non-owner is `badarg` and leaves the protection unchanged (probe 8). The
    // table is `public`, so the write gate admits the caller and the *owner* check
    // is what denies it.
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, other).with_tables(&mut tables);
        let opts = opt_list(&mut ctx, &[protection, private]);
        let err = bifs
            .call("ets", "setopts", 2, &mut ctx, &[table, opts])
            .expect_err("badarg");
        assert_badarg(err);
    }
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, owner).with_tables(&mut tables);
        let got = info(&bifs, &mut ctx, table, protection);
        assert_atom(&ctx, got, "public");
    }
}

/// `ets:setopts/2` requires the owner — after a `give_away` the former owner is
/// denied.
#[test]
fn setopts_requires_the_owner() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let heir_key = atom(&mut ctx, "heir");
    let none = atom(&mut ctx, "none");
    let gift = atom(&mut ctx, "a");
    let target = ProcessId::new(9, 0).to_eterm();

    let ret = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, target, gift])
        .expect("ets:give_away");
    assert_true(&ctx, ret);

    let opts = opt_list(&mut ctx, &[heir_key, none]);
    let err = bifs
        .call("ets", "setopts", 2, &mut ctx, &[table, opts])
        .expect_err("badarg");
    assert_badarg(err);
}

// -------------------------------------------------------------- give_away/3

/// `ets:give_away/3` transfers ownership to a live different pid and returns
/// `true`; `ets:info(T, owner)` reports the new owner (probe 10).
#[test]
fn give_away_transfers_ownership() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let owner_key = atom(&mut ctx, "owner");
    let gift = atom(&mut ctx, "a");
    let target = ProcessId::new(9, 0).to_eterm();

    let info = bifs
        .call("ets", "info", 2, &mut ctx, &[table, owner_key])
        .expect("ets:info");
    assert_eq!(info, self_pid().to_eterm(), "the creator owns the table");

    let ret = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, target, gift])
        .expect("ets:give_away");
    assert_true(&ctx, ret);
    let info = bifs
        .call("ets", "info", 2, &mut ctx, &[table, owner_key])
        .expect("ets:info");
    assert_eq!(info, target, "the new owner is the target pid");
}

/// The self-gift, a non-pid target, a non-owner caller and an unknown table are
/// each `badarg` (probes 9, 11).
#[test]
fn give_away_rejects_self_bad_pids_and_non_owners() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let gift = atom(&mut ctx, "a");
    let target = ProcessId::new(9, 0).to_eterm();
    let small = ctx.int(5).expect("small int");

    // The caller's own pid.
    let mine = self_pid().to_eterm();
    let err = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, mine, gift])
        .expect_err("badarg");
    assert_badarg(err);
    // A non-pid target.
    let err = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, small, gift])
        .expect_err("badarg");
    assert_badarg(err);
    // An unknown table argument.
    let unknown = atom(&mut ctx, "nosuch");
    let err = bifs
        .call("ets", "give_away", 3, &mut ctx, &[unknown, target, gift])
        .expect_err("badarg");
    assert_badarg(err);

    // After the transfer the former owner is denied.
    let ret = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, target, gift])
        .expect("ets:give_away");
    assert_true(&ctx, ret);
    let third = ProcessId::new(10, 0).to_eterm();
    let err = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, third, gift])
        .expect_err("badarg");
    assert_badarg(err);
}

// ------------------------------------------------------------ foldl/3 foldr/3

/// An empty table answers `Acc0` **without** calling the fun, so no `FunCaller`
/// is needed (probe 12).
#[test]
fn folds_on_an_empty_table_return_the_accumulator() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let empty = new_table(&bifs, &mut ctx, "e", &["set"]).expect("ets:new");
    let notfun = atom(&mut ctx, "notfun");
    let acc = atom(&mut ctx, "section");

    for verb in ["foldl", "foldr"] {
        let ret = bifs
            .call("ets", verb, 3, &mut ctx, &[notfun, acc, empty])
            .expect("an empty fold");
        assert_eq!(ret, acc, "ets:{verb}/3 answers Acc0 untouched");
    }
}

/// A fold table argument that names no table is `badarg` — including the *name
/// field* of a table created without `named_table`. The empty-table short circuit
/// must not hide it.
#[test]
fn folds_reject_a_table_argument_that_names_no_table() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    // A table whose name field is `justname` but which is not a `named_table`.
    let _ = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let notfun = atom(&mut ctx, "notfun");
    let acc = atom(&mut ctx, "section");
    let justname = atom(&mut ctx, "justname");
    let nosuch = atom(&mut ctx, "nosuch");

    for table in [justname, nosuch] {
        let err = bifs
            .call("ets", "foldl", 3, &mut ctx, &[notfun, acc, table])
            .expect_err("badarg");
        assert_badarg(err);
    }
}

/// With no `FunCaller` installed a non-empty fold is the typed `unsupported_bif`
/// gap — never a fabricated accumulator.
#[test]
fn folds_without_a_fun_caller_are_a_typed_gap() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let key = atom(&mut ctx, "k");
    let value = ctx.int(1).expect("small int");
    let object = ctx.tuple(&[key, value]).expect("object");
    let ret = bifs
        .call("ets", "insert", 2, &mut ctx, &[table, object])
        .expect("ets:insert");
    assert_true(&ctx, ret);

    let notfun = atom(&mut ctx, "notfun");
    let acc = atom(&mut ctx, "section");
    for verb in ["foldl", "foldr"] {
        let err = bifs
            .call("ets", verb, 3, &mut ctx, &[notfun, acc, table])
            .expect_err("the typed gap");
        assert_unsupported(err);
    }
}

// -------------------------------------------------------- owner→handle roster

/// The runtime's reclamation seam, reached through the **public** registry API: a
/// dead owner's tables are enumerated with `handles_owned_by` (keyed by
/// `owner_of`, the lossless pid→owner bridge) and dropped through the existing
/// `remove`/`remove_name` path.
///
/// An **unnamed** table's handle is not derivable from any public term, so this
/// roster is the only way a host can enumerate it — the adoption gap the runtime's
/// owner-death reclamation needed closed.
#[test]
fn the_owner_roster_lists_a_pids_handles_ascending() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let alpha = self_pid();
    let beta = ProcessId::new(2, 7);

    // Alpha owns one unnamed table and one `named_table`; beta owns one unnamed
    // table. Each `ets:new/2` runs as the creating process's own pid, so the
    // registry key is set per context.
    let named_term = {
        let mut ctx = BifContext::new(&mut heap, &atoms, alpha).with_tables(&mut tables);
        new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
        new_table(&bifs, &mut ctx, "ntab", &["named_table", "set"]).expect("ets:new")
    };
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, beta).with_tables(&mut tables);
        new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    }
    // The named table's handle *is* its registered name's target.
    let named_handle = tables
        .lookup_name(atom_val(named_term))
        .expect("the named table is registered");

    let alpha_roster = tables.handles_owned_by(TableRegistry::owner_of(alpha));
    let beta_roster = tables.handles_owned_by(TableRegistry::owner_of(beta));

    assert_eq!(alpha_roster.len(), 2, "alpha owns two tables: {alpha_roster:?}");
    assert_eq!(beta_roster.len(), 1, "beta owns one table: {beta_roster:?}");
    assert!(
        alpha_roster.contains(&named_handle),
        "the named table's handle is listed: {alpha_roster:?} vs {named_handle}"
    );
    assert!(
        alpha_roster.windows(2).all(|pair| pair[0] < pair[1]),
        "ascending by handle: {alpha_roster:?}"
    );
    assert!(
        alpha_roster.iter().all(|handle| !beta_roster.contains(handle)),
        "the two owners' rosters are disjoint"
    );

    // Every listed handle names a live table, so each can be dropped (the
    // runtime's withdrawal) through the existing removal path.
    let dead = alpha_roster.clone();
    for handle in &dead {
        assert!(
            tables.get(*handle).is_some(),
            "handle {handle} names a live table"
        );
    }
    for handle in dead {
        tables.remove(handle);
    }
    tables.remove_name(atom_val(named_term));
    assert!(
        tables.handles_owned_by(TableRegistry::owner_of(alpha)).is_empty(),
        "the withdrawn tables leave the roster"
    );

    // A pid that owns nothing gets an empty roster.
    assert!(tables
        .handles_owned_by(TableRegistry::owner_of(ProcessId::new(3, 9)))
        .is_empty());

    // `owner_of` is lossless: the raw owner id is the pid immediate, exactly what
    // `ets:new/2` recorded (and what `beam_rt`'s own `owner_key` computes).
    for pid in [alpha, beta, ProcessId::new(3, 9)] {
        assert_eq!(
            TableRegistry::owner_of(pid).raw(),
            pid.to_eterm().raw() as u64
        );
    }
}
