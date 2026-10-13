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

//! Access enforcement: the protection matrix and a public table.
//!
//! The matrix itself is verified against the system `erl` (OTP 29 / erts-17.1)
//! for **both** the owner and a non-owner: the owner is admitted to everything in
//! every mode, while a non-owner gets a `protected` table's *reads* but none of its
//! *writes*, a `private` table's neither, and a `public` table's both. The
//! `ets:info/1,2` introspection — and the `size`/`is_empty` projection of it — is
//! **never** access-gated, and `ets:setopts/2 {protection,_}` / `ets:give_away/3`
//! are **owner**-gated rather than protection-gated. The `TableAccess` view is
//! exercised here for the whole observable `ets` surface.

mod common;

use std::collections::HashMap;
use std::time::Duration;

use beam_common::BeamError;
use beam_ets::{
    compile, Access, Heir, MatchSpec, OwnerId, RenameOutcome, SetOpt, Table, TableId, TableKind,
    TableOptions,
};
use beam_term::prelude::*;
use common::*;

/// The wall-clock bound for every live-oracle probe (a wedged `erl` fails the test
/// instead of hanging the suite).
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// One `key -> value` line of an oracle probe (panics — loudly — on a missing key,
/// so a half-dead oracle cannot masquerade as a passing assertion).
fn oracle_fact<'a>(facts: &'a HashMap<String, String>, key: &str) -> &'a str {
    facts
        .get(key)
        .unwrap_or_else(|| panic!("the oracle printed no `{key} -> ...` line"))
        .as_str()
}

/// Assert that a gated operation either succeeded (when `allowed`) or failed with
/// exactly `BeamError::BadArg` (when `!allowed`) — never a wrong result.
fn assert_gate<T>(result: beam_common::Result<T>, allowed: bool, msg: &str) {
    match result {
        Ok(_) => assert!(allowed, "{msg}: expected `badarg` but the op succeeded"),
        Err(BeamError::BadArg) => assert!(!allowed, "{msg}: expected success but got `badarg`"),
        Err(other) => panic!("{msg}: expected success/`badarg`, got {other:?}"),
    }
}

/// The access matrix as real ETS applies it: whether `requester` may read/write a
/// table with `access` owned by `owner`.
fn expected(access: Access, owner: OwnerId, requester: Option<OwnerId>) -> (bool, bool) {
    let is_owner = requester == Some(owner);
    let read = match access {
        Access::Public | Access::Protected => true,
        Access::Private => is_owner,
    };
    let write = match access {
        Access::Public => true,
        Access::Protected | Access::Private => is_owner,
    };
    (read, write)
}

/// A table of `access` owned by `owner`, seeded by the owner with six objects
/// `{K, K*100}` for `K ∈ {0, 10, 20, 30, 40, 50}`.
fn seeded_table(access: Access, owner: OwnerId, heap: &mut Heap, atoms: &AtomTable) -> Table {
    let mut opts = TableOptions::new();
    opts.set_access(access);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.set_owner(Some(owner));
    // The owner may always write, whatever the mode.
    {
        let mut as_owner = t.as_requester(Some(owner));
        for k in [0i64, 10, 20, 30, 40, 50] {
            as_owner.insert(object(heap, k, k * 100), atoms).unwrap();
        }
    }
    t
}

/// A compiled match-spec selecting every 2-tuple object with body `[true]`
/// (the shape `ets:select_delete/2` needs to delete the matches).
fn every_object_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let underscore = interned_atom(atoms, "_");
    let head = heap.tuple(&[underscore, underscore]).expect("test heap has room");
    let body = proper_list(heap, &[interned_atom(atoms, "true")]);
    let clause = heap.tuple(&[head, NIL, body]).expect("test heap has room");
    let spec = proper_list(heap, &[clause]);
    compile(spec, atoms).expect("a valid match spec")
}

/// A compiled match-spec selecting every 2-tuple object with body `['$_']`
/// (the whole object), the shape that shows the traversal *order* of a result.
fn whole_object_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let underscore = interned_atom(atoms, "_");
    let whole = interned_atom(atoms, "$_");
    let head = heap.tuple(&[underscore, underscore]).expect("test heap has room");
    let body = proper_list(heap, &[whole]);
    let clause = heap.tuple(&[head, NIL, body]).expect("test heap has room");
    let spec = proper_list(heap, &[clause]);
    compile(spec, atoms).expect("a valid match spec")
}

/// The `{'_', '_'}` pattern term matching every 2-tuple object.
fn wildcard_pattern(heap: &mut Heap, atoms: &mut AtomTable) -> Eterm {
    heap.tuple(&[interned_atom(atoms, "_"), interned_atom(atoms, "_")])
        .expect("test heap has room")
}

#[test]
fn access_enforcement_follows_the_protection_matrix() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);

    // (protection, other may write, other may read)
    let matrix = [
        (Access::Public, true, true),
        (Access::Protected, false, true),
        (Access::Private, false, false),
    ];

    for (access, other_write, other_read) in matrix {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut opts = TableOptions::new();
        opts.set_access(access);
        let mut t = Table::new(TableId::next(), opts).unwrap();
        t.set_owner(Some(owner));

        // The owner is always allowed; the non-owner follows the mode.
        t.check_read(Some(owner)).unwrap();
        t.check_write(Some(owner)).unwrap();
        assert_eq!(
            t.check_read(Some(other)).is_ok(),
            other_read,
            "{access:?} read by a non-owner"
        );
        assert_eq!(
            t.check_write(Some(other)).is_ok(),
            other_write,
            "{access:?} write by a non-owner"
        );
        // A requester that is not the owner is denied exactly as the mode says.
        assert_eq!(t.check_write(None).is_ok(), access == Access::Public);

        // The owner seeds the table: the owner may always write, whatever the mode.
        {
            let mut as_owner = t.as_requester(Some(owner));
            as_owner.insert(object(&mut heap, 1, 10), &atoms).unwrap();
        }

        // The enforcing view rechecks every operation (a violation is `badarg`).
        {
            let mut view = t.as_requester(Some(other));
            assert_eq!(view.requester(), Some(other));

            // Reads follow the mode.
            if other_read {
                assert_eq!(view.lookup(make_small(1), &atoms).unwrap().len(), 1);
                assert!(view.member(make_small(1), &atoms).unwrap());
                assert_eq!(
                    view.lookup_element(make_small(1), 1, &atoms).unwrap(),
                    make_small(1)
                );
            } else {
                assert!(matches!(
                    view.lookup(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.member(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.lookup_element(make_small(1), 1, &atoms),
                    Err(BeamError::BadArg)
                ));
            }

            // Writes follow the mode too.
            if other_write {
                view.insert(object(&mut heap, 2, 20), &atoms).unwrap();
                assert!(view.insert_new(object(&mut heap, 3, 30), &atoms).unwrap());
                assert_eq!(view.take(make_small(2), &atoms).unwrap().len(), 1);
                view.delete(make_small(3), &atoms).unwrap();
            } else {
                assert!(matches!(
                    view.insert(object(&mut heap, 2, 20), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.insert_new(object(&mut heap, 2, 20), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.delete(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.take(make_small(1), &atoms),
                    Err(BeamError::BadArg)
                ));
            }
        }

        // A denied write changed nothing: the owner's object is still there and the
        // table holds exactly it.
        assert_eq!(t.lookup(make_small(1), &atoms).len(), 1);
        assert_eq!(t.size(), 1, "{access:?} final size");
    }
}

#[test]
fn a_public_table_is_open_to_every_requester() {
    let atoms = AtomTable::new();
    let mut heap = Heap::new(64);
    let mut opts = TableOptions::new();
    opts.set_access(Access::Public);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.set_owner(Some(OwnerId::new(1)));

    // Even an ownerless requester may read and write a public table.
    let mut view = t.as_requester(None);
    view.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    assert_eq!(view.lookup(make_small(1), &atoms).unwrap().len(), 1);
    view.take(make_small(1), &atoms).unwrap();

    // A protected table still admits any reader, but only the owner may write.
    let mut opts = TableOptions::new();
    opts.set_access(Access::Protected);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.set_owner(Some(OwnerId::new(1)));
    {
        let mut owner_view = t.as_requester(Some(OwnerId::new(1)));
        owner_view.insert(object(&mut heap, 1, 10), &atoms).unwrap();
    }
    let mut reader = t.as_requester(Some(OwnerId::new(2)));
    assert_eq!(reader.lookup(make_small(1), &atoms).unwrap().len(), 1);
    assert!(matches!(
        reader.insert(object(&mut heap, 2, 20), &atoms),
        Err(BeamError::BadArg)
    ));
    assert!(!t.member(make_small(2), &atoms));
}

/// Every newly write-gated operation obeys the matrix for the three requesters
/// (owner / another process / none), across all three access modes.
#[test]
fn newly_wrapped_write_ops_follow_the_protection_matrix() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let mut atoms = AtomTable::new();
            let mut heap = Heap::new(4096);
            let mut t = seeded_table(access, owner, &mut heap, &atoms);
            let (_, allowed) = expected(access, owner, requester);
            let tag = format!("{access:?}/{who}");
            let spec = every_object_spec(&mut heap, &mut atoms);
            let wildcard = wildcard_pattern(&mut heap, &mut atoms);
            let seeded = t.size();

            let mut view = t.as_requester(requester);

            let list = [object(&mut heap, 1, 10), object(&mut heap, 2, 20)];
            assert_gate(
                view.insert_list(&list, &atoms),
                allowed,
                &format!("{tag} insert_list"),
            );
            assert_gate(
                view.insert_new(object(&mut heap, 3, 30), &atoms),
                allowed,
                &format!("{tag} insert_new"),
            );
            assert_gate(
                view.delete(make_small(0), &atoms),
                allowed,
                &format!("{tag} delete"),
            );
            assert_gate(
                view.delete_object(object(&mut heap, 10, 1000), &atoms),
                allowed,
                &format!("{tag} delete_object"),
            );
            let mut bh = rebuild_heap();
            assert_gate(
                view.update_counter(make_small(20), make_small(1), &atoms, &mut bh),
                allowed,
                &format!("{tag} update_counter"),
            );
            // The `/4` (`update_counter_with`) form obeys the same write gate. The
            // default must be a valid object tuple (arity ≥ keypos), even though
            // key 40 is present and the default is not used.
            let mut bh = rebuild_heap();
            let default_object = object(&mut heap, 40, 0);
            assert_gate(
                view.update_counter_with(
                    make_small(40),
                    make_small(1),
                    default_object,
                    &atoms,
                    &mut bh,
                ),
                allowed,
                &format!("{tag} update_counter_with"),
            );
            let mut bh = rebuild_heap();
            assert_gate(
                view.update_element(make_small(30), &[(2, make_small(999))], &atoms, &mut bh),
                allowed,
                &format!("{tag} update_element"),
            );
            let mut bh = rebuild_heap();
            assert_gate(
                view.select_delete(&spec, &atoms, &mut bh),
                allowed,
                &format!("{tag} select_delete"),
            );
            let mut bh = rebuild_heap();
            assert_gate(
                view.match_delete(wildcard, &atoms, &mut bh),
                allowed,
                &format!("{tag} match_delete"),
            );
            assert_gate(
                view.delete_all_objects(),
                allowed,
                &format!("{tag} delete_all_objects"),
            );

            drop(view);
            if allowed {
                // Every write went through (the seed objects were eventually
                // deleted); nothing is left.
                assert_eq!(t.size(), 0, "{tag}: writes should have applied");
            } else {
                // Not one denied write touched the table.
                assert_eq!(t.size(), seeded, "{tag}: a denied write mutated the table");
            }
        }
    }
}

/// Every newly read-gated operation obeys the matrix for the three requesters.
#[test]
fn newly_wrapped_read_ops_follow_the_protection_matrix() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let mut atoms = AtomTable::new();
            let mut heap = Heap::new(4096);
            let mut t = seeded_table(access, owner, &mut heap, &atoms);
            let (allowed, _) = expected(access, owner, requester);
            let tag = format!("{access:?}/{who}");
            let spec = every_object_spec(&mut heap, &mut atoms);
            let wildcard = wildcard_pattern(&mut heap, &mut atoms);

            let view = t.as_requester(requester);
            let mut bh = rebuild_heap();

            assert_gate(
                view.lookup(make_small(0), &atoms),
                allowed,
                &format!("{tag} lookup"),
            );
            assert_gate(
                view.member(make_small(0), &atoms),
                allowed,
                &format!("{tag} member"),
            );
            assert_gate(
                view.lookup_element(make_small(0), 1, &atoms),
                allowed,
                &format!("{tag} lookup_element"),
            );
            assert_gate(
                view.lookup_element_or(make_small(0), 1, make_small(0), &atoms),
                allowed,
                &format!("{tag} lookup_element_or"),
            );
            assert_gate(
                view.select(&spec, &atoms, &mut bh),
                allowed,
                &format!("{tag} select"),
            );
            assert_gate(
                view.select_count(&spec, &atoms, &mut bh),
                allowed,
                &format!("{tag} select_count"),
            );
            assert_gate(
                view.match_object(wildcard, &atoms, &mut bh),
                allowed,
                &format!("{tag} match_object"),
            );
            assert_gate(
                view.match_(wildcard, &atoms, &mut bh),
                allowed,
                &format!("{tag} match"),
            );
            assert_gate(view.first(&atoms), allowed, &format!("{tag} first"));
            assert_gate(view.last(&atoms), allowed, &format!("{tag} last"));
            assert_gate(
                view.next(make_small(0), &atoms),
                allowed,
                &format!("{tag} next"),
            );
            assert_gate(
                view.prev(make_small(0), &atoms),
                allowed,
                &format!("{tag} prev"),
            );
            assert_gate(view.tab2list(), allowed, &format!("{tag} tab2list"));
            // `size`/`is_empty` are the **ungated** `ets:info(T, size)`
            // projection — every requester may read them, a non-owner of a
            // `private` table included, exactly as the oracle resolves
            // `ets:info/1,2` without an access check.
            assert!(
                view.size().is_ok(),
                "{tag}: `ets:info(T, size)` is ungated for every requester"
            );
            assert!(
                view.is_empty().is_ok(),
                "{tag}: emptiness is ungated for every requester"
            );
        }
    }
}

/// `ets:safe_fixtable/2` is **read**-gated, not write-gated: the owner may always
/// fix the table, and so may any non-owner of a `protected` or `public` table —
/// but a non-owner of a `private` table is `badarg`. A denied call must leave the
/// table's fixation state untouched (verified against the system `erl`, OTP 29 /
/// erts-17.1).
#[test]
fn set_fixation_follows_the_read_gate() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(512);
            let mut t = seeded_table(access, owner, &mut heap, &atoms);
            let (read_ok, _) = expected(access, owner, requester);
            let tag = format!("{access:?}/{who}");

            // The gate follows the *read* column of the matrix, not the write one:
            // a `protected` non-owner is admitted here even though every write is
            // denied.
            assert!(
                !t.is_fixed(),
                "{tag}: a freshly seeded table starts unfixed"
            );
            assert_gate(
                t.as_requester(requester).set_fixation(true),
                read_ok,
                &format!("{tag} set_fixation(true)"),
            );
            // A denied call changed nothing; a permitted one fixed the table.
            assert_eq!(
                t.is_fixed(),
                read_ok,
                "{tag}: set_fixation(true) must only take effect when permitted"
            );

            // The clearing call obeys the same gate and is idempotent, so the
            // table ends unfixed whatever the outcome.
            assert_gate(
                t.as_requester(requester).set_fixation(false),
                read_ok,
                &format!("{tag} set_fixation(false)"),
            );
            assert!(!t.is_fixed(), "{tag}: the table ends unfixed");
        }
    }
}

/// `ordered_set` navigation is table-level, so the read-checked wrappers cover it
/// exactly like the hash kinds.
#[test]
fn ordered_set_navigation_is_access_checked() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(512);
            let mut opts = TableOptions::new();
            opts.set_kind(beam_ets::TableKind::OrderedSet);
            opts.set_access(access);
            let mut t = Table::new(TableId::next(), opts).unwrap();
            t.set_owner(Some(owner));
            {
                let mut as_owner = t.as_requester(Some(owner));
                for k in [10i64, 20, 30] {
                    as_owner.insert(object(&mut heap, k, k * 10), &atoms).unwrap();
                }
            }

            let (read_ok, _) = expected(access, owner, requester);
            let tag = format!("ordered_set {access:?}/{who}");
            let view = t.as_requester(requester);

            assert_gate(view.first(&atoms), read_ok, &format!("{tag} first"));
            assert_gate(view.last(&atoms), read_ok, &format!("{tag} last"));
            assert_gate(
                view.next(make_small(10), &atoms),
                read_ok,
                &format!("{tag} next"),
            );
            assert_gate(
                view.prev(make_small(30), &atoms),
                read_ok,
                &format!("{tag} prev"),
            );

            if read_ok {
                // The navigation actually walks the ordered key set.
                assert_eq!(view.first(&atoms).unwrap(), Some(make_small(10)));
                assert_eq!(view.last(&atoms).unwrap(), Some(make_small(30)));
                assert_eq!(view.next(make_small(10), &atoms).unwrap(), Some(make_small(20)));
                assert_eq!(view.prev(make_small(30), &atoms).unwrap(), Some(make_small(20)));
            }
        }
    }
}

/// A denied write is `badarg` — never a silent no-op reported as success — and it
/// leaves the table byte-for-byte unchanged.
#[test]
fn a_denied_write_never_reports_success() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(512);

    let mut t = seeded_table(Access::Protected, owner, &mut heap, &atoms);
    let before = t.size();
    let spec = every_object_spec(&mut heap, &mut atoms);
    let wildcard = wildcard_pattern(&mut heap, &mut atoms);

    let mut view = t.as_requester(Some(other));
    let mut bh = rebuild_heap();
    assert!(matches!(
        view.delete_all_objects(),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        view.select_delete(&spec, &atoms, &mut bh),
        Err(BeamError::BadArg)
    ));
    let mut bh = rebuild_heap();
    assert!(matches!(
        view.match_delete(wildcard, &atoms, &mut bh),
        Err(BeamError::BadArg)
    ));

    drop(view);
    assert_eq!(t.size(), before);
}

/// The multi-kind element-list lookups are read-gated exactly like the scalar
/// ones: a denied read is `badarg` for both the `/3` and the `/4` form, and the
/// permitted ones return the element list (never a scalar).
#[test]
fn the_element_list_lookups_follow_the_read_gate() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(512);
            let mut opts = TableOptions::new();
            opts.set_kind(beam_ets::TableKind::Bag);
            opts.set_access(access);
            let mut t = Table::new(TableId::next(), opts).unwrap();
            t.set_owner(Some(owner));
            {
                let mut as_owner = t.as_requester(Some(owner));
                as_owner.insert(object(&mut heap, 1, 10), &atoms).unwrap();
                as_owner.insert(object(&mut heap, 1, 20), &atoms).unwrap();
            }

            let (read_ok, _) = expected(access, owner, requester);
            let tag = format!("bag {access:?}/{who}");
            let default = make_small(-1);
            let view = t.as_requester(requester);

            assert_gate(
                view.lookup_element_all(make_small(1), 2, &atoms),
                read_ok,
                &format!("{tag} lookup_element_all"),
            );
            assert_gate(
                view.lookup_element_all_or(make_small(1), 2, default, &atoms),
                read_ok,
                &format!("{tag} lookup_element_all_or"),
            );

            if read_ok {
                let values = view.lookup_element_all(make_small(1), 2, &atoms).unwrap();
                assert_eq!(values.len(), 2, "{tag}");
                assert!(
                    values
                        .iter()
                        .any(|v| eq(*v, make_small(10), &atoms))
                        && values.iter().any(|v| eq(*v, make_small(20), &atoms)),
                    "{tag}: both objects' elements are projected"
                );
                // An absent key is `None` (the caller substitutes the default).
                assert_eq!(
                    view.lookup_element_all_or(make_small(9), 2, default, &atoms)
                        .unwrap(),
                    None,
                    "{tag}"
                );
            }
        }
    }
}

/// `ets:select_replace/2` is **write**-gated: a denied requester gets `badarg` and
/// the table is left exactly as it was; a permitted one rewrites every object.
#[test]
fn select_replace_follows_the_write_gate() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];
    // The keys `seeded_table` inserts, and the value it gives each.
    let keys = [0i64, 10, 20, 30, 40, 50];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let mut atoms = AtomTable::new();
            let mut heap = Heap::new(2048);
            let mut t = seeded_table(access, owner, &mut heap, &atoms);

            // `[{{{ '$1', 99 }}}]` — the `{{ … }}` tuple-construction body keeps the
            // key (`'$1'`) and sets the value to 99. (A bare 2-tuple in a body is a
            // *function call*, not a constructor — see `select::expr`.)
            let dollar1 = make_atom(atoms.intern("$1"));
            let underscore = interned_atom(&mut atoms, "_");
            let inner = heap.tuple(&[dollar1, make_small(99)]).unwrap();
            let construction = heap.tuple(&[inner]).unwrap();
            let head = heap.tuple(&[dollar1, underscore]).unwrap();
            let body = proper_list(&mut heap, &[construction]);
            let clause = heap.tuple(&[head, NIL, body]).unwrap();
            let spec_term = proper_list(&mut heap, &[clause]);
            let spec = compile(spec_term, &atoms).expect("a valid match spec");

            let (_, write_ok) = expected(access, owner, requester);
            let tag = format!("{access:?}/{who}");

            // The select family builds its results on a `beam_alloc::Heap` (the
            // caller's heap), distinct from the `beam_term` test arena above.
            let mut out = rebuild_heap();
            let replaced = t
                .as_requester(requester)
                .select_replace(&spec, &atoms, &mut out);
            assert_gate(replaced, write_ok, &format!("{tag} select_replace"));

            for key in keys {
                let got = t
                    .lookup_element_or(make_small(key), 2, make_small(-1), &atoms)
                    .unwrap();
                let want = if write_ok {
                    make_small(99)
                } else {
                    make_small(key * 100)
                };
                assert!(
                    eq(got, want, &atoms),
                    "{tag}: key {key} is {}",
                    if write_ok { "rewritten" } else { "untouched" }
                );
            }
        }
    }
}

/// The limited `/3` forms and their `/1` resumptions are **read**-gated. The
/// continuation used here is produced by an always-permitted (owner) view of the
/// same table, so the requester under test can fail *only* on the gate.
#[test]
fn the_limited_and_continuation_forms_follow_the_read_gate() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let mut atoms = AtomTable::new();
            let mut heap = Heap::new(2048);
            let mut t = seeded_table(access, owner, &mut heap, &atoms);
            let spec = every_object_spec(&mut heap, &mut atoms);
            let pattern = wildcard_pattern(&mut heap, &mut atoms);
            let (read_ok, _) = expected(access, owner, requester);
            let tag = format!("{access:?}/{who}");

            let continuation = t
                .as_requester(Some(owner))
                .select_limit(&spec, 2, &atoms, &mut rebuild_heap())
                .unwrap()
                .1
                .expect("six objects with limit 2 yield a continuation");

            // The select family builds its results on a `beam_alloc::Heap` (the
            // caller's heap), distinct from the `beam_term` test arena above.
            let mut out = rebuild_heap();
            let view = t.as_requester(requester);
            assert_gate(
                view.select_limit(&spec, 2, &atoms, &mut out),
                read_ok,
                &format!("{tag} select_limit"),
            );
            assert_gate(
                view.select_continue(&continuation, &atoms, &mut out),
                read_ok,
                &format!("{tag} select_continue"),
            );
            assert_gate(
                view.match_object_limit(pattern, 2, &atoms, &mut out),
                read_ok,
                &format!("{tag} match_object_limit"),
            );
            assert_gate(
                view.match_object_continue(&continuation, &atoms, &mut out),
                read_ok,
                &format!("{tag} match_object_continue"),
            );
            assert_gate(
                view.match_limit(pattern, 2, &atoms, &mut out),
                read_ok,
                &format!("{tag} match_limit"),
            );
            assert_gate(
                view.match_continue(&continuation, &atoms, &mut out),
                read_ok,
                &format!("{tag} match_continue"),
            );

            if read_ok {
                let (chunk, next) = view.select_limit(&spec, 2, &atoms, &mut out).unwrap();
                assert_eq!(chunk.len(), 2, "{tag}: an exact limit-sized chunk");
                assert!(next.is_some(), "{tag}: a full chunk carries a continuation");
            }
        }
    }
}

/// The `ets:select_reverse` family is **read**-gated, exactly like the forward
/// `select` family: it only scans the table and builds results, so it must not be
/// mistaken for a write. A non-owner may call all three forms on a `protected` or
/// `public` table but is denied (`badarg`) on a `private` one; the owner is always
/// allowed. The continuation used here is produced by an always-permitted (owner)
/// view of the same table, so the requester under test can fail *only* on the gate.
#[test]
fn the_reverse_select_family_follows_the_read_gate() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let mut atoms = AtomTable::new();
            let mut heap = Heap::new(2048);
            let mut t = seeded_table(access, owner, &mut heap, &atoms);
            let spec = every_object_spec(&mut heap, &mut atoms);
            let (read_ok, _) = expected(access, owner, requester);
            let tag = format!("{access:?}/{who}");

            let continuation = t
                .as_requester(Some(owner))
                .select_reverse_limit(&spec, 2, &atoms, &mut rebuild_heap())
                .unwrap()
                .1
                .expect("six objects with limit 2 yield a continuation");

            // The select family builds its results on a `beam_alloc::Heap` (the
            // caller's heap), distinct from the `beam_term` test arena above.
            let mut out = rebuild_heap();
            let view = t.as_requester(requester);
            assert_gate(
                view.select_reverse(&spec, &atoms, &mut out),
                read_ok,
                &format!("{tag} select_reverse"),
            );
            assert_gate(
                view.select_reverse_limit(&spec, 2, &atoms, &mut out),
                read_ok,
                &format!("{tag} select_reverse_limit"),
            );
            assert_gate(
                view.select_reverse_continue(&continuation, &atoms, &mut out),
                read_ok,
                &format!("{tag} select_reverse_continue"),
            );

            if read_ok {
                let (chunk, next) =
                    view.select_reverse_limit(&spec, 2, &atoms, &mut out).unwrap();
                assert_eq!(chunk.len(), 2, "{tag}: an exact limit-sized chunk");
                assert!(next.is_some(), "{tag}: a full chunk carries a continuation");
            } else {
                // A denial is `badarg` — not merely "not `Ok`".
                assert!(matches!(
                    view.select_reverse(&spec, &atoms, &mut out),
                    Err(BeamError::BadArg)
                ));
                assert!(matches!(
                    view.select_reverse_continue(&continuation, &atoms, &mut out),
                    Err(BeamError::BadArg)
                ));
            }
        }
    }
}

/// The reverse wrappers route to the reverse traversal, not merely to the forward
/// one: on an `ordered_set` the requester sees descending key order
/// (`ets:select_reverse/2`), the exact reverse of the forward `select`, and the
/// limited form chunks from the top down.
#[test]
fn select_reverse_routes_to_the_descending_scan() {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(1024);
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);

    let mut opts = TableOptions::new();
    opts.set_kind(TableKind::OrderedSet);
    opts.set_access(Access::Protected);
    let mut t = Table::new(TableId::next(), opts).unwrap();
    t.set_owner(Some(owner));
    {
        let mut as_owner = t.as_requester(Some(owner));
        for k in 1..=3i64 {
            as_owner.insert(object(&mut heap, k, k), &atoms).unwrap();
        }
    }

    let spec = whole_object_spec(&mut heap, &mut atoms);
    let ascending = [1i64, 2, 3];
    let mut out = rebuild_heap();
    // A non-owner reads the `protected` table — the reverse family is read-gated.
    let view = t.as_requester(Some(other));

    let forward = view.select(&spec, &atoms, &mut out).unwrap();
    assert_eq!(forward.len(), 3, "three objects selected");
    for (i, k) in ascending.iter().enumerate() {
        assert!(
            eq(forward[i], object(&mut heap, *k, *k), &atoms),
            "forward select[{i}] must be {{{k},{k}}}"
        );
    }

    let reverse = view.select_reverse(&spec, &atoms, &mut out).unwrap();
    assert_eq!(reverse.len(), 3, "three objects selected in reverse");
    for (i, k) in ascending.iter().rev().enumerate() {
        assert!(
            eq(reverse[i], object(&mut heap, *k, *k), &atoms),
            "reverse select[{i}] must be {{{k},{k}}}"
        );
    }

    // The limited form starts at the top of the order and resumes downwards.
    let (chunk, next) = view.select_reverse_limit(&spec, 2, &atoms, &mut out).unwrap();
    assert_eq!(chunk.len(), 2, "an exact limit-sized chunk");
    assert!(
        eq(chunk[0], object(&mut heap, 3, 3), &atoms),
        "the first reverse chunk starts at the top"
    );
    assert!(
        eq(chunk[1], object(&mut heap, 2, 2), &atoms),
        "…and walks down"
    );
    let next = next.expect("a full chunk carries a continuation");
    let (chunk2, next2) = view.select_reverse_continue(&next, &atoms, &mut out).unwrap();
    assert_eq!(chunk2.len(), 1, "the tail holds the last object");
    assert!(
        eq(chunk2[0], object(&mut heap, 1, 1), &atoms),
        "the tail resumes at the bottom"
    );
    assert!(next2.is_none(), "a partial final chunk ends the traversal");
}

// --------------------- The remaining families' gates + the live oracle ---------------------
/// `ets:rename/2` is **write**-gated, not owner-gated like `ets:give_away/3`: the owner
/// may rename in every mode, a non-owner only a `public` table, and a denial leaves the
/// name untouched. Verified against `erl` (OTP 29 / erts-17.1): `ets:rename/2` resolves
/// the table with the `DB_WRITE` mask (`db_get_table_aux`).
#[test]
fn rename_follows_the_write_gate() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let mut atoms = AtomTable::new();
            // An *unnamed* table: a rename only overwrites the recorded name and does
            // no conflict check, so the gate is the only thing that can deny it.
            let mut opts = TableOptions::new();
            opts.set_access(access);
            let mut t = Table::new(TableId::next(), opts).unwrap();
            t.set_owner(Some(owner));
            let before = t.name();
            let new_name = atoms.intern("renamed");
            let (_, write_ok) = expected(access, owner, requester);
            let tag = format!("{access:?}/{who}");

            let result = t.as_requester(requester).rename(new_name, |_| false);
            match (&result, write_ok) {
                (Ok(RenameOutcome::Unnamed), true) => {}
                (Err(BeamError::BadArg), false) => {}
                _ => panic!("{tag} rename: expected allowed={write_ok}, got {result:?}"),
            }
            if write_ok {
                assert_eq!(t.name(), Some(new_name), "{tag}: the name was written");
            } else {
                assert_eq!(t.name(), before, "{tag}: a denied rename changed the name");
            }
            // A rename is a name write and nothing else — no object moved.
            assert_eq!(t.size(), 0, "{tag}: rename touched no object");
        }
    }
}

/// `ets:info/1,2` is **never** access-gated: a non-owner (and an ownerless requester)
/// reads every field of a `private` table. There is no `TableAccess::info` wrapper, so
/// the introspection surface cannot be gated by construction.
#[test]
fn ets_info_is_never_access_gated() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);

    for access in [Access::Public, Access::Protected, Access::Private] {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = seeded_table(access, owner, &mut heap, &atoms);

        // The discriminating fact: a `private` table answers `ets:info` for any
        // requester, even though every read *and* write is denied to a non-owner.
        let info = t.info();
        assert_eq!(
            info.size, 6,
            "{access:?}: info is available without a requester check"
        );
        assert_eq!(info.access, access, "{access:?}: the mode is reported verbatim");
        assert_eq!(info.owner, Some(owner), "{access:?}: the owner is reported");

        // A gated op on the *same* table is still denied for a non-owner of a
        // private table — it is `info` that is ungated, not the table.
        let denied = t.as_requester(Some(other)).lookup(make_small(0), &atoms);
        assert_eq!(
            denied.is_ok(),
            access != Access::Private,
            "{access:?}: only a private table denies a non-owner reader"
        );

        // The `size`/`is_empty` projections are the `ets:info(T, size)` field and
        // are ungated for the same reason: a non-owner sizes a `private` table.
        assert_eq!(
            t.as_requester(Some(other)).size().unwrap(),
            6,
            "{access:?}: a non-owner may size any table (ets:info(T, size))"
        );
        assert!(
            !t.as_requester(Some(other)).is_empty().unwrap(),
            "{access:?}: a non-owner may test emptiness (ets:info(T, size))"
        );
    }
}

/// The **ownership** operations (`ets:give_away/3`, `ets:setopts/2`) are
/// owner-gated, not protection-gated: a non-owner is `badarg` in **every** mode,
/// `public` included, and a denial leaves the owner / the heir untouched. (The
/// protection matrix never enters their decision — a `public` table is not
/// `give_away`-able by a stranger.)
#[test]
fn the_ownership_operations_are_owner_gated_not_protection_gated() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let new_owner = OwnerId::new(3);
    let requesters = [("owner", Some(owner)), ("other", Some(other)), ("none", None)];

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in requesters {
            let atoms = AtomTable::new();
            let mut heap = Heap::new(512);
            let is_owner = requester == Some(owner);
            let tag = format!("{access:?}/{who}");

            // `ets:setopts/2` — owner-only in every mode, `public` included.
            let mut t = seeded_table(access, owner, &mut heap, &atoms);
            let opt = SetOpt::Heir(Heir {
                owner: new_owner,
                data: None,
            });
            assert_gate(
                t.as_requester(requester).setopts(opt),
                is_owner,
                &format!("{tag} setopts"),
            );
            assert_eq!(
                t.heir().is_some(),
                is_owner,
                "{tag}: setopts is owner-only, whatever the mode"
            );

            // `ets:give_away/3` likewise: no protection check, only ownership.
            let mut t = seeded_table(access, owner, &mut heap, &atoms);
            assert_gate(
                t.as_requester(requester).give_away(Some(new_owner)),
                is_owner,
                &format!("{tag} give_away"),
            );
            assert_eq!(
                t.owner(),
                if is_owner { Some(new_owner) } else { Some(owner) },
                "{tag}: give_away is owner-only, whatever the mode"
            );
        }
    }
}

// ----------------------------------- The live oracle -----------------------------------
/// One operation's gate for one requester: `true` iff the operation was
/// permitted (the oracle's `ok`; `false` is its `badarg`).
#[derive(Debug, Clone, Copy)]
struct Gates {
    read: bool,
    write: bool,
    fix: bool,
    info1: bool,
    info2: bool,
    rename: bool,
    setopts: bool,
    give_away: bool,
}

impl Gates {
    /// The oracle's field names in its emitted order, each with its gate.
    fn fields(&self) -> [(&'static str, bool); 8] {
        [
            ("read", self.read),
            ("write", self.write),
            ("fix", self.fix),
            ("info1", self.info1),
            ("info2", self.info2),
            ("rename", self.rename),
            ("setopts", self.setopts),
            ("give_away", self.give_away),
        ]
    }
}

/// Run `op` on its own **freshly seeded** `access` table owned by `owner`, as
/// `requester` — the Rust mirror of the oracle program's fresh-table-per-operation
/// probe, so no operation's mutation can perturb a later operation's observable.
fn on_fresh<R>(
    access: Access,
    owner: OwnerId,
    requester: Option<OwnerId>,
    op: impl FnOnce(Option<OwnerId>, &mut Table, &mut AtomTable, &mut Heap) -> R,
) -> R {
    let mut atoms = AtomTable::new();
    let mut heap = Heap::new(512);
    let mut table = seeded_table(access, owner, &mut heap, &atoms);
    op(requester, &mut table, &mut atoms, &mut heap)
}

/// Every operation of the access matrix, run as `requester`, each on its own
/// fresh table: the read/write split, the **read**-gated `safe_fixtable/2`, the
/// **write**-gated `rename/2`, the **ungated** `ets:info/1,2` (`info1`/`info2`)
/// and the **owner**-gated `setopts/2 {protection,_}` / `give_away/3`.
fn gates(access: Access, owner: OwnerId, requester: Option<OwnerId>) -> Gates {
    let new_owner = OwnerId::new(3);
    Gates {
        read: on_fresh(access, owner, requester, |r, t, atoms, _| {
            t.as_requester(r).lookup(make_small(0), atoms).is_ok()
        }),
        write: on_fresh(access, owner, requester, |r, t, atoms, heap| {
            t.as_requester(r).insert(object(heap, 99, 99), atoms).is_ok()
        }),
        fix: on_fresh(access, owner, requester, |r, t, _, _| {
            t.as_requester(r).set_fixation(true).is_ok()
        }),
        // `ets:info/1` takes **no** requester at all: the view cannot gate it.
        info1: on_fresh(access, owner, requester, |_, t, _, _| {
            let info = t.info();
            info.size == 6 && info.owner == Some(owner) && info.access == access
        }),
        // `ets:info(T, size)` is likewise ungated (probed for a non-owner of a
        // `private` table).
        info2: on_fresh(access, owner, requester, |r, t, _, _| {
            matches!(t.as_requester(r).size(), Ok(6))
        }),
        rename: on_fresh(access, owner, requester, |r, t, atoms, _| {
            t.as_requester(r)
                .rename(atoms.intern("oracle_renamed"), |_| false)
                .is_ok()
        }),
        setopts: on_fresh(access, owner, requester, |r, t, _, _| {
            t.as_requester(r).set_access(Access::Private).is_ok()
        }),
        give_away: on_fresh(access, owner, requester, |r, t, _, _| {
            t.as_requester(r).give_away(Some(new_owner)).is_ok()
        }),
    }
}

/// The probe: for each protection mode × requester (the **owning** process and a
/// spawned **non-owner**) every operation of the matrix is run on its **own**
/// fresh, owner-seeded table and printed as
/// `{mode}.{owner|other}.{field} -> ok|badarg`.
///
/// The fields cover the read/write split (`read`/`write`), the read-gated
/// `ets:safe_fixtable/2` (`fix`), the **ungated** `ets:info/1` / `ets:info/2`
/// (`info1`/`info2`), the write-gated `ets:rename/2` (`rename`), and the
/// **owner**-gated `ets:setopts/2 {protection,_}` (`setopts`) and
/// `ets:give_away/3` (`give_away`).
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    C = fun(E) -> try E() of _ -> ok catch error:badarg -> badarg end end,
    Parent = self(),
    %% A live process that is never the caller: the target of `ets:give_away/3`
    %% (a self-gift is `badarg`, so it must differ from whoever calls).
    Gift = spawn(fun() -> receive stop -> ok end end),
    %% One operation, on its own fresh table owned by *this* (parent) process.
    Run = fun(Prot, Who, Op) ->
        T = ets:new(probe, [set, Prot]),
        ets:insert(T, {1, 10}),
        R = case Who of
                owner -> C(fun() -> Op(T) end);
                other ->
                    Ref = make_ref(),
                    spawn(fun() -> Parent ! {Ref, C(fun() -> Op(T) end)} end),
                    receive {Ref, V} -> V after 5000 -> timeout end
            end,
        catch ets:delete(T),
        R
    end,
    Ops = [
        {read,      fun(T) -> ets:lookup(T, 1) end},
        {write,     fun(T) -> ets:insert(T, {2, 20}) end},
        {fix,       fun(T) -> ets:safe_fixtable(T, true) end},
        {info1,     fun(T) -> ets:info(T) end},
        {info2,     fun(T) -> ets:info(T, size) end},
        {rename,    fun(T) -> ets:rename(T, renamed_probe) end},
        {setopts,   fun(T) -> ets:setopts(T, {protection, private}) end},
        {give_away, fun(T) -> ets:give_away(T, Gift, []) end}
    ],
    lists:foreach(
      fun(Prot) ->
        lists:foreach(
          fun(Who) ->
            lists:foreach(
              fun({Field, Op}) ->
                Key = lists:flatten(io_lib:format("~p.~p.~p", [Prot, Who, Field])),
                P(Key, Run(Prot, Who, Op))
              end, Ops)
          end, [owner, other])
      end, [public, protected, private]),
    halt(0).
"#;

/// The **full** access matrix, cross-checked **line by line** with the live system
/// `erl`: for **both** the owner and a non-owner, in each of the three modes, the
/// crate's gate must equal the oracle's `ok`/`badarg` for the read/write split, the
/// read-gated `safe_fixtable/2`, the write-gated `rename/2`, the **ungated**
/// `ets:info/1,2`, and the **owner**-gated `setopts/2 {protection,_}` /
/// `give_away/3`.
///
/// The oracle is **required by default**: an absent Erlang fails this check rather
/// than skipping it (`BEAM_ETS_ALLOW_NO_ORACLE=1` opts out;
/// `BEAM_ETS_REQUIRE_ORACLE=1` forces it).
#[test]
fn the_access_matrix_matches_the_live_oracle() {
    const TEST: &str = "the_access_matrix_matches_the_live_oracle";
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            TEST,
            "no Erlang oracle found (no `$ERTS_ORACLE`, no `erl`/`escript` on `$PATH`)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        ORACLE_PROGRAM,
        &[],
        "access_matrix",
        ORACLE_TIMEOUT,
    );
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    let mut facts: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once(" -> ") {
            facts.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let got = |ok: bool| if ok { "ok" } else { "badarg" };

    // Every cell of the matrix: owner and non-owner × the three modes × the eight
    // operations, each compared against the oracle's own answer.
    for (access, mode) in [
        (Access::Public, "public"),
        (Access::Protected, "protected"),
        (Access::Private, "private"),
    ] {
        for (who, requester) in [("owner", Some(owner)), ("other", Some(other))] {
            let g = gates(access, owner, requester);
            let tag = format!("{mode}.{who}");
            for (field, crate_ok) in g.fields() {
                assert_eq!(
                    got(crate_ok),
                    oracle_fact(&facts, &format!("{tag}.{field}")),
                    "{tag}: `{field}` disagrees with the live oracle"
                );
            }
        }
    }

    // Belt-and-braces: pin the *oracle's* own answers, so the comparison above
    // cannot pass vacuously on a broken probe.
    for mode in ["public", "protected", "private"] {
        // The owner is admitted to every op, whatever the mode.
        for field in ["read", "write", "fix", "rename", "info1", "info2", "setopts", "give_away"] {
            assert_eq!(
                oracle_fact(&facts, &format!("{mode}.owner.{field}")),
                "ok",
                "the oracle: the owner is admitted to `{field}` in `{mode}`"
            );
        }
        // `ets:info/1,2` is ungated for **both** requesters, in every mode.
        for who in ["owner", "other"] {
            for field in ["info1", "info2"] {
                assert_eq!(
                    oracle_fact(&facts, &format!("{mode}.{who}.{field}")),
                    "ok",
                    "the oracle: `ets:info` is ungated in `{mode}` for `{who}`"
                );
            }
        }
    }
    // The discriminating cells: a non-owner of a `private` table is denied every
    // read and write yet still `info`s it, and the ownership ops are owner-gated
    // in every mode (`public` included).
    assert_eq!(oracle_fact(&facts, "private.other.read"), "badarg");
    assert_eq!(oracle_fact(&facts, "private.other.write"), "badarg");
    assert_eq!(oracle_fact(&facts, "private.other.info2"), "ok");
    assert_eq!(oracle_fact(&facts, "protected.other.read"), "ok");
    assert_eq!(oracle_fact(&facts, "protected.other.write"), "badarg");
    assert_eq!(oracle_fact(&facts, "public.other.read"), "ok");
    assert_eq!(oracle_fact(&facts, "public.other.write"), "ok");
    for mode in ["public", "protected", "private"] {
        assert_eq!(oracle_fact(&facts, &format!("{mode}.other.setopts")), "badarg");
        assert_eq!(oracle_fact(&facts, &format!("{mode}.other.give_away")), "badarg");
        // `safe_fixtable/2` follows the **read** gate: a non-owner may fix a
        // `public`/`protected` table but not a `private` one.
        assert_eq!(
            oracle_fact(&facts, &format!("{mode}.other.fix")),
            if mode == "private" { "badarg" } else { "ok" },
        );
        // `rename/2` follows the **write** gate: only a non-owner of a `public`
        // table may rename.
        assert_eq!(
            oracle_fact(&facts, &format!("{mode}.other.rename")),
            if mode == "public" { "ok" } else { "badarg" },
        );
    }
}

/// The whole matrix, pinned **without** the oracle — a compile-time-stable
/// statement of the read/write split for owner, non-owner and an ownerless
/// requester across the three modes, plus the ungated introspection and the
/// owner-gated ownership operations.
#[test]
fn the_read_write_matrix_and_the_ungated_introspection_are_pinned() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);

    for access in [Access::Public, Access::Protected, Access::Private] {
        for (who, requester) in [("owner", Some(owner)), ("other", Some(other)), ("none", None)] {
            let (read, write) = expected(access, owner, requester);
            let is_owner = requester == Some(owner);
            let g = gates(access, owner, requester);
            let tag = format!("{access:?}/{who}");

            // The read/write split of the three modes.
            assert_eq!(g.read, read, "{tag}: read");
            assert_eq!(g.write, write, "{tag}: write");
            // `ets:safe_fixtable/2` follows the *read* gate, `ets:rename/2` the
            // *write* gate (never the owner gate).
            assert_eq!(g.fix, read, "{tag}: safe_fixtable is read-gated");
            assert_eq!(g.rename, write, "{tag}: rename is write-gated");
            // `ets:info/1,2` — and the `size` projection — are **ungated**.
            assert!(g.info1, "{tag}: `ets:info/1` is ungated");
            assert!(g.info2, "{tag}: `ets:info/2` is ungated");
            // The ownership operations are **owner**-gated, whatever the mode.
            assert_eq!(g.setopts, is_owner, "{tag}: setopts is owner-gated");
            assert_eq!(g.give_away, is_owner, "{tag}: give_away is owner-gated");
        }
    }
}

// ----------------------- The full per-operation matrix vs the live oracle -----------------------
//
// `the_access_matrix_matches_the_live_oracle` above cross-checks eight
// *representative* operations (`read` = `lookup`, `write` = `insert`,
// `safe_fixtable/2`, `ets:info/1,2`, `rename/2`, `setopts/2`, `give_away/3`).
// This suite widens the oracle cross-check to **every** operation the
// access-enforcing view exposes — each read, each write, each introspection
// field, each ownership operation, each continuation resume — one row per
// operation. An operation whose gate is grouped with the wrong class
// (`select_reverse` gated as a write, `take` as a read, `size` as a read, …)
// cannot hide behind a representative one: it shows up as a line-level
// disagreement with the live oracle.
//
// Every operation is also run against a **fresh one-object table** and the
// oracle's `{SizeBefore, SizeAfter, is_list(ets:info(T))}` triple is compared, so
// the suite pins the second half of the contract too: a *denied* operation is
// exactly `badarg` **and** leaves the table intact (`SizeAfter == SizeBefore`,
// which is also the oracle's own answer), while a permitted one applies.

/// The access class an operation belongs to — its gate, stated independently of
/// the concrete access mode.
///
/// Stating the class here, rather than deriving it from the crate under test, is
/// what lets the comparison catch a *systematically* wrong gate and not merely an
/// inconsistent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpGate {
    /// A read: the oracle's `DB_READ` mask (`DB_PROTECTED|DB_PUBLIC`) — anyone may
    /// read a `protected`/`public` table, only the owner a `private` one.
    Read,
    /// A write: the oracle's `DB_WRITE` mask (`DB_PUBLIC`) — anyone may write a
    /// `public` table, only the owner a `protected`/`private` one.
    Write,
    /// Never gated: `ets:info/1,2` resolves the table with the `DB_INFO` mask (all
    /// three protection bits), so its status mask can never fail.
    Ungated,
    /// Owner-gated (`caller == owner`) whatever the protection mode:
    /// `ets:setopts/2` and `ets:give_away/3`.
    Owner,
}

impl OpGate {
    /// Whether an operation of this class is permitted for `requester` on an
    /// `access` table owned by `owner`.
    fn allows(self, access: Access, owner: OwnerId, requester: Option<OwnerId>) -> bool {
        match self {
            OpGate::Read => expected(access, owner, requester).0,
            OpGate::Write => expected(access, owner, requester).1,
            OpGate::Ungated => true,
            OpGate::Owner => requester == Some(owner),
        }
    }
}

/// One probe: run a single `ets` operation against `table` as `requester`,
/// building input terms on `arena` (the `beam_term` test arena) and pouring the
/// select family's results into `out` (the `beam_alloc` scratch heap those
/// operations require).
type OpProbe = dyn Fn(
    &mut Table,
    Option<OwnerId>,
    &mut AtomTable,
    &mut Heap,
    &mut beam_alloc::Heap,
) -> beam_common::Result<()>;

/// A table of `access` owned by `owner` holding the single object `{1, 10}` — the
/// oracle probe's seed (`ets:new(probe, [set, Prot])` + `ets:insert(T, {1, 10})`).
fn seeded_one(access: Access, owner: OwnerId, heap: &mut Heap, atoms: &AtomTable) -> Table {
    let mut opts = TableOptions::new();
    opts.set_access(access);
    let mut t = Table::new(TableId::next(), opts).expect("a fresh table is constructible");
    t.set_owner(Some(owner));
    {
        let mut as_owner = t.as_requester(Some(owner));
        as_owner
            .insert(object(heap, 1, 10), atoms)
            .expect("the owner may seed the table");
    }
    t
}

/// Run `probe` as `requester` on a **fresh** seeded `access` table owned by
/// `owner`, returning the operation's outcome plus the table's object count
/// before and after — the oracle's `{Before, After, …}` pair.
///
/// A fresh table per operation is the oracle program's own shape, so no
/// operation's mutation can perturb another row of the matrix.
fn op_outcome(
    access: Access,
    owner: OwnerId,
    requester: Option<OwnerId>,
    probe: &OpProbe,
) -> (beam_common::Result<()>, usize, usize) {
    let mut atoms = AtomTable::new();
    let mut arena = Heap::new(4096);
    let mut out = rebuild_heap_words(4096);
    let mut table = seeded_one(access, owner, &mut arena, &atoms);
    let before = table.size();
    let result = probe(&mut table, requester, &mut atoms, &mut arena, &mut out);
    (result, before, table.size())
}

/// The oracle's rendering of an operation's outcome: `ok` when it was permitted,
/// `badarg` when it was denied. Anything else is a crate defect rather than an
/// access gate, so it panics — the "typed `badarg`, never a wrong result" rule.
fn outcome(result: beam_common::Result<()>) -> &'static str {
    match result {
        Ok(()) => "ok",
        Err(BeamError::BadArg) => "badarg",
        Err(other) => panic!("a gated operation must be `ok`/`badarg`, got {other:?}"),
    }
}

/// The `[{{'$1','_'},[],[{{'$1',0}}]}]` spec: a key-keeping body, the only shape
/// `ets:select_replace/2` accepts (the C `db_match_keeps_key` static body check).
fn key_keeping_spec(heap: &mut Heap, atoms: &mut AtomTable) -> MatchSpec {
    let dollar1 = make_atom(atoms.intern("$1"));
    let underscore = interned_atom(atoms, "_");
    let inner = heap.tuple(&[dollar1, make_small(0)]).expect("test heap has room");
    let construction = heap.tuple(&[inner]).expect("test heap has room");
    let head = heap.tuple(&[dollar1, underscore]).expect("test heap has room");
    let body = proper_list(heap, &[construction]);
    let clause = heap.tuple(&[head, NIL, body]).expect("test heap has room");
    let spec = proper_list(heap, &[clause]);
    compile(spec, atoms).expect("a valid match spec")
}

/// Parse an oracle program's `key -> value` lines into a map.
fn parse_facts(stdout: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .filter_map(|line| line.split_once(" -> "))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// Every operation the access-enforcing view exposes, with its class — the rows of
/// the full matrix. The names are the oracle program's field names.
fn all_ops() -> Vec<(&'static str, OpGate, Box<OpProbe>)> {
    let read = OpGate::Read;
    let write = OpGate::Write;
    vec![
        // ------------------------------------------------------------------ reads
        ("r_lookup", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).lookup(make_small(1), atoms).map(|_| ())
        })),
        ("r_member", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).member(make_small(1), atoms).map(|_| ())
        })),
        ("r_lookup_element", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r)
                .lookup_element(make_small(1), 1, atoms)
                .map(|_| ())
        })),
        ("r_first", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).first(atoms).map(|_| ())
        })),
        ("r_last", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).last(atoms).map(|_| ())
        })),
        ("r_next", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).next(make_small(1), atoms).map(|_| ())
        })),
        ("r_prev", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).prev(make_small(1), atoms).map(|_| ())
        })),
        ("r_tab2list", read, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).tab2list().map(|_| ())
        })),
        ("r_slot", read, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).slot(1, atoms).map(|_| ())
        })),
        ("r_select", read, Box::new(|t, r, atoms, a, o| {
            let spec = every_object_spec(a, atoms);
            t.as_requester(r).select(&spec, atoms, o).map(|_| ())
        })),
        ("r_select_count", read, Box::new(|t, r, atoms, a, o| {
            let spec = every_object_spec(a, atoms);
            t.as_requester(r).select_count(&spec, atoms, o).map(|_| ())
        })),
        ("r_select_reverse", read, Box::new(|t, r, atoms, a, o| {
            let spec = every_object_spec(a, atoms);
            t.as_requester(r)
                .select_reverse(&spec, atoms, o)
                .map(|_| ())
        })),
        ("r_match_object", read, Box::new(|t, r, atoms, a, o| {
            let pattern = wildcard_pattern(a, atoms);
            t.as_requester(r)
                .match_object(pattern, atoms, o)
                .map(|_| ())
        })),
        ("r_match", read, Box::new(|t, r, atoms, a, o| {
            let pattern = wildcard_pattern(a, atoms);
            t.as_requester(r).match_(pattern, atoms, o).map(|_| ())
        })),
        ("r_safe_fixtable", read, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).set_fixation(true)
        })),
        ("r_select_limit", read, Box::new(|t, r, atoms, a, o| {
            let spec = every_object_spec(a, atoms);
            t.as_requester(r)
                .select_limit(&spec, 1, atoms, o)
                .map(|_| ())
        })),
        ("r_select_reverse_limit", read, Box::new(|t, r, atoms, a, o| {
            let spec = every_object_spec(a, atoms);
            t.as_requester(r)
                .select_reverse_limit(&spec, 1, atoms, o)
                .map(|_| ())
        })),
        ("r_match_limit", read, Box::new(|t, r, atoms, a, o| {
            let pattern = wildcard_pattern(a, atoms);
            t.as_requester(r)
                .match_limit(pattern, 1, atoms, o)
                .map(|_| ())
        })),
        ("r_match_object_limit", read, Box::new(|t, r, atoms, a, o| {
            let pattern = wildcard_pattern(a, atoms);
            t.as_requester(r)
                .match_object_limit(pattern, 1, atoms, o)
                .map(|_| ())
        })),
        // ----------------------------------------------------------------- writes
        ("w_insert", write, Box::new(|t, r, atoms, a, _o| {
            t.as_requester(r).insert(object(a, 2, 20), atoms)
        })),
        ("w_insert_new", write, Box::new(|t, r, atoms, a, _o| {
            t.as_requester(r)
                .insert_new(object(a, 2, 20), atoms)
                .map(|_| ())
        })),
        ("w_insert_list", write, Box::new(|t, r, atoms, a, _o| {
            let list = [object(a, 2, 20), object(a, 3, 30)];
            t.as_requester(r).insert_list(&list, atoms)
        })),
        ("w_delete", write, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).delete(make_small(1), atoms)
        })),
        ("w_delete_object", write, Box::new(|t, r, atoms, a, _o| {
            t.as_requester(r).delete_object(object(a, 1, 10), atoms)
        })),
        ("w_delete_all", write, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).delete_all_objects()
        })),
        ("w_take", write, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r).take(make_small(1), atoms).map(|_| ())
        })),
        ("w_update_counter", write, Box::new(|t, r, atoms, _a, o| {
            t.as_requester(r)
                .update_counter(make_small(1), make_small(1), atoms, o)
                .map(|_| ())
        })),
        ("w_update_counter_default", write, Box::new(|t, r, atoms, a, o| {
            t.as_requester(r)
                .update_counter_with(make_small(9), make_small(1), object(a, 9, 0), atoms, o)
                .map(|_| ())
        })),
        ("w_update_element", write, Box::new(|t, r, atoms, _a, o| {
            t.as_requester(r)
                .update_element(make_small(1), &[(2, make_small(99))], atoms, o)
                .map(|_| ())
        })),
        ("w_select_delete", write, Box::new(|t, r, atoms, a, o| {
            let spec = every_object_spec(a, atoms);
            t.as_requester(r).select_delete(&spec, atoms, o).map(|_| ())
        })),
        ("w_match_delete", write, Box::new(|t, r, atoms, a, o| {
            let pattern = wildcard_pattern(a, atoms);
            t.as_requester(r)
                .match_delete(pattern, atoms, o)
                .map(|_| ())
        })),
        ("w_select_replace", write, Box::new(|t, r, atoms, a, o| {
            let spec = key_keeping_spec(a, atoms);
            t.as_requester(r)
                .select_replace(&spec, atoms, o)
                .map(|_| ())
        })),
        ("w_rename", write, Box::new(|t, r, atoms, _a, _o| {
            t.as_requester(r)
                .rename(atoms.intern("renamed_probe"), |_| false)
                .map(|_| ())
        })),
        // ---------------------------------------------------------- introspection
        ("i_info1", OpGate::Ungated, Box::new(|t, _r, _atoms, _a, _o| {
            let _ = t.info();
            Ok(())
        })),
        ("i_info2_size", OpGate::Ungated, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).size().map(|_| ())
        })),
        ("i_info2_protection", OpGate::Ungated, Box::new(|t, _r, _atoms, _a, _o| {
            let _ = t.info().access;
            Ok(())
        })),
        ("i_info2_memory", OpGate::Ungated, Box::new(|t, _r, _atoms, _a, _o| {
            let _ = t.memory();
            Ok(())
        })),
        // --------------------------------------------------------------- ownership
        ("o_setopts_protection", OpGate::Owner, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).set_access(Access::Private)
        })),
        ("o_setopts_heir2", OpGate::Owner, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).setopts(SetOpt::Heir(Heir {
                owner: OwnerId::new(3),
                data: None,
            }))
        })),
        ("o_setopts_heir3", OpGate::Owner, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).setopts(SetOpt::Heir(Heir {
                owner: OwnerId::new(3),
                data: Some(make_small(1)),
            }))
        })),
        ("o_setopts_clear_heir", OpGate::Owner, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).setopts(SetOpt::ClearHeir)
        })),
        ("o_give_away", OpGate::Owner, Box::new(|t, r, _atoms, _a, _o| {
            t.as_requester(r).give_away(Some(OwnerId::new(3)))
        })),
    ]
}

/// The continuation-resume gate: build the continuation as the **owner** (so it
/// cannot fail on the gate) and resume it as `requester`.
///
/// `which` is `"sel"` (`ets:select/1`), `"rev"` (`ets:select_reverse/1`), `"m"`
/// (`ets:match/1`) or `"mo"` (`ets:match_object/1`).
fn cont_outcome(
    access: Access,
    owner: OwnerId,
    requester: Option<OwnerId>,
    which: &str,
) -> beam_common::Result<()> {
    let mut atoms = AtomTable::new();
    let mut arena = Heap::new(2048);
    let mut out = rebuild_heap_words(2048);
    let mut t = seeded_table(access, owner, &mut arena, &atoms);
    let spec = every_object_spec(&mut arena, &mut atoms);
    let pattern = wildcard_pattern(&mut arena, &mut atoms);
    let cont = match which {
        "sel" => t.as_requester(Some(owner)).select_limit(&spec, 1, &atoms, &mut out),
        "rev" => t
            .as_requester(Some(owner))
            .select_reverse_limit(&spec, 1, &atoms, &mut out),
        "m" => t
            .as_requester(Some(owner))
            .match_limit(pattern, 1, &atoms, &mut out),
        "mo" => t
            .as_requester(Some(owner))
            .match_object_limit(pattern, 1, &atoms, &mut out),
        other => panic!("unknown continuation form `{other}`"),
    }
    .expect("the owner may build the continuation")
    .1
    .expect("a full chunk carries a continuation");

    let view = t.as_requester(requester);
    match which {
        "sel" => view.select_continue(&cont, &atoms, &mut out).map(|_| ()),
        "rev" => view
            .select_reverse_continue(&cont, &atoms, &mut out)
            .map(|_| ()),
        "m" => view.match_continue(&cont, &atoms, &mut out).map(|_| ()),
        "mo" => view
            .match_object_continue(&cont, &atoms, &mut out)
            .map(|_| ()),
        _ => unreachable!(),
    }
}

/// The probe: for every protection mode × requester (the **owning** process and a
/// spawned **non-owner**) each operation of the matrix is run on its **own** fresh
/// one-object table and printed as `{mode}.{who}.{field} -> ok|badarg`, followed by
/// the same run's `{SizeBefore, SizeAfter, is_list(ets:info(T))}` as
/// `{mode}.{who}.{field}.intact`. A third section resumes an owner-built
/// continuation as the requester under test (`…c_{sel,rev,m,mo}`).
const EVERY_OP_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~w~n", [K, V]) end,
    C = fun(E) -> try E() of _ -> ok catch error:badarg -> badarg end end,
    Parent = self(),
    %% A live process that is never the caller: the target of `ets:give_away/3`
    %% (a self-gift is `badarg`, so it must differ from whoever calls).
    Gift = spawn(fun() -> receive stop -> ok end end),
    Spec = [{{'_','_'},[],['$_']}],
    Pat = {'_','_'},
    %% One op on its own fresh table owned by *this* (parent) process.
    Run = fun(Prot, Who, Op) ->
        T = ets:new(probe, [set, Prot]),
        ets:insert(T, {1, 10}),
        R = case Who of
                owner -> C(fun() -> Op(T) end);
                other ->
                    Ref = make_ref(),
                    spawn(fun() -> Parent ! {Ref, C(fun() -> Op(T) end)} end),
                    receive {Ref, V} -> V after 5000 -> timeout end
            end,
        catch ets:delete(T),
        R
    end,
    Ops = [
        %% reads
        {r_lookup,          fun(T) -> ets:lookup(T, 1) end},
        {r_member,          fun(T) -> ets:member(T, 1) end},
        {r_lookup_element,  fun(T) -> ets:lookup_element(T, 1, 1) end},
        {r_first,           fun(T) -> ets:first(T) end},
        {r_last,            fun(T) -> ets:last(T) end},
        {r_next,            fun(T) -> ets:next(T, 1) end},
        {r_prev,            fun(T) -> ets:prev(T, 1) end},
        {r_tab2list,        fun(T) -> ets:tab2list(T) end},
        {r_slot,            fun(T) -> ets:slot(T, 1) end},
        {r_select,          fun(T) -> ets:select(T, Spec) end},
        {r_select_count,    fun(T) -> ets:select_count(T, Spec) end},
        {r_select_reverse,  fun(T) -> ets:select_reverse(T, Spec) end},
        {r_match_object,    fun(T) -> ets:match_object(T, Pat) end},
        {r_match,           fun(T) -> ets:match(T, Pat) end},
        {r_safe_fixtable,   fun(T) -> ets:safe_fixtable(T, true) end},
        {r_select_limit,    fun(T) -> ets:select(T, Spec, 1) end},
        {r_select_reverse_limit, fun(T) -> ets:select_reverse(T, Spec, 1) end},
        {r_match_limit,     fun(T) -> ets:match(T, Pat, 1) end},
        {r_match_object_limit, fun(T) -> ets:match_object(T, Pat, 1) end},
        %% writes
        {w_insert,          fun(T) -> ets:insert(T, {2, 20}) end},
        {w_insert_new,      fun(T) -> ets:insert_new(T, {2, 20}) end},
        {w_insert_list,     fun(T) -> ets:insert(T, [{2,20},{3,30}]) end},
        {w_delete,          fun(T) -> ets:delete(T, 1) end},
        {w_delete_object,   fun(T) -> ets:delete_object(T, {1, 10}) end},
        {w_delete_all,      fun(T) -> ets:delete_all_objects(T) end},
        {w_take,            fun(T) -> ets:take(T, 1) end},
        {w_update_counter,  fun(T) -> ets:update_counter(T, 1, 1) end},
        {w_update_counter_default, fun(T) -> ets:update_counter(T, 9, 1, {9, 0}) end},
        {w_update_element,  fun(T) -> ets:update_element(T, 1, {2, 99}) end},
        {w_select_delete,   fun(T) -> ets:select_delete(T, [{{'_','_'},[],[true]}]) end},
        {w_match_delete,    fun(T) -> ets:match_delete(T, Pat) end},
        {w_select_replace,  fun(T) -> ets:select_replace(T, [{{'$1','_'},[],[{{'$1',0}}]}]) end},
        {w_rename,          fun(T) -> ets:rename(T, renamed_probe) end},
        %% introspection (never gated: DB_INFO)
        {i_info1,           fun(T) -> ets:info(T) end},
        {i_info2_size,      fun(T) -> ets:info(T, size) end},
        {i_info2_protection,fun(T) -> ets:info(T, protection) end},
        {i_info2_memory,    fun(T) -> ets:info(T, memory) end},
        %% ownership (owner-gated, whatever the mode)
        {o_setopts_protection, fun(T) -> ets:setopts(T, {protection, private}) end},
        {o_setopts_heir2,   fun(T) -> ets:setopts(T, {heir, Gift}) end},
        {o_setopts_heir3,   fun(T) -> ets:setopts(T, {heir, Gift, gift}) end},
        {o_setopts_clear_heir, fun(T) -> ets:setopts(T, {heir, none}) end},
        {o_give_away,       fun(T) -> ets:give_away(T, Gift, []) end}
    ],
    lists:foreach(
      fun(Prot) ->
        lists:foreach(
          fun(Who) ->
            lists:foreach(
              fun({Field, Op}) ->
                Key = lists:flatten(io_lib:format("~p.~p.~p", [Prot, Who, Field])),
                P(Key, Run(Prot, Who, Op))
              end, Ops)
          end, [owner, other])
      end, [public, protected, private]),

    %% (B) A continuation resume is read-gated exactly like the primary form: the
    %%     continuation is built by the *owner* and resumed by `Who`.
    ContRun = fun(Prot, Who, Which) ->
        T = ets:new(probe, [set, Prot]),
        ets:insert(T, {1,10}), ets:insert(T, {2,20}), ets:insert(T, {3,30}),
        Cont = case Which of
                   sel -> {_, C1} = ets:select(T, Spec, 1), C1;
                   rev -> {_, C2} = ets:select_reverse(T, Spec, 1), C2;
                   m   -> {_, C3} = ets:match(T, Pat, 1), C3;
                   mo  -> {_, C4} = ets:match_object(T, Pat, 1), C4
               end,
        Op = fun() -> case Which of
                          sel -> ets:select(Cont);
                          rev -> ets:select_reverse(Cont);
                          m   -> ets:match(Cont);
                          mo  -> ets:match_object(Cont)
                      end end,
        R = case Who of
                owner -> C(Op);
                other ->
                    Ref = make_ref(),
                    spawn(fun() -> Parent ! {Ref, C(Op)} end),
                    receive {Ref, V} -> V after 5000 -> timeout end
            end,
        catch ets:delete(T),
        R
    end,
    lists:foreach(
      fun(Prot) ->
        lists:foreach(
          fun(Who) ->
            lists:foreach(
              fun(Which) ->
                Key = lists:flatten(io_lib:format("~p.~p.c_~p", [Prot, Who, Which])),
                P(Key, ContRun(Prot, Who, Which))
              end, [sel, rev, m, mo])
          end, [owner, other])
      end, [public, protected, private]),

    %% (C) The size and liveness around every operation: a *denied* op leaves the
    %%     table intact (`After == Before`), a permitted one applies.
    Intact = fun(Prot, Who, Op) ->
        T = ets:new(probe, [set, Prot]),
        ets:insert(T, {1, 10}),
        Before = ets:info(T, size),
        _ = case Who of
                owner -> C(fun() -> Op(T) end);
                other ->
                    Ref = make_ref(),
                    spawn(fun() -> Parent ! {Ref, C(fun() -> Op(T) end)} end),
                    receive {Ref, _} -> skip after 5000 -> skip end
            end,
        After = ets:info(T, size),
        Alive = is_list(ets:info(T)),
        catch ets:delete(T),
        {Before, After, Alive}
    end,
    lists:foreach(
      fun(Prot) ->
        lists:foreach(
          fun(Who) ->
            lists:foreach(
              fun({Field, Op}) ->
                Key = lists:flatten(io_lib:format("~p.~p.~p.intact", [Prot, Who, Field])),
                P(Key, Intact(Prot, Who, Op))
              end, Ops)
          end, [owner, other])
      end, [public, protected, private]),
    halt(0).
"#;

/// The **full** per-operation access matrix, cross-checked **line by line** with
/// the live system `erl` for **both** the owner and a non-owner in each of the
/// three modes: every read, every write, every introspection field, every
/// ownership operation and every continuation resume, plus the size/liveness of
/// the table around each call.
///
/// The oracle is **required by default**: an absent Erlang fails this check rather
/// than skipping it (`BEAM_ETS_ALLOW_NO_ORACLE=1` opts out;
/// `BEAM_ETS_REQUIRE_ORACLE=1` forces it).
#[test]
fn every_read_and_write_op_of_the_access_matrix_matches_the_live_oracle() {
    const TEST: &str = "every_read_and_write_op_of_the_access_matrix_matches_the_live_oracle";
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            TEST,
            "no Erlang oracle found (no `$ERTS_ORACLE`, no `erl`/`escript` on `$PATH`)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        EVERY_OP_ORACLE_PROGRAM,
        &[],
        "access_every_op",
        ORACLE_TIMEOUT,
    );
    let facts = parse_facts(&output.stdout);

    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let ops = all_ops();

    for (access, mode) in [
        (Access::Public, "public"),
        (Access::Protected, "protected"),
        (Access::Private, "private"),
    ] {
        for (who, requester) in [("owner", Some(owner)), ("other", Some(other))] {
            for (name, gate, probe) in &ops {
                let tag = format!("{mode}.{who}.{name}");
                let (result, before, after) = op_outcome(access, owner, requester, probe.as_ref());
                let got = outcome(result);

                // (1) The crate agrees with the live oracle, cell by cell.
                assert_eq!(
                    got,
                    oracle_fact(&facts, &tag),
                    "{tag}: the crate and the live oracle disagree"
                );
                // (2) …and with the class the operation was placed in — which is
                //     what makes the comparison able to catch a *systematically*
                //     wrong gate rather than only an inconsistent one.
                let wanted = gate.allows(access, owner, requester);
                assert_eq!(
                    got == "ok",
                    wanted,
                    "{tag}: the {gate:?} class says allowed={wanted}, the view said {got}"
                );
                // (3) The table's object count before/after must be the oracle's
                //     own, and it pins the "a denial is harmless" half: a denied
                //     operation is `badarg` **and** left the table untouched.
                assert_eq!(before, 1, "{tag}: the probe seeds exactly one object");
                assert_eq!(
                    format!("{{{before},{after},true}}"),
                    oracle_fact(&facts, &format!("{tag}.intact")),
                    "{tag}: the table's object count disagrees with the oracle"
                );
                if got == "badarg" {
                    assert_eq!(after, before, "{tag}: a denied operation mutated the table");
                }
            }

            // The `/1` resumptions are read-gated exactly like the primary forms,
            // even though the continuation itself was built by the owner.
            for which in ["sel", "rev", "m", "mo"] {
                let tag = format!("{mode}.{who}.c_{which}");
                let got = outcome(cont_outcome(access, owner, requester, which));
                assert_eq!(
                    got,
                    oracle_fact(&facts, &tag),
                    "{tag}: the crate and the live oracle disagree"
                );
                assert_eq!(
                    got == "ok",
                    expected(access, owner, requester).0,
                    "{tag}: a continuation resume is read-gated"
                );
            }
        }
    }
}
