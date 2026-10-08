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

use beam_common::BeamError;
use beam_ets::{compile, Access, MatchSpec, OwnerId, Table, TableId, TableOptions};
use beam_term::prelude::*;
use common::*;

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
