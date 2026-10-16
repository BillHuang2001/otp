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
//! 13> ets:foldl(fun(X, A) -> [X|A] end, [], T2).        % forward traversal
//! 14> ets:foldr(fun(X, A) -> [X|A] end, [], T2).        % last-to-first *only*
//!                                                       % for an ordered_set;
//!                                                       % a hash kind keeps 13's
//!                                                       % key order and reverses
//!                                                       % each key's objects
//! ```

use std::cell::RefCell;
use std::process::Command;
use std::rc::Rc;

use beam_alloc::Heap;
use beam_bifs::{
    default_registry, BifContext, BifRegistry, FunCaller, SendHook, TableRegistry,
};
use beam_codec::term::{cons_head, cons_tail, tuple_element};
use beam_common::{BeamError, ExitReason, Result as BmResult};
use beam_process::ProcessId;
use beam_term::{
    atom_val, is_list, is_nil, is_small, make_header, signed_value, AtomTable, Eterm, NIL,
};

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
        // The `ets:give_away/3` transfer message and the `ets:info/2` items the
        // transfer tests read (`id`/`size`/`memory`), plus a gift's own tag atom.
        "ETS-TRANSFER",
        "id",
        "size",
        "memory",
        "complex",
    ]);
    atoms
}

/// The pid `ets:new/2` records as the owner of a table this suite creates.
fn self_pid() -> ProcessId {
    ProcessId::new(1, 0)
}

/// The shared delivery journal a [`RecordingSend`] double appends to: a test keeps
/// this handle so it can inspect what was delivered **while** the context still
/// borrows the hook.
type Journal = Rc<RefCell<Vec<(Eterm, Eterm)>>>;

/// A recording [`SendHook`] double: `ets:give_away/3`'s delivery appends each
/// `(to, message)` pair to a **shared** cell, so a test can inspect what was
/// delivered while the context still borrows the hook.
#[derive(Clone, Default)]
struct RecordingSend {
    sent: Journal,
}

impl RecordingSend {
    /// A handle on the (shared) delivery journal, usable alongside the hook.
    fn journal(&self) -> Journal {
        Rc::clone(&self.sent)
    }
}

impl SendHook for RecordingSend {
    fn send(&mut self, to: Eterm, term: Eterm) -> BmResult<()> {
        self.sent.borrow_mut().push((to, term));
        Ok(())
    }
}

/// The `index`-th element of the 4-tuple `message` a [`RecordingSend`] recorded.
fn elem(message: Eterm, index: usize) -> Eterm {
    tuple_element(message, index).expect("a message element")
}

/// Every `(to, message)` pair recorded in `journal`, in delivery order.
fn recorded(journal: &Journal) -> Vec<(Eterm, Eterm)> {
    journal.borrow().clone()
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

/// As [`assert_atom`], but read through the shared table — usable after the context
/// that built the term has been dropped.
fn assert_atom_name(atoms: &AtomTable, term: Eterm, expected: &str) {
    assert_eq!(
        atoms.name(atom_val(term)),
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
    let mut hook = RecordingSend::default();
    let journal = hook.journal();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid())
        .with_tables(&mut tables)
        .with_sender(&mut hook);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let heir_key = atom(&mut ctx, "heir");
    let none = atom(&mut ctx, "none");
    let gift = atom(&mut ctx, "a");
    let target = ProcessId::new(9, 0).to_eterm();

    let ret = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, target, gift])
        .expect("ets:give_away");
    assert_true(&ctx, ret);
    assert_eq!(
        recorded(&journal).len(),
        1,
        "the transfer delivered its gift"
    );

    let opts = opt_list(&mut ctx, &[heir_key, none]);
    let err = bifs
        .call("ets", "setopts", 2, &mut ctx, &[table, opts])
        .expect_err("badarg");
    assert_badarg(err);
}

// -------------------------------------------------------------- give_away/3

/// `ets:give_away/3` transfers ownership to a live different pid and returns
/// `true`; `ets:info(T, owner)` reports the new owner (probe 10) and the delivery
/// is the `{'ETS-TRANSFER', Tid, FromPid, GiftData}` message, with an **unnamed**
/// table's tid a reference that names the very table that moved.
#[test]
fn give_away_transfers_ownership() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut hook = RecordingSend::default();
    let journal = hook.journal();

    let (target, gift) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, self_pid())
            .with_tables(&mut tables)
            .with_sender(&mut hook);

        let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
        let owner_key = atom(&mut ctx, "owner");
        let gift = atom(&mut ctx, "a");
        let target = ProcessId::new(9, 0).to_eterm();

        let owner_now = info(&bifs, &mut ctx, table, owner_key);
        assert_eq!(owner_now, self_pid().to_eterm(), "the creator owns the table");

        let ret = bifs
            .call("ets", "give_away", 3, &mut ctx, &[table, target, gift])
            .expect("ets:give_away");
        assert_true(&ctx, ret);
        let owner_now = info(&bifs, &mut ctx, table, owner_key);
        assert_eq!(owner_now, target, "the new owner is the target pid");
        (target, gift)
    };

    // The delivery: one `{'ETS-TRANSFER', Tid, FromPid, GiftData}` to the new owner.
    let (to, message) = {
        let recorded = recorded(&journal);
        assert_eq!(recorded.len(), 1, "exactly one transfer message");
        recorded[0]
    };
    assert_eq!(to, target, "delivered to the new owner");
    assert_atom_name(&atoms, elem(message, 0), "ETS-TRANSFER");
    assert_eq!(
        elem(message, 2),
        self_pid().to_eterm(),
        "FromPid is the calling (former owner) pid"
    );
    assert_eq!(elem(message, 3), gift, "GiftData is carried verbatim");

    // An unnamed table's tid is a **reference** — and it resolves back to the very
    // table that moved, so it is not a fabricated handle.
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);
    let tid = elem(message, 1);
    let is_ref = bifs
        .call("erlang", "is_reference", 1, &mut ctx, &[tid])
        .expect("erlang:is_reference/1");
    assert_true(&ctx, is_ref);
    let size_key = atom(&mut ctx, "size");
    let size = info(&bifs, &mut ctx, tid, size_key);
    assert_eq!(
        size,
        ctx.int(0).expect("small int"),
        "the tid names the transferred table"
    );
}

/// The self-gift, a non-pid target, a non-owner caller and an unknown table are
/// each `badarg` (probes 9, 11), and a rejected call delivers nothing.
#[test]
fn give_away_rejects_self_bad_pids_and_non_owners() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut hook = RecordingSend::default();
    let journal = hook.journal();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid())
        .with_tables(&mut tables)
        .with_sender(&mut hook);

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
    assert!(
        recorded(&journal).is_empty(),
        "a rejected call delivers no transfer message"
    );

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
    assert_eq!(
        recorded(&journal).len(),
        1,
        "only the successful transfer delivered"
    );
}

/// The requested case `ets/give_away_gift_data_reaches_the_new_owner` (A.1): the
/// `GiftData` argument survives the transfer — an arbitrary term is carried intact
/// to the new owner, the giver is the calling pid, and for a `named_table` the tid
/// is its **name atom** even when the caller passed the table by its *handle
/// reference* (so the tid comes from the registry, not from the argument's shape).
///
/// The gift owns no counted storage after the transfer: `give_away_with_gift`
/// checks it out (`relinquish`), so `ets:info(T, memory)` is unchanged.
///
/// The second half pins the honest gap: with **no `SendHook`** the delivery cannot
/// happen, so the call is the typed `unsupported_bif` and the owner never moves.
#[test]
fn give_away_gift_data_reaches_the_new_owner() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut hook = RecordingSend::default();
    let journal = hook.journal();

    let (named, tid_ref, target, gift, memory_before) = {
        let mut ctx = BifContext::new(&mut heap, &atoms, self_pid())
            .with_tables(&mut tables)
            .with_sender(&mut hook);

        let named = new_table(&bifs, &mut ctx, "ntab", &["named_table", "set"]).expect("ets:new");
        assert_eq!(
            named,
            atom(&mut ctx, "ntab"),
            "a named table's handle is its name"
        );
        let id_key = atom(&mut ctx, "id");
        let memory_key = atom(&mut ctx, "memory");
        let memory_before = info(&bifs, &mut ctx, named, memory_key);
        // The named table's **handle reference** (`ets:info(T, id)`), so the transfer
        // is driven by a ref even though the table *is* a `named_table`.
        let tid_ref = info(&bifs, &mut ctx, named, id_key);
        assert_ne!(tid_ref, named, "the id is the reference, not the name atom");

        // A gift with real structure: `{complex, [1,2,3], <caller>}`.
        let complex = atom(&mut ctx, "complex");
        let body = ctx
            .list(&[
                ctx.int(1).expect("small int"),
                ctx.int(2).expect("small int"),
                ctx.int(3).expect("small int"),
            ])
            .expect("list");
        let gift = ctx
            .tuple(&[complex, body, self_pid().to_eterm()])
            .expect("gift tuple");
        let target = ProcessId::new(9, 0).to_eterm();

        let ret = bifs
            .call("ets", "give_away", 3, &mut ctx, &[tid_ref, target, gift])
            .expect("ets:give_away");
        assert_true(&ctx, ret);
        (named, tid_ref, target, gift, memory_before)
    };

    let (to, message) = {
        let recorded = recorded(&journal);
        assert_eq!(recorded.len(), 1, "exactly one transfer message");
        recorded[0]
    };
    assert_eq!(to, target, "delivered to the new owner");
    assert_atom_name(&atoms, elem(message, 0), "ETS-TRANSFER");

    {
        let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);
        // The tid is the registry's name atom, not the reference the caller passed.
        assert_eq!(
            elem(message, 1),
            named,
            "the tid is the named_table's name atom"
        );
        assert_eq!(
            elem(message, 2),
            self_pid().to_eterm(),
            "FromPid is the calling pid"
        );
        // The gift arrives intact (the copy is structural, so it is not the caller's
        // own word — `=:=` is).
        let same = bifs
            .call("erlang", "=:=", 2, &mut ctx, &[elem(message, 3), gift])
            .expect("erlang:'=:='/2");
        assert_true(&ctx, same);
        // The gift is relinquished: the transfer does not count it.
        let memory_key = atom(&mut ctx, "memory");
        let memory_after = info(&bifs, &mut ctx, tid_ref, memory_key);
        assert_eq!(
            memory_after, memory_before,
            "the gift is relinquished, never counted by ets:info(T, memory)"
        );
    }

    // The typed gap: with no `SendHook` the delivery cannot happen, so the call is
    // `unsupported_bif` and nothing moves.
    {
        let mut ctx = BifContext::new(&mut heap, &atoms, self_pid()).with_tables(&mut tables);
        let owner_key = atom(&mut ctx, "owner");
        let plain = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
        let third = ProcessId::new(11, 0).to_eterm();
        let plain_gift = atom(&mut ctx, "a");
        let err = bifs
            .call("ets", "give_away", 3, &mut ctx, &[plain, third, plain_gift])
            .expect_err("unsupported_bif without a SendHook");
        assert_unsupported(err);
        let info_after = info(&bifs, &mut ctx, plain, owner_key);
        assert_eq!(
            info_after,
            self_pid().to_eterm(),
            "without a SendHook the owner never moves"
        );
    }
    assert_eq!(
        recorded(&journal).len(),
        1,
        "the hook-free call delivered nothing"
    );
}

/// An **uncopyable** gift (a bare header word — not a term at all) is the store's
/// typed `badarg`, and because `give_away_with_gift` copies **before** the owner
/// moves, the owner is left unchanged and nothing is delivered.
#[test]
fn give_away_with_an_uncopyable_gift_leaves_the_owner_unchanged() {
    let bifs = default_registry();
    let mut heap = Heap::new(1 << 16).expect("test heap");
    let atoms = seeded_atoms();
    let mut tables = TableRegistry::new();
    let mut hook = RecordingSend::default();
    let journal = hook.journal();
    let mut ctx = BifContext::new(&mut heap, &atoms, self_pid())
        .with_tables(&mut tables)
        .with_sender(&mut hook);

    let table = new_table(&bifs, &mut ctx, "justname", &["set"]).expect("ets:new");
    let owner_key = atom(&mut ctx, "owner");
    let target = ProcessId::new(9, 0).to_eterm();

    let bare_header = make_header(1, 0);
    let err = bifs
        .call("ets", "give_away", 3, &mut ctx, &[table, target, bare_header])
        .expect_err("badarg");
    assert_badarg(err);
    let info_after = info(&bifs, &mut ctx, table, owner_key);
    assert_eq!(
        info_after,
        self_pid().to_eterm(),
        "the copy-before-move order leaves the owner on a rejected gift"
    );
    assert!(
        recorded(&journal).is_empty(),
        "a rejected transfer delivers nothing"
    );
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

// ------------------------------------------------ foldl/3 foldr/3 traversal
//
// `ets:foldl/3`/`ets:foldr/3` are stdlib `ets.erl` functions (`do_foldl`/`do_foldr`,
// `stdlib/src/ets.erl:2510,2552`): a *key* walk (`first_lookup`/`next_lookup`,
// resp. `last_lookup`/`prev_lookup`) whose landed objects are folded with
// `lists:foldl`/`lists:foldr`. That makes two independent directions:
//
//   * `foldr` reverses each key's object list for **every** kind;
//   * `foldr` walks the keys last-to-first **only** where the interior can, i.e.
//     only for an `ordered_set` — a hash kind's `ets:last_lookup`/`ets:prev_lookup`
//     alias the forward primitives, so there `foldr` visits the keys in the very
//     same order as `foldl`.
//
// The `ordered_set` and single-key-`bag` probes below are order-*defined*, so they
// are pinned against the oracle's literal output; the multi-key hash-kind probes
// only pin the oracle's *relation* (whose absolute order the docs call
// "unspecified").

/// Sentinel "fun": `fun(Object, Acc) -> [Object | Acc] end` — the oracle probes'
/// own fun, so the accumulator can be compared with the oracle's printed list.
const COLLECT: i64 = 1;

/// Sentinel "fun": `fun({Key, _}, Acc) -> [Key | Acc] end` — collects the **key
/// walk** only, which is what distinguishes a hash kind from an `ordered_set`.
const COLLECT_KEY: i64 = 2;

/// The mock [`FunCaller`] the fold tests install: it runs exactly the bodies the
/// oracle probes ran, on its own heap.
struct MockFolds {
    heap: &'static mut Heap,
}

impl FunCaller for MockFolds {
    fn call_fun(&mut self, fun: Eterm, args: &[Eterm]) -> BmResult<Eterm> {
        assert!(is_small(fun), "the mock is only called with a sentinel fun");
        match signed_value(fun) {
            COLLECT => self.heap.cons(args[0], args[1]),
            COLLECT_KEY => {
                let key = tuple_element(args[0], 0).expect("a {Key, Value} object");
                self.heap.cons(key, args[1])
            }
            other => panic!("unknown mock fun {other}"),
        }
    }
}

/// The atom names the fold-traversal tests need.
fn fold_atoms() -> AtomTable {
    let mut atoms = AtomTable::new();
    atoms.seed(["true", "set", "ordered_set", "bag", "duplicate_bag", "t"]);
    atoms
}

/// Runs `ets:{verb}/3` over a freshly created `kind` table holding `objects`, with
/// the mock sentinel `fun`, and answers the resulting term (the fun's accumulator).
fn fold_objects(kind: &str, objects: &[(i64, i64)], verb: &str, fun: i64) -> Eterm {
    let bifs = default_registry();
    let atoms = fold_atoms();
    // Both heaps are leaked on purpose: the mock builds the accumulator on its own
    // heap and the caller decodes that term after this function returns (the
    // `tests/lists_funs.rs` pattern).
    let mock_heap: &'static mut Heap = Box::leak(Box::new(Heap::new(1 << 16).expect("mock heap")));
    let mut mock = MockFolds { heap: mock_heap };
    let heap: &'static mut Heap = Box::leak(Box::new(Heap::new(1 << 16).expect("heap")));
    let mut tables = TableRegistry::new();
    let mut ctx = BifContext::new(heap, &atoms, self_pid())
        .with_tables(&mut tables)
        .with_fun_caller(&mut mock);

    let name = atom(&mut ctx, "t");
    let kind_atom = atom(&mut ctx, kind);
    let options = ctx.list(&[kind_atom]).expect("option list");
    let table = bifs
        .call("ets", "new", 2, &mut ctx, &[name, options])
        .expect("ets:new");
    for &(k, v) in objects {
        let key = ctx.int(k).expect("small int");
        let value = ctx.int(v).expect("small int");
        let object = ctx.tuple(&[key, value]).expect("object");
        bifs.call("ets", "insert", 2, &mut ctx, &[table, object])
            .expect("ets:insert");
    }
    let fun = ctx.int(fun).expect("small int");
    bifs.call("ets", verb, 3, &mut ctx, &[fun, NIL, table])
        .expect("the fold")
}

/// The `{Key, Value}` pairs of a `COLLECT` accumulator, in the order the fun consed
/// them — i.e. **reverse** visit order, exactly the list the oracle probe printed.
fn accumulator(term: Eterm) -> Vec<(i64, i64)> {
    let pieces = proper_terms(term);
    pieces
        .iter()
        .map(|&object| {
            let key = tuple_element(object, 0).expect("a {Key, Value} object");
            let value = tuple_element(object, 1).expect("a {Key, Value} object");
            assert!(is_small(key) && is_small(value), "the mock collects small ints");
            (signed_value(key), signed_value(value))
        })
        .collect()
}

/// The application order of a fold: the accumulator reversed.
fn visit_order(term: Eterm) -> Vec<(i64, i64)> {
    let mut order = accumulator(term);
    order.reverse();
    order
}

/// Groups a visit order into consecutive same-key runs (`(key, values)`).
fn runs(visit: &[(i64, i64)]) -> Vec<(i64, Vec<i64>)> {
    let mut out: Vec<(i64, Vec<i64>)> = Vec::new();
    for &(key, value) in visit {
        match out.last_mut() {
            Some((last, values)) if *last == key => values.push(value),
            _ => out.push((key, vec![value])),
        }
    }
    out
}

/// The elements of a proper list built on a mock heap.
fn proper_terms(term: Eterm) -> Vec<Eterm> {
    let mut out = Vec::new();
    let mut cur = term;
    while is_list(cur) {
        out.push(cons_head(cur).expect("a cons head"));
        cur = cons_tail(cur).expect("a cons tail");
    }
    assert!(is_nil(cur), "expected a proper list, got a term ending in {cur:?}");
    out
}

/// `ets:foldl/3`/`ets:foldr/3` on an `ordered_set` — the one kind whose traversal
/// order the docs define — must reproduce the oracle verbatim: `foldl` walks
/// first-to-last, `foldr` last-to-first, and each key's object list is folded with
/// `lists:foldr`. (The pre-fix bridge walked `last`/`prev` *and* reversed the whole
/// collected vector, so `foldr` answered exactly what `foldl` did.)
///
/// Oracle probe (OTP 29.1.1 / erts-17.1):
/// ```text
/// T = ets:new(t, [ordered_set]), [ets:insert(T, {K, K*10}) || K <- lists:seq(1,5)],
/// ets:foldl(fun(X, A) -> [X|A] end, [], T) => [{5,50},{4,40},{3,30},{2,20},{1,10}]
/// ets:foldr(fun(X, A) -> [X|A] end, [], T) => [{1,10},{2,20},{3,30},{4,40},{5,50}]
/// ```
#[test]
fn folds_walk_an_ordered_set_last_to_first_like_the_oracle() {
    let objects: Vec<(i64, i64)> = (1..=5).map(|k| (k, k * 10)).collect();

    let forward = fold_objects("ordered_set", &objects, "foldl", COLLECT);
    assert_eq!(
        accumulator(forward),
        vec![(5, 50), (4, 40), (3, 30), (2, 20), (1, 10)],
        "ets:foldl/3 walks an ordered_set first-to-last"
    );

    let backward = fold_objects("ordered_set", &objects, "foldr", COLLECT);
    assert_eq!(
        accumulator(backward),
        vec![(1, 10), (2, 20), (3, 30), (4, 40), (5, 50)],
        "ets:foldr/3 walks an ordered_set last-to-first"
    );

    // The two are the exact reverse of one another (visit order).
    let mut expected = visit_order(forward);
    expected.reverse();
    assert_eq!(visit_order(backward), expected);
}

/// A `bag` holding several objects under **one** key: the oracle's key walk is
/// trivial there, so this pins the per-key object reversal verbatim — `foldl`
/// folds the key's objects with `lists:foldl`, `foldr` with `lists:foldr`.
///
/// Oracle probe (OTP 29.1.1 / erts-17.1):
/// ```text
/// B = ets:new(b, [bag]), ets:insert(B, [{1,10},{1,11},{1,12}]),
/// ets:foldl(fun(X, A) -> [X|A] end, [], B) => [{1,12},{1,11},{1,10}]
/// ets:foldr(fun(X, A) -> [X|A] end, [], B) => [{1,10},{1,11},{1,12}]
/// ```
#[test]
fn folds_reverse_a_key_objects_like_the_oracle() {
    let objects = vec![(1, 10), (1, 11), (1, 12)];
    for kind in ["bag", "duplicate_bag"] {
        assert_eq!(
            accumulator(fold_objects(kind, &objects, "foldl", COLLECT)),
            vec![(1, 12), (1, 11), (1, 10)],
            "{kind}: ets:foldl/3 folds the key's objects left to right"
        );
        assert_eq!(
            accumulator(fold_objects(kind, &objects, "foldr", COLLECT)),
            vec![(1, 10), (1, 11), (1, 12)],
            "{kind}: ets:foldr/3 folds the key's objects right to left"
        );
    }
}

/// The **hash-kind** rule: `ets:foldr/3` visits the keys in exactly the same order
/// as `ets:foldl/3` (its `last_lookup`/`prev_lookup` alias the forward primitives)
/// and only reverses each key's object list. A `set` has one object per key, so
/// there the two folds are literally identical.
#[test]
fn hash_kind_folds_keep_the_key_order_and_reverse_each_keys_objects() {
    let one_each: Vec<(i64, i64)> = (1..=5).map(|k| (k, k * 10)).collect();
    for kind in ["set", "bag", "duplicate_bag"] {
        let forward = fold_objects(kind, &one_each, "foldl", COLLECT);
        let backward = fold_objects(kind, &one_each, "foldr", COLLECT);
        let forward_order = visit_order(forward);
        let backward_order = visit_order(backward);
        assert_eq!(
            backward_order
                .iter()
                .map(|&(key, _)| key)
                .collect::<Vec<_>>(),
            forward_order.iter().map(|&(key, _)| key).collect::<Vec<_>>(),
            "{kind}: a hash kind keeps the foldl key order in foldr"
        );
        assert_eq!(
            backward_order, forward_order,
            "{kind}: with one object per key the two folds agree"
        );
    }

    // Multi-object keys: the key runs stay in order and only the values within a
    // run are reversed.
    let multi = vec![(1, 10), (1, 11), (1, 12), (2, 20), (2, 21), (3, 30)];
    for kind in ["bag", "duplicate_bag"] {
        let forward = runs(&visit_order(fold_objects(kind, &multi, "foldl", COLLECT)));
        let backward = runs(&visit_order(fold_objects(kind, &multi, "foldr", COLLECT)));
        assert_eq!(
            backward.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            forward.iter().map(|(key, _)| *key).collect::<Vec<_>>(),
            "{kind}: foldr keeps foldl's key order"
        );
        assert_eq!(forward.len(), 3, "{kind}: three key runs");
        for ((fkey, fvalues), (bkey, bvalues)) in forward.iter().zip(backward.iter()) {
            assert_eq!(fkey, bkey);
            let mut reversed = fvalues.clone();
            reversed.reverse();
            assert_eq!(
                &reversed, bvalues,
                "{kind}: only key {fkey}'s object list is reversed"
            );
        }
    }

    // And the key walk itself, collected on its own, agrees between the two folds.
    // One object per key, so the call count is the key count for every hash kind.
    for kind in ["set", "bag", "duplicate_bag"] {
        let forward_keys = proper_terms(fold_objects(kind, &one_each, "foldl", COLLECT_KEY));
        let backward_keys = proper_terms(fold_objects(kind, &one_each, "foldr", COLLECT_KEY));
        assert_eq!(
            backward_keys, forward_keys,
            "{kind}: foldr visits the keys in foldl's order"
        );
        assert_eq!(forward_keys.len(), one_each.len(), "{kind}: one call per object");
    }
}

/// A one-object table (any kind) folds to the same single-object list either way,
/// and the `ordered_set` fold of a single key is likewise direction-independent
/// (the oracle's `'$end_of_table'` short circuits).
#[test]
fn folds_on_a_single_object_agree_in_both_directions() {
    let one = vec![(7, 70)];
    for kind in ["set", "ordered_set", "bag", "duplicate_bag"] {
        assert_eq!(
            accumulator(fold_objects(kind, &one, "foldl", COLLECT)),
            vec![(7, 70)],
            "{kind}"
        );
        assert_eq!(
            accumulator(fold_objects(kind, &one, "foldr", COLLECT)),
            vec![(7, 70)],
            "{kind}"
        );
    }
}

// ------------------------------------------------------- live oracle check

/// Runs one `-eval` statement under the system `erl`; `None` when the oracle is
/// unavailable (the test then skips loudly).
fn oracle_eval(statement: &str) -> Option<String> {
    let output = Command::new("erl")
        .args(["-noshell", "-eval", statement, "-s", "init", "stop"])
        .output();
    match output {
        Ok(out) if out.status.success() => Some(
            String::from_utf8_lossy(&out.stdout)
                .trim_end_matches(['\n', '\r'])
                .to_string(),
        ),
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

/// The live cross-check for the fold traversal: re-runs the probe the goldens above
/// were measured from (an `ordered_set`, a single-key `bag`, and a multi-key `bag`
/// whose **key walk alone** is collected), so a future OTP change cannot silently
/// invalidate them. `~w` (not `~p`) keeps the oracle from line-wrapping.
#[test]
fn the_fold_traversals_agree_with_the_live_oracle() {
    let statement = concat!(
        "T = ets:new(ft_o, [ordered_set]), ",
        "[ets:insert(T, {K, K*10}) || K <- lists:seq(1,5)], ",
        "B = ets:new(ft_b, [bag]), ets:insert(B, [{1,10},{1,11},{1,12}]), ",
        "M = ets:new(ft_m, [bag]), ",
        "ets:insert(M, [{1,10},{1,11},{1,12},{2,20},{2,21},{3,30}]), ",
        "F = fun(X, A) -> [X|A] end, ",
        "K = fun({Key, _}, A) -> [Key|A] end, ",
        "io:format(\"~w~n\", [{",
        "ets:foldl(F, [], T), ets:foldr(F, [], T), ",
        "ets:foldl(F, [], B), ets:foldr(F, [], B), ",
        "ets:foldl(K, [], M), ets:foldr(K, [], M)}])"
    );
    let Some(actual) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        actual,
        "{[{5,50},{4,40},{3,30},{2,20},{1,10}],[{1,10},{2,20},{3,30},{4,40},{5,50}],\
         [{1,12},{1,11},{1,10}],[{1,10},{1,11},{1,12}],\
         [3,2,2,1,1,1],[3,2,2,1,1,1]}",
        "the live oracle disagrees with the pinned fold goldens"
    );
}

/// The live cross-check for the gift-carrying transfer: the oracle delivers
/// `{'ETS-TRANSFER', Tid, FromPid, GiftData}` to the new owner with the gift
/// intact and the giver the calling pid, and for a `named_table` the message's tid
/// is the **name atom** (an unnamed table's is a reference). `~w` (not `~p`) keeps
/// the oracle from line-wrapping, and no pid is printed, so the golden is stable.
#[test]
fn the_gift_carrying_transfer_agrees_with_the_live_oracle() {
    let statement = concat!(
        "Self = self(), ",
        "Pn = spawn(fun() -> receive M -> Self ! M end end), ",
        "Pu = spawn(fun() -> receive M -> Self ! M end end), ",
        "N = ets:new(ga_n, [named_table, set]), ",
        "ets:give_away(N, Pn, {complex, [1,2,3]}), ",
        "Rn = receive Xn -> Xn end, ",
        "U = ets:new(ga_u, [set]), ",
        "ets:give_away(U, Pu, 42), ",
        "Ru = receive Xu -> Xu end, ",
        "io:format(\"~w~n\", [{element(1, Rn), element(2, Rn), ",
        "element(3, Rn) =:= Self, element(4, Rn), tuple_size(Rn)}, ",
        "{element(1, Ru), is_reference(element(2, Ru)), ",
        "element(3, Ru) =:= Self, element(4, Ru)}])"
    );
    let Some(actual) = oracle_eval(statement) else {
        return;
    };
    assert_eq!(
        actual,
        "{{'ETS-TRANSFER',ga_n,true,{complex,[1,2,3]},4},\
         {'ETS-TRANSFER',true,true,42}}",
        "the live oracle disagrees with the pinned give_away/3 goldens"
    );
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
