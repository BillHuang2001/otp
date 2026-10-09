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

//! `ets:rename/2` — the table's name write, cross-checked against the live oracle.
//!
//! The crate owns the **checked name write** ([`Table::rename`]); the name → table
//! registry and the term-level rejections live in the host (`beam_bifs`'s `ets:*`
//! bridge). These tests drive the *whole host path* through the same seam the
//! runtime uses ([`rename_as`]: write gate → crate primitive → registry rekey) and
//! compare it, fact by fact, with the system `erl`.
//!
//! Every oracle fact this file asserts is printed by `ORACLE_PROGRAM` as a
//! `key -> value` line; the assertions name the same key, so a divergence points at
//! the exact rule. Facts the *crate* cannot express (a non-atom name, an unknown or
//! deleted table) are pinned from the oracle alone, with the owner named in a
//! comment — see [`oracle_rejects_non_atom_names_and_unknown_tables`].

mod common;

use std::collections::HashMap;
use std::time::Duration;

use beam_common::{BeamError, Result};
use beam_ets::{can_write, Access, OwnerId, RenameOutcome, Table, TableId, TableKind, TableOptions};
use beam_term::prelude::*;

use common::oracle;

/// The calling process that owns the tables under test.
fn owner() -> OwnerId {
    OwnerId::new(1)
}

/// A different, live process (the non-owner in the gate tests).
fn other_process() -> OwnerId {
    OwnerId::new(2)
}

/// The name → table-id registry the host owns.
///
/// Two properties mirror the oracle exactly and are load-bearing for the rules
/// under test:
///
/// * only tables created with `named_table` are registered — a name merely
///   *recorded* on an unnamed table never blocks anything, and two tables may
///   report the same `name`;
/// * a name resolves to **one** table, so `ets:whereis/1`-style lookup is a plain
///   map read.
#[derive(Debug, Default)]
struct Registry {
    names: HashMap<Atom, TableId>,
}

impl Registry {
    fn register(&mut self, name: Atom, id: TableId) {
        self.names.insert(name, id);
    }

    /// The oracle's registry hit test (`insert_named_tab`): is *any* named table
    /// registered under `name`?
    fn is_registered(&self, name: Atom) -> bool {
        self.names.contains_key(&name)
    }

    /// The name lookup of every `ets:*` call that addresses a table by name.
    fn resolve(&self, name: Atom) -> Option<TableId> {
        self.names.get(&name).copied()
    }

    /// The same lookup, shaped the way the host raises it: an unregistered name is
    /// the oracle's `badarg` (the `undefined` an `ets:info/2` renders for it is the
    /// *caller's* rendering, not the registry's).
    fn lookup_by_name(&self, name: Atom) -> Result<TableId> {
        self.resolve(name).ok_or(BeamError::BadArg)
    }

    /// The rekey a successful *named* rename owes the registry.
    fn rekey(&mut self, previous: Option<Atom>, name: Atom, id: TableId) {
        if let Some(previous) = previous {
            self.names.remove(&previous);
        }
        self.names.insert(name, id);
    }
}

/// The complete host path for `ets:rename/2`: the write gate, then the crate's
/// checked name write, then the registry rekey the outcome obliges.
///
/// This is what `beam_bifs`'s `ets:rename/2` bridge must do; driving it here means
/// the crate's rules and the oracle's are compared through the seam the runtime
/// actually uses, not through a test-only shortcut.
fn rename_as(
    table: &mut Table,
    registry: &mut Registry,
    name: Atom,
    requester: Option<OwnerId>,
) -> Result<RenameOutcome> {
    if !can_write(table.access(), table.owner(), requester) {
        return Err(BeamError::BadArg);
    }
    let outcome = table.rename(name, |candidate| registry.is_registered(candidate))?;
    if let RenameOutcome::Named { previous_name } = outcome {
        registry.rekey(previous_name, name, table.id());
    }
    Ok(outcome)
}

/// A table with a recorded `name`, the `named_table` flag and an access mode,
/// owned by [`owner`].
fn table(atoms: &mut AtomTable, name: &str, named: bool, access: Access) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::Set)
        .set_access(access)
        .set_named(named)
        .set_name(atoms.intern(name));
    let mut table = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    table.set_owner(Some(owner()));
    table
}

// ---------------------------------------------------------------------------
// The oracle battery
// ---------------------------------------------------------------------------

/// Wall-clock bound for the oracle program.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// Every probed `ets:rename/2` fact, printed as `key -> value`.
///
/// The program is the probe this module's rules were derived from: it renames
/// named and unnamed tables, checks identity/data/name/info before and after, the
/// two `badarg` rejections of a named table, the write gate across the three
/// protections, the non-atom-name and unknown-table rejections, and a rename after
/// `ets:delete/1`.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    C = fun(E) -> try {ok, E()} catch error:badarg -> badarg; Class:Reason -> {Class, Reason} end end,
    R = fun({ok, V}) -> V; (X) -> X end,

    %% ---- (1) a named table: identity, data and name across a rename ----
    Named = ets:new(a1, [named_table, public]),
    ets:insert(Named, {1, x}),
    TidBefore = ets:info(Named, id),
    P("named.rename", R(C(fun() -> ets:rename(Named, b1) end))),
    P("named.name_after", R(C(fun() -> ets:info(b1, name) end))),
    P("named.named_after", R(C(fun() -> ets:info(b1, named_table) end))),
    P("named.size_after", R(C(fun() -> ets:info(b1, size) end))),
    P("named.lookup_after", R(C(fun() -> ets:lookup(b1, 1) end))),
    P("named.tid_same", R(C(fun() -> ets:info(b1, id) =:= TidBefore end))),
    P("named.tid_still_usable", R(C(fun() -> ets:lookup(TidBefore, 1) =:= ets:lookup(b1, 1) end))),
    P("named.old_name_info", R(C(fun() -> ets:info(a1, name) end))),
    P("named.old_name_lookup", R(C(fun() -> ets:lookup(a1, 1) end))),
    P("named.old_name_whereis", R(C(fun() -> ets:whereis(a1) end))),
    P("named.new_name_whereis_tid_same", R(C(fun() -> ets:whereis(b1) =:= TidBefore end))),
    P("named.owner_same", R(C(fun() -> ets:info(b1, owner) =:= self() end))),
    P("named.protection_after", R(C(fun() -> ets:info(b1, protection) end))),
    P("named.old_name_free_for_new_table", is_atom(ets:new(a1, [named_table]))),

    %% ---- (2) a named table renamed to its own current name ----
    _Self = ets:new(e1, [named_table]),
    P("selfname.rename_by_name", R(C(fun() -> ets:rename(e1, e1) end))),
    P("selfname.rename_by_tid", R(C(fun() -> ets:rename(ets:info(e1, id), e1) end))),
    P("selfname.name_after", R(C(fun() -> ets:info(e1, name) end))),

    %% ---- (3) a named table renamed to a taken name ----
    _Taken = ets:new(d1, [named_table]),
    P("taken.rename", R(C(fun() -> ets:rename(d1, b1) end))),
    P("taken.self_name_after", R(C(fun() -> ets:info(d1, name) end))),
    P("taken.victim_size", R(C(fun() -> ets:info(b1, size) end))),

    %% ---- (4) a table WITHOUT named_table can be renamed ----
    Anon = ets:new(c1, []),
    P("anon.name_before", R(C(fun() -> ets:info(Anon, name) end))),
    P("anon.named_before", R(C(fun() -> ets:info(Anon, named_table) end))),
    Renamed = C(fun() -> ets:rename(Anon, c2) end),
    P("anon.rename_is_ok", case Renamed of {ok, _} -> true; _ -> false end),
    P("anon.rename_returns_tid", case Renamed of {ok, RV} -> not is_atom(RV); _ -> badarg end),
    P("anon.name_after", R(C(fun() -> ets:info(Anon, name) end))),
    P("anon.named_after", R(C(fun() -> ets:info(Anon, named_table) end))),
    P("anon.whereis_new_name", R(C(fun() -> ets:whereis(c2) end))),

    %% ---- (5) an unnamed table renamed to a REGISTERED name (no conflict check) ----
    P("anon2.rename_to_registered", case C(fun() -> ets:rename(Anon, b1) end) of {ok, RV2} -> not is_atom(RV2); _ -> badarg end),
    P("anon2.name_after", R(C(fun() -> ets:info(Anon, name) end))),
    P("anon2.named_table_intact", R(C(fun() -> ets:info(b1, size) end))),
    P("anon2.whereis_still_the_named_one", R(C(fun() -> ets:whereis(b1) =:= TidBefore end))),

    %% ---- (6) a named table taking a name merely RECORDED on an unnamed table ----
    U2 = ets:new(u2, []),
    _N2 = ets:new(n2, [named_table]),
    TidN2 = ets:info(n2, id),
    P("named2.rename", R(C(fun() -> ets:rename(n2, u2) end))),
    P("named2.name_after", R(C(fun() -> ets:info(u2, name) end))),
    P("named2.both_reports_the_same_name", R(C(fun() -> ets:info(u2, name) =:= ets:info(U2, name) end))),
    P("named2.whereis_is_the_named_one", R(C(fun() -> ets:whereis(u2) =:= TidN2 end))),
    P("named2.old_name_resolves", R(C(fun() -> ets:whereis(n2) end))),
    P("named2.unnamed_named_flag", R(C(fun() -> ets:info(U2, named_table) end))),
    P("named2.unnamed_still_there", R(C(fun() -> ets:info(U2, size) end))),

    %% ---- (7) non-atom names / unknown tables (BIF/registry-layer rejections) ----
    P("badname.string", R(C(fun() -> ets:rename(e1, "str") end))),
    P("badname.int", R(C(fun() -> ets:rename(e1, 42) end))),
    P("badname.list", R(C(fun() -> ets:rename(e1, []) end))),
    P("unknown.atom", R(C(fun() -> ets:rename(no_such_table, x) end))),
    P("unknown.ref", R(C(fun() -> ets:rename(make_ref(), x) end))),

    %% ---- (8) the write gate ----
    Parent = self(),
    Gate = fun(Prot, Owner) ->
        Name = list_to_atom(lists:flatten(io_lib:format("gate_~p_~p", [Prot, Owner]))),
        New = list_to_atom(lists:flatten(io_lib:format("gatenew_~p_~p", [Prot, Owner]))),
        _ = ets:new(Name, [named_table, Prot]),
        F = fun() -> C(fun() -> ets:rename(Name, New) end) end,
        case Owner of
            owner -> R(F());
            nonowner ->
                Ref = make_ref(),
                spawn(fun() -> Parent ! {Ref, F()} end),
                receive {Ref, GV} -> R(GV) end
        end
    end,
    [P(lists:flatten(io_lib:format("gate.~p.~p", [Pp, Oo])), Gate(Pp, Oo))
     || Pp <- [public, protected, private], Oo <- [owner, nonowner]],

    %% ---- (9) after delete ----
    _Dead = ets:new(dead1, [named_table]),
    DeadTid = ets:info(dead1, id),
    ets:delete(dead1),
    P("dead.rename_by_name", R(C(fun() -> ets:rename(dead1, x) end))),
    P("dead.rename_by_tid", R(C(fun() -> ets:rename(DeadTid, x) end))),
    ok.
"#;

/// Run the oracle battery once and return its `key -> value` facts.
///
/// Returns `None` (after a **loud** skip) only under the explicit oracle opt-out
/// (`BEAM_ETS_ALLOW_NO_ORACLE=1`); a genuinely absent Erlang otherwise **fails** the
/// calling test, since the oracle is required by default.
///
/// The scratch directory is keyed by the *test* name, not just the process id, so
/// two tests of this binary can run the battery concurrently without one removing
/// the other's scratch directory.
fn oracle_facts(test: &str) -> Option<HashMap<String, String>> {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            test,
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return None;
    };
    let output = oracle::run_escript(&escript, ORACLE_PROGRAM, &[], test, ORACLE_TIMEOUT);
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut facts = HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(" -> ") {
            facts.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    // The battery prints one line per fact; a short read means the program died
    // half way through and every missing key would otherwise be reported as a
    // single confusing assertion failure.
    assert!(
        facts.len() >= 45,
        "the oracle program printed only {} facts (expected the whole battery):\n{text}",
        facts.len()
    );
    Some(facts)
}

/// One fact of the battery (panics — loudly — on a missing key).
fn fact<'a>(facts: &'a HashMap<String, String>, key: &str) -> &'a str {
    facts
        .get(key)
        .unwrap_or_else(|| panic!("the oracle printed no `{key} -> ...` line"))
        .as_str()
}

/// The oracle's own rendering of a **table name** / of `true`/`false`.
fn is_false(value: &str) -> bool {
    value == "false"
}

/// The atom a name string is interned as, rendered back (asserts the interning
/// round-trip used to tie an oracle `key -> b1` line to an [`Atom`]).
fn name_of(atoms: &AtomTable, atom: Option<Atom>) -> Option<&str> {
    atoms.name(atom?)
}

// ---------------------------------------------------------------------------
// The crate's own rules (no oracle needed)
// ---------------------------------------------------------------------------

/// A rename changes the name and **nothing else**: objects, identifier, owner,
/// access mode, kind, `keypos`, store words and iteration order are untouched.
#[test]
fn renaming_a_named_table_keeps_identity_objects_and_owner() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let old = atoms.intern("a1");
    let new = atoms.intern("b1");
    let mut table = table(&mut atoms, "a1", true, Access::Public);
    let one = common::object(&mut heap, 1, 7);
    let two = common::object(&mut heap, 2, 8);
    table.insert(one, &atoms).unwrap();
    table.insert(two, &atoms).unwrap();

    let id = table.id();
    let memory = table.memory();
    let words_before = table.tab2list();
    assert_eq!(table.name(), Some(old));

    let outcome = table
        .rename(new, |_| false)
        .expect("a free name on a named table");
    assert_eq!(
        outcome,
        RenameOutcome::Named {
            previous_name: Some(old)
        }
    );

    assert_eq!(table.name(), Some(new), "the new name is reported");
    assert_eq!(table.info().name, Some(new));
    assert!(table.info().named_table);
    assert_eq!(table.id(), id, "the table identifier survives");
    assert_eq!(table.owner(), Some(owner()));
    assert_eq!(table.access(), Access::Public);
    assert_eq!(table.keypos(), 1);
    assert_eq!(table.kind(), TableKind::Set);
    assert_eq!(table.memory(), memory, "the term store is untouched");
    assert_eq!(table.tab2list(), words_before, "the objects survive as they were");
    assert_eq!(table.size(), 2);
    // The store hands back *copies*, so compare structurally.
    let looked = table.lookup(make_small(1), &atoms);
    assert_eq!(looked.len(), 1);
    assert!(eq(looked[0], one, &atoms));
    assert!(eq(table.lookup(make_small(2), &atoms)[0], two, &atoms));
}

/// A **named** table cannot take a name that is already registered — neither
/// another table's nor its own — and a rejected rename leaves everything as it was.
#[test]
fn a_named_table_rejects_a_taken_name_and_its_own_name() {
    let mut atoms = AtomTable::new();
    let victim_name = atoms.intern("victim");
    let other_name = atoms.intern("other");
    let victim = table(&mut atoms, "victim", true, Access::Protected);
    let mut other = table(&mut atoms, "other", true, Access::Protected);
    let mut registry = Registry::default();
    registry.register(victim_name, victim.id());
    registry.register(other_name, other.id());

    // (a) another table's name.
    assert!(matches!(
        rename_as(&mut other, &mut registry, victim_name, Some(owner())),
        Err(BeamError::BadArg)
    ));
    assert_eq!(other.name(), Some(other_name), "the name is unchanged");
    assert_eq!(
        registry.resolve(victim_name),
        Some(victim.id()),
        "the registry is untouched by a rejected rename"
    );

    // (b) its own name — with a *correct* registry predicate ...
    assert!(matches!(
        rename_as(&mut other, &mut registry, other_name, Some(owner())),
        Err(BeamError::BadArg)
    ));
    // ... and with a naive one that reports every name free: the rule is the
    // crate's (the oracle's `insert_named_tab` finds the entry the table owns).
    assert!(matches!(
        other.rename(other_name, |_| false),
        Err(BeamError::BadArg)
    ));
    assert_eq!(other.name(), Some(other_name));
    assert_eq!(registry.resolve(other_name), Some(other.id()));
}

/// A rename of a table **without** `named_table` has no conflict rule at all, and
/// never consults the registry.
#[test]
fn an_unnamed_table_is_renamed_without_any_conflict_check() {
    let mut atoms = AtomTable::new();
    let taken = atoms.intern("taken");
    let named = table(&mut atoms, "taken", true, Access::Protected);
    let mut anon = table(&mut atoms, "c1", false, Access::Protected);
    let mut registry = Registry::default();
    registry.register(taken, named.id());

    // Even a *registered* name is taken freely (the table is not in the registry).
    let outcome = anon
        .rename(taken, |_| {
            panic!("an unnamed table's rename must not consult the name registry")
        })
        .expect("an unnamed table is always renamable");
    assert_eq!(outcome, RenameOutcome::Unnamed);
    assert_eq!(anon.name(), Some(taken), "the name is overwritten");
    assert!(!anon.info().named_table, "the table is still not a named table");
    assert_eq!(
        registry.resolve(taken),
        Some(named.id()),
        "the registry is untouched"
    );

    // ... and renaming it to the name it now has is fine too.
    assert_eq!(
        anon.rename(taken, |_| panic!("no registry lookup")).unwrap(),
        RenameOutcome::Unnamed
    );
    assert_eq!(anon.name(), Some(taken));
}

/// A **named** table may take a name that is merely *recorded* on an unnamed table:
/// only named tables are in the registry, so nothing conflicts. Both tables then
/// report the same `name`; only the named one is resolvable.
#[test]
fn a_named_table_may_take_a_name_recorded_on_an_unnamed_table() {
    let mut atoms = AtomTable::new();
    let shared = atoms.intern("u2");
    let old = atoms.intern("n2");
    let unnamed = table(&mut atoms, "u2", false, Access::Protected);
    let mut named = table(&mut atoms, "n2", true, Access::Protected);
    let mut registry = Registry::default();
    registry.register(old, named.id());
    assert!(!registry.is_registered(shared), "not registered: unnamed tables are not");

    assert_eq!(
        rename_as(&mut named, &mut registry, shared, Some(owner())).unwrap(),
        RenameOutcome::Named {
            previous_name: Some(old)
        }
    );
    assert_eq!(named.name(), Some(shared));
    assert_eq!(unnamed.name(), Some(shared), "both report the same name");
    assert_eq!(
        registry.resolve(shared),
        Some(named.id()),
        "the name resolves to the named table"
    );
    assert!(
        matches!(registry.lookup_by_name(old), Err(BeamError::BadArg)),
        "the named table's old name is gone"
    );
    assert!(!unnamed.info().named_table);
}

// ---------------------------------------------------------------------------
// The live oracle
// ---------------------------------------------------------------------------

/// A rename of a `named_table`: same identifier, same objects, same owner/access,
/// the new name, and the old name gone from the registry.
#[test]
fn oracle_agrees_on_a_named_rename() {
    let Some(facts) = oracle_facts("oracle_agrees_on_a_named_rename") else {
        return;
    };
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let old = atoms.intern("a1");
    let new = atoms.intern("b1");
    let mut table = table(&mut atoms, "a1", true, Access::Public);
    let one = common::object(&mut heap, 1, 7);
    table.insert(one, &atoms).unwrap();
    let id = table.id();
    let mut registry = Registry::default();
    registry.register(old, id);

    assert_eq!(fact(&facts, "named.rename"), "b1", "returns the new NAME");
    assert_eq!(
        rename_as(&mut table, &mut registry, new, Some(owner())).unwrap(),
        RenameOutcome::Named {
            previous_name: Some(old)
        }
    );
    assert_eq!(name_of(&atoms, table.name()), Some(fact(&facts, "named.name_after")));
    assert_eq!(!is_false(fact(&facts, "named.named_after")), table.info().named_table);
    assert_eq!(fact(&facts, "named.size_after").parse::<usize>().unwrap(), table.size());
    assert_eq!(fact(&facts, "named.lookup_after"), "[{1,x}]");
    let looked = table.lookup(make_small(1), &atoms);
    assert_eq!(looked.len(), 1);
    assert!(eq(looked[0], one, &atoms), "the object survives the rename");
    assert!(!is_false(fact(&facts, "named.tid_same")), "the identifier survives");
    assert_eq!(table.id(), id);
    assert!(
        !is_false(fact(&facts, "named.tid_still_usable")),
        "the pre-rename identifier still addresses the table"
    );
    // The old name is gone: `ets:info`/`ets:whereis` answer `undefined` and any
    // other use of it is the registry layer's `badarg`.
    assert_eq!(fact(&facts, "named.old_name_info"), "undefined");
    assert_eq!(fact(&facts, "named.old_name_whereis"), "undefined");
    assert_eq!(fact(&facts, "named.old_name_lookup"), "badarg");
    assert!(matches!(registry.lookup_by_name(old), Err(BeamError::BadArg)));
    assert!(
        !is_false(fact(&facts, "named.new_name_whereis_tid_same")),
        "the new name resolves to the SAME identifier"
    );
    assert_eq!(registry.resolve(new), Some(id));
    assert_eq!(!is_false(fact(&facts, "named.owner_same")), table.owner() == Some(owner()));
    assert_eq!(fact(&facts, "named.protection_after"), "public");
    assert_eq!(table.access(), Access::Public);
    assert!(
        !is_false(fact(&facts, "named.old_name_free_for_new_table")),
        "the freed name is registerable again"
    );
    assert!(!registry.is_registered(old));
}

/// The two `badarg` rejections of a **named** table: a name already registered by
/// another table, and its own current name.
#[test]
fn oracle_agrees_on_the_named_table_rejections() {
    let Some(facts) = oracle_facts("oracle_agrees_on_the_named_table_rejections") else {
        return;
    };
    let mut atoms = AtomTable::new();
    let self_name = atoms.intern("e1");
    let taken = atoms.intern("b1");
    let own_name = atoms.intern("d1");

    // `ets:rename(e1, e1)` — by name and by identifier, both `badarg`.
    assert_eq!(fact(&facts, "selfname.rename_by_name"), "badarg");
    assert_eq!(fact(&facts, "selfname.rename_by_tid"), "badarg");
    let mut same = table(&mut atoms, "e1", true, Access::Protected);
    let mut registry = Registry::default();
    registry.register(self_name, same.id());
    assert!(matches!(
        rename_as(&mut same, &mut registry, self_name, Some(owner())),
        Err(BeamError::BadArg)
    ));
    assert_eq!(name_of(&atoms, same.name()), Some(fact(&facts, "selfname.name_after")));

    // `ets:rename(d1, b1)` where `b1` is another named table.
    assert_eq!(fact(&facts, "taken.rename"), "badarg");
    let mut heap = Heap::new(16);
    let mut victim = table(&mut atoms, "b1", true, Access::Public);
    victim.insert(common::object(&mut heap, 1, 7), &atoms).unwrap();
    let mut taken_table = table(&mut atoms, "d1", true, Access::Protected);
    let mut registry = Registry::default();
    registry.register(taken, victim.id());
    registry.register(own_name, taken_table.id());
    assert!(matches!(
        rename_as(&mut taken_table, &mut registry, taken, Some(owner())),
        Err(BeamError::BadArg)
    ));
    assert_eq!(
        name_of(&atoms, taken_table.name()),
        Some(fact(&facts, "taken.self_name_after"))
    );
    assert_eq!(
        fact(&facts, "taken.victim_size").parse::<usize>().unwrap(),
        victim.size()
    );
    assert_eq!(registry.resolve(taken), Some(victim.id()));
}

/// A table **without** `named_table` is renamable at will: the name changes, the
/// table is still not a named table, the registry is untouched, and the oracle
/// returns the unchanged identifier rather than the name.
#[test]
fn oracle_agrees_on_a_rename_without_named_table() {
    let Some(facts) = oracle_facts("oracle_agrees_on_a_rename_without_named_table") else {
        return;
    };
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(16);
    let c2 = atoms.intern("c2");
    let b1 = atoms.intern("b1");
    let mut named = table(&mut atoms, "b1", true, Access::Public);
    named.insert(common::object(&mut heap, 1, 7), &atoms).unwrap();
    let mut anon = table(&mut atoms, "c1", false, Access::Protected);
    let mut registry = Registry::default();
    registry.register(b1, named.id());

    assert_eq!(fact(&facts, "anon.name_before"), "c1");
    assert_eq!(name_of(&atoms, anon.name()), Some(fact(&facts, "anon.name_before")));
    assert_eq!(!is_false(fact(&facts, "anon.named_before")), anon.info().named_table);

    assert!(!is_false(fact(&facts, "anon.rename_is_ok")));
    assert!(
        !is_false(fact(&facts, "anon.rename_returns_tid")),
        "the oracle returns the identifier, not the name atom"
    );
    assert_eq!(
        rename_as(&mut anon, &mut registry, c2, Some(owner())).unwrap(),
        RenameOutcome::Unnamed
    );
    assert_eq!(name_of(&atoms, anon.name()), Some(fact(&facts, "anon.name_after")));
    assert_eq!(!is_false(fact(&facts, "anon.named_after")), anon.info().named_table);
    assert!(!anon.info().named_table, "still not a named table");
    assert_eq!(fact(&facts, "anon.whereis_new_name"), "undefined");
    assert!(matches!(registry.lookup_by_name(c2), Err(BeamError::BadArg)));

    // Renaming it to a name another (named) table holds is fine, and leaves that
    // table's registration intact.
    assert!(!is_false(fact(&facts, "anon2.rename_to_registered")));
    assert_eq!(
        rename_as(&mut anon, &mut registry, b1, Some(owner())).unwrap(),
        RenameOutcome::Unnamed
    );
    assert_eq!(name_of(&atoms, anon.name()), Some(fact(&facts, "anon2.name_after")));
    assert_eq!(
        fact(&facts, "anon2.named_table_intact").parse::<usize>().unwrap(),
        named.size()
    );
    assert!(!is_false(fact(&facts, "anon2.whereis_still_the_named_one")));
    assert_eq!(registry.resolve(b1), Some(named.id()), "the named table keeps the name");
}

/// The same two rules from the *named* side: a named table may take a name merely
/// recorded on an unnamed table, and both then report that name while only the
/// named one is resolvable by it.
#[test]
fn oracle_agrees_on_a_named_table_taking_a_recorded_name() {
    let Some(facts) = oracle_facts("oracle_agrees_on_a_named_table_taking_a_recorded_name") else {
        return;
    };
    let mut atoms = AtomTable::new();
    let u2 = atoms.intern("u2");
    let n2 = atoms.intern("n2");
    let unnamed = table(&mut atoms, "u2", false, Access::Protected);
    let mut named = table(&mut atoms, "n2", true, Access::Protected);
    let mut registry = Registry::default();
    registry.register(n2, named.id());

    assert_eq!(fact(&facts, "named2.rename"), "u2", "a free (unregistered) name");
    assert_eq!(
        rename_as(&mut named, &mut registry, u2, Some(owner())).unwrap(),
        RenameOutcome::Named {
            previous_name: Some(n2)
        }
    );
    assert_eq!(name_of(&atoms, named.name()), Some(fact(&facts, "named2.name_after")));
    assert!(
        !is_false(fact(&facts, "named2.both_reports_the_same_name")),
        "the unnamed table still reports the same name"
    );
    assert_eq!(named.name(), unnamed.name());
    assert!(!is_false(fact(&facts, "named2.whereis_is_the_named_one")));
    assert_eq!(registry.resolve(u2), Some(named.id()));
    assert_eq!(fact(&facts, "named2.old_name_resolves"), "undefined");
    assert!(matches!(registry.lookup_by_name(n2), Err(BeamError::BadArg)));
    assert_eq!(!is_false(fact(&facts, "named2.unnamed_named_flag")), unnamed.info().named_table);
    assert!(!unnamed.info().named_table);
    assert_eq!(
        fact(&facts, "named2.unnamed_still_there").parse::<usize>().unwrap(),
        unnamed.size()
    );
}

/// `ets:rename/2` is **write**-gated, not owner-gated: a non-owner may rename a
/// `public` table, but not a `protected`/`private` one.
#[test]
fn oracle_agrees_that_a_rename_is_write_gated() {
    let Some(facts) = oracle_facts("oracle_agrees_that_a_rename_is_write_gated") else {
        return;
    };
    let mut atoms = AtomTable::new();
    for (protection, access) in [
        ("public", Access::Public),
        ("protected", Access::Protected),
        ("private", Access::Private),
    ] {
        for (who, requester) in [
            ("owner", Some(owner())),
            ("nonowner", Some(other_process())),
        ] {
            let key = format!("gate.{protection}.{who}");
            let success = fact(&facts, &key) != "badarg";
            assert_eq!(
                success,
                can_write(access, Some(owner()), requester),
                "{key}: the oracle and the write gate must agree (oracle said {})",
                fact(&facts, &key)
            );
            let name = atoms.intern(&format!("g_{protection}_{who}"));
            let new_name = atoms.intern(&format!("ng_{protection}_{who}"));
            let mut table = table(&mut atoms, &format!("g_{protection}_{who}"), true, access);
            let mut registry = Registry::default();
            registry.register(name, table.id());

            assert_eq!(
                rename_as(&mut table, &mut registry, new_name, requester).is_ok(),
                success,
                "{key}: the host path must match the oracle"
            );
            if success {
                assert_eq!(table.name(), Some(new_name));
                assert!(!registry.is_registered(name), "rekeyed on success");
                assert_eq!(registry.resolve(new_name), Some(table.id()));
            } else {
                assert_eq!(table.name(), Some(name), "nothing changes on a denied rename");
                assert_eq!(registry.resolve(name), Some(table.id()));
            }
        }
    }
}

/// The two rejections the *crate* cannot express, pinned from the oracle for the
/// layers that own them: a new name that is not an atom is the BIF's check (the
/// crate's primitive takes an interned [`Atom`]), and an unknown/deleted table is
/// the registry's lookup (the crate's primitive takes a resolved [`Table`]).
#[test]
fn oracle_rejects_non_atom_names_and_unknown_tables() {
    let Some(facts) = oracle_facts("oracle_rejects_non_atom_names_and_unknown_tables") else {
        return;
    };
    // `beam_bifs`: the BIF must reject a non-atom `Name` *before* the name write.
    for key in ["badname.string", "badname.int", "badname.list"] {
        assert_eq!(fact(&facts, key), "badarg", "{key}: a non-atom name is badarg");
    }
    // The registry/`beam_bifs`: an unresolved name or identifier is `badarg`, and a
    // deleted table is unresolvable by either route.
    for key in ["unknown.atom", "unknown.ref", "dead.rename_by_name", "dead.rename_by_tid"] {
        assert_eq!(fact(&facts, key), "badarg", "{key}: an unknown table is badarg");
    }
    // The crate's own stand-in for both, so the *shape* of the failure is pinned
    // too: the registry layer raises the same typed `badarg`.
    let mut atoms = AtomTable::new();
    let unknown = atoms.intern("no_such_table");
    let registry = Registry::default();
    assert!(matches!(registry.lookup_by_name(unknown), Err(BeamError::BadArg)));
}
