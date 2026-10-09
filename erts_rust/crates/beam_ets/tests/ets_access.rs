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
//! The matrix itself is verified against the system `erl` (OTP 29 / erts-17.1):
//! for a non-owner, a `protected` table admits every *read* but no *write*, a
//! `private` table admits neither, and a `public` table admits both. The
//! `TableAccess` view is exercised here for the whole observable `ets` surface —
//! including the operations it only recently began to gate.

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
            assert_gate(view.size(), allowed, &format!("{tag} size"));
            assert_gate(view.is_empty(), allowed, &format!("{tag} is_empty"));
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
/// The probe: for each protection mode, a spawned (non-owner) process exercises
/// `lookup` (read), `insert` (write), `safe_fixtable` (read — not write),
/// `ets:info/2` (never gated) and `ets:rename` (write — not owner), each recorded
/// as `ok`/`badarg`.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
%%! -noshell
main(_) ->
    P = fun(K, V) -> io:format("~s -> ~p~n", [K, V]) end,
    C = fun(E) -> try E(), ok catch error:badarg -> badarg end end,
    Parent = self(),
    Probe = fun(Prot) ->
        %% The table is owned by *this* (parent) process; the prober is not.
        T = ets:new(probe, [set, Prot]),
        ets:insert(T, {1, 10}),
        Ref = make_ref(),
        spawn(fun() ->
            Parent ! {Ref, {
                C(fun() -> ets:lookup(T, 1) end),
                C(fun() -> ets:insert(T, {2, 20}) end),
                C(fun() -> ets:safe_fixtable(T, true) end),
                C(fun() -> ets:info(T, size) end),
                C(fun() -> ets:rename(T, renamed_probe) end)
            }}
        end),
        {Read, Write, Fix, Info, Rename} =
            receive {Ref, V} -> V after 5000 -> timeout end,
        S = fun(Field) -> lists:flatten(io_lib:format("~p.~s", [Prot, Field])) end,
        P(S("read"), Read),
        P(S("write"), Write),
        P(S("fix"), Fix),
        P(S("info"), Info),
        P(S("rename"), Rename)
    end,
    [Probe(X) || X <- [public, protected, private]],
    halt(0).
"#;

/// The access matrix, cross-checked **line by line** with the live system `erl`:
/// the crate's [`TableAccess`] gate for a non-owner must match the oracle's
/// `ok`/`badarg` for every mode × operation, and `Table::info` (the ungated path)
/// must answer where the oracle's `ets:info/2` does. The oracle is a **loud skip**
/// when absent (`BEAM_ETS_REQUIRE_ORACLE=1` turns the skip into a failure).
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

    for (access, key) in [
        (Access::Public, "public"),
        (Access::Protected, "protected"),
        (Access::Private, "private"),
    ] {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(512);
        let mut t = seeded_table(access, owner, &mut heap, &atoms);

        // `ets:info/1,2` is ungated: the raw path answers regardless of requester
        // (read *before* the permitted writes below mutate the table).
        let info = t.info();
        let info_ok = info.size == 6 && info.owner == Some(owner) && info.access == access;

        // The crate's answers for a NON-owner, through the enforcing view.
        let (read, write, fix, rename) = {
            let mut view = t.as_requester(Some(other));
            let read = view.lookup(make_small(0), &atoms).is_ok();
            let write = view.insert(object(&mut heap, 99, 99), &atoms).is_ok();
            let fix = view.set_fixation(true).is_ok();
            let rename = view.rename(atoms.intern("oracle_renamed"), |_| false).is_ok();
            (read, write, fix, rename)
        };

        let got = |ok: bool| if ok { "ok" } else { "badarg" };
        for (field, crate_ok) in [
            ("read", read),
            ("write", write),
            ("fix", fix),
            ("rename", rename),
        ] {
            assert_eq!(
                got(crate_ok),
                oracle_fact(&facts, &format!("{key}.{field}")),
                "{key}: `{field}` disagrees with the live oracle"
            );
        }
        assert!(info_ok, "{key}: `ets:info` must answer for a non-owner");
        assert_eq!(
            oracle_fact(&facts, &format!("{key}.info")),
            "ok",
            "{key}: the oracle also reports `ets:info` as ungated"
        );
    }
}
