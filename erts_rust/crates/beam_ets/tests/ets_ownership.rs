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

//! Ownership semantics: `ets:give_away/3` and the `{heir, ...}` option of
//! `ets:setopts/2` (plus the `heir` introspection field).
//!
//! The oracle behaviour was verified against the system `erl` (OTP 29 /
//! erts-17.1): a fresh table's `ets:info(T, heir)` is `none`; `setopts/2` returns
//! `true` and requires the owner; `give_away/3` is `true` only for a different
//! live target and `badarg` (`cause => owner`) otherwise; `ets:info(T, holder)`
//! is `badarg` (no such item).
//!
//! The second half covers the **owner-death transition** a host acts on: the live
//! oracle's facts (probed, and pinned by
//! [`owner_death_observations_match_the_live_oracle`]) are that a live heir inherits —
//! receiving an arity-4 `{'ETS-TRANSFER', Tid, FromPid, Data}` whose `Tid` is the
//! table's id, whose `FromPid` is the dead owner and whose `Data` is the gift — while
//! the heir **stays installed**; no heir, a self-heir and a dead heir each leave the
//! table gone. [`Table::owner_died`] hands that to the runtime as
//! [`OwnerDeath::Transferred`]/[`OwnerDeath::Deleted`]/[`OwnerDeath::NotOwner`].
//!
//! Finally, the **gift-data lifetime**: [`Table::install_heir`] deep-copies the gift
//! data into table-owned storage (the C `set_heir` does the same), so the term the
//! runtime puts in the transfer message cannot dangle when the installing process's
//! heap dies — the raw `setopts`/`set_heir` primitives alias instead.

mod common;

use std::time::Duration;

use beam_common::BeamError;
use beam_ets::{
    Access, Heir, OwnerDeath, OwnerId, SetOpt, Table, TableId, TableInfo, TableKind,
    TableOptions,
};
use beam_term::prelude::*;
use common::oracle;
use common::*;

fn table_with_owner(owner: OwnerId) -> Table {
    let mut t = new_table(TableKind::Set);
    t.set_owner(Some(owner));
    t
}

/// A table built with a **creation-time** `{heir, Pid, Data}` option (the `data`
/// is the caller's raw term; `Table::build` deep-copies it into table storage).
fn table_with_creation_heir(kind: TableKind, owner: OwnerId, data: Option<Eterm>) -> Table {
    let mut opts = TableOptions::new();
    opts.set_kind(kind).set_heir(Some(Heir { owner, data }));
    Table::new(TableId::next(), opts).expect("a fresh table is constructible")
}

/// A successful `give_away/3` moves ownership to the (different) target; the
/// former owner no longer owns the table.
#[test]
fn give_away_transfers_ownership() {
    let mut t = new_table(TableKind::Set);
    let from = OwnerId::new(1);
    let to = OwnerId::new(2);
    t.set_owner(Some(from));

    t.give_away(Some(from), Some(to)).expect("a valid transfer");
    assert_eq!(t.owner(), Some(to));

    // The new owner may now give it away again; the old owner cannot.
    assert!(matches!(
        t.give_away(Some(from), Some(OwnerId::new(3))),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.owner(), Some(to), "owner unchanged after a denied call");
    t.give_away(Some(to), Some(OwnerId::new(3)))
        .expect("the new owner may transfer");
    assert_eq!(t.owner(), Some(OwnerId::new(3)));
}

/// Every representable `give_away/3` failure is `badarg`, and the owner is left
/// **unchanged** after each.
#[test]
fn give_away_badarg_cases_leave_the_owner_unchanged() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);

    // (a) caller is not the current owner.
    let mut t = table_with_owner(owner);
    assert!(matches!(
        t.give_away(Some(other), Some(other)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.owner(), Some(owner));

    // (b) target is the caller's own pid (== current owner).
    let mut t = table_with_owner(owner);
    assert!(matches!(
        t.give_away(Some(owner), Some(owner)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.owner(), Some(owner));

    // (c) target is `None` — the crate's stand-in for a non-pid / dead pid.
    let mut t = table_with_owner(owner);
    assert!(matches!(
        t.give_away(Some(owner), None),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.owner(), Some(owner));

    // A `None` caller (no live calling process) is likewise not the owner.
    let mut t = table_with_owner(owner);
    assert!(matches!(
        t.give_away(None, Some(other)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.owner(), Some(owner));

    // An ownerless table: nobody can give it away.
    let mut t = new_table(TableKind::Set);
    assert!(matches!(
        t.give_away(Some(other), Some(other)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.owner(), None);
}

/// `ets:setopts/2` installs and clears the heir, and requires the owner.
#[test]
fn setopts_installs_and_clears_the_heir() {
    let owner = OwnerId::new(1);
    let heir_pid = OwnerId::new(9);
    let mut t = table_with_owner(owner);

    // Oracle: `ets:info(ets:new(t,[]), heir) =:= none` for a fresh table.
    assert_eq!(t.heir(), None);

    let gift = Heir {
        owner: heir_pid,
        data: Some(make_small(7)),
    };
    t.setopts(Some(owner), SetOpt::Heir(gift))
        .expect("the owner may set the heir");
    assert_eq!(t.heir(), Some(gift));
    // The introspectable heir is only the pid (the gift data is never reported).
    assert_eq!(t.info().heir, Some(heir_pid));

    t.setopts(Some(owner), SetOpt::ClearHeir)
        .expect("the owner may clear the heir");
    assert_eq!(t.heir(), None);
    assert_eq!(t.info().heir, None);

    // The owner-level primitive bypasses the owner check.
    t.set_heir(Some(gift));
    assert_eq!(t.heir(), Some(gift));
    t.set_heir(None);
    assert_eq!(t.heir(), None);
}

/// `setopts/2` from a non-owner (or no caller) is `badarg` and leaves the heir
/// unchanged.
#[test]
fn setopts_requires_the_owner() {
    let owner = OwnerId::new(1);
    let mut t = table_with_owner(owner);
    let gift = Heir {
        owner: OwnerId::new(9),
        data: Some(make_small(7)),
    };

    assert!(matches!(
        t.setopts(Some(OwnerId::new(8)), SetOpt::Heir(gift)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.heir(), None, "heir unchanged after a denied setopts");

    // Install a real heir, then confirm a stranger cannot clear it either.
    t.setopts(Some(owner), SetOpt::Heir(gift)).unwrap();
    assert!(matches!(
        t.setopts(Some(OwnerId::new(8)), SetOpt::ClearHeir),
        Err(BeamError::BadArg)
    ));
    assert!(matches!(
        t.setopts(None, SetOpt::ClearHeir),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.heir(), Some(gift), "heir unchanged after denied clears");
}

/// The rendered `ets:info/1` `heir` field: the atom `none` when unset and the
/// atom `undefined` when set (the documented pid gap — this crate cannot render a
/// pid term).
#[test]
fn info_renders_the_heir_field() {
    let mut atoms = AtomTable::new();
    let owner = OwnerId::new(1);

    // A fresh table reports `heir => none`.
    let mut t = table_with_owner(owner);
    let entries = t.info().entries(&mut atoms);
    assert_eq!(
        info_entry(&entries, &atoms, "heir"),
        Some(make_atom(atoms.lookup("none").unwrap()))
    );

    // A set heir renders as `undefined` (no pid term) while the structured field
    // still carries the heir pid.
    t.setopts(
        Some(owner),
        SetOpt::Heir(Heir {
            owner: OwnerId::new(6),
            data: Some(make_small(0)),
        }),
    )
    .unwrap();
    let info = t.info();
    assert_eq!(info.heir, Some(OwnerId::new(6)));
    let entries = info.entries(&mut atoms);
    assert_eq!(
        info_entry(&entries, &atoms, "heir"),
        Some(make_atom(atoms.lookup("undefined").unwrap()))
    );

    // `holder` is **not** an info item — the oracle raises `badarg`; the crate
    // simply never emits it.
    assert_eq!(info_entry(&entries, &atoms, "holder"), None);
}

/// A full `TableInfo` snapshot with owner and heir installed.
#[test]
fn table_info_snapshot_includes_owner_and_heir() {
    let id = TableId::new(77);
    let mut t = Table::new(id, TableOptions::new()).unwrap();
    let owner = OwnerId::new(5);
    let heir = OwnerId::new(6);
    t.set_owner(Some(owner));
    t.set_heir(Some(Heir {
        owner: heir,
        data: Some(make_small(0)),
    }));

    let memory = t.memory();
    assert_eq!(
        t.info(),
        TableInfo {
            id,
            name: None,
            kind: TableKind::Set,
            keypos: 1,
            access: Access::Protected,
            size: 0,
            memory,
            owner: Some(owner),
            heir: Some(heir),
            named_table: false,
            compressed: false,
            write_concurrency: false,
            read_concurrency: false,
            decentralized_counters: false,
        }
    );
    // A fresh table has no heir in the structured field.
    assert_eq!(Table::new(TableId::next(), TableOptions::new()).unwrap().info().heir, None);
}

/// The access-enforcing view routes `give_away`/`setopts` for its requester.
#[test]
fn table_access_routes_ownership() {
    let owner = OwnerId::new(1);
    let other = OwnerId::new(2);
    let mut t = table_with_owner(owner);

    // A stranger cannot give the table away (not the owner).
    assert!(matches!(
        t.as_requester(Some(other)).give_away(Some(other)),
        Err(BeamError::BadArg)
    ));
    assert_eq!(t.owner(), Some(owner));

    // The owner can, to a different target.
    t.as_requester(Some(owner))
        .give_away(Some(other))
        .expect("the owner may transfer");
    assert_eq!(t.owner(), Some(other));

    // `setopts` is likewise owner-gated through the view.
    let gift = Heir {
        owner: OwnerId::new(9),
        data: Some(make_small(3)),
    };
    assert!(matches!(
        t.as_requester(Some(owner)).setopts(SetOpt::Heir(gift)),
        Err(BeamError::BadArg)
    ));
    t.as_requester(Some(other))
        .setopts(SetOpt::Heir(gift))
        .expect("the (new) owner may set the heir");
    assert_eq!(t.heir(), Some(gift));
    assert_eq!(t.as_requester(Some(other)).requester(), Some(other));
}

// ---------------------------------------------------------------------------
// The owner-death transition: what a host gets to build `{'ETS-TRANSFER', …}`
// ---------------------------------------------------------------------------

/// The transition a host acts on when a table's owner dies: it must hand back the
/// **intact** heir (its pid *and* its gift data) and — exactly like the oracle —
/// leave the heir **installed** (the oracle still reports it from
/// `ets:info(T, heir)` after the transfer).
#[test]
fn owner_died_transfers_to_a_live_heir_and_keeps_it_installed() {
    let owner = OwnerId::new(1);
    let heir = Heir {
        owner: OwnerId::new(9),
        data: Some(make_small(7)),
    };
    let mut t = table_with_owner(owner);
    t.set_heir(Some(heir));

    let outcome = t.owner_died(owner, |_| true);
    assert_eq!(
        outcome,
        OwnerDeath::Transferred(heir),
        "the runtime gets the intact heir (owner + gift data) for the ETS-TRANSFER message"
    );
    assert_eq!(t.owner(), Some(OwnerId::new(9)), "the heir is the new owner");
    assert_eq!(
        t.heir(),
        Some(heir),
        "the heir is NOT cleared (the oracle still reports it)"
    );
    assert_eq!(t.info().owner, Some(OwnerId::new(9)));
    assert_eq!(t.info().heir, Some(OwnerId::new(9)));
}

/// A death that does not concern this table is a complete no-op: the runtime has
/// nothing to deliver and nothing to drop.
#[test]
fn owner_died_for_a_non_owner_is_a_no_op() {
    let owner = OwnerId::new(1);
    let heir = Heir {
        owner: OwnerId::new(9),
        data: Some(make_small(7)),
    };
    let mut t = table_with_owner(owner);
    t.set_heir(Some(heir));

    assert_eq!(
        t.owner_died(OwnerId::new(2), |_| panic!(
            "a non-owner death needs no liveness check"
        )),
        OwnerDeath::NotOwner
    );
    assert_eq!(t.owner(), Some(owner), "owner unchanged");
    assert_eq!(t.heir(), Some(heir), "heir unchanged");
}

/// The owner died with no heir: the runtime must drop the table (the oracle
/// reports `ets:info(T)` → `undefined`).
#[test]
fn owner_died_without_a_heir_deletes_the_table() {
    let owner = OwnerId::new(1);
    let mut t = table_with_owner(owner);
    assert_eq!(t.heir(), None, "a fresh table has no heir");

    assert_eq!(
        t.owner_died(owner, |_| panic!("no heir: no liveness check")),
        OwnerDeath::Deleted
    );
    assert_eq!(t.heir(), None);
}

/// A self-heir can never inherit: the table is dropped, and the liveness oracle is
/// not even consulted (the owner cannot outlive itself).
#[test]
fn owner_died_with_a_self_heir_deletes_the_table() {
    let owner = OwnerId::new(1);
    let mut t = table_with_owner(owner);
    t.set_heir(Some(Heir {
        owner,
        data: Some(make_small(7)),
    }));

    assert_eq!(
        t.owner_died(owner, |_| panic!("self-heir: no liveness check")),
        OwnerDeath::Deleted
    );
}

/// The heir died before the owner: the table is dropped (the liveness oracle is
/// consulted exactly once).
#[test]
fn owner_died_with_a_dead_heir_deletes_the_table() {
    let owner = OwnerId::new(1);
    let mut t = table_with_owner(owner);
    t.set_heir(Some(Heir {
        owner: OwnerId::new(9),
        data: Some(make_small(7)),
    }));

    let mut liveness_calls = 0;
    let outcome = t.owner_died(owner, |_| {
        liveness_calls += 1;
        false
    });
    assert_eq!(outcome, OwnerDeath::Deleted);
    assert_eq!(liveness_calls, 1, "liveness is consulted exactly once");
}

// ---------------------------------------------------------------------------
// The gift-data lifetime: `install_heir` copies, the raw primitives alias
// ---------------------------------------------------------------------------

/// [`Table::install_heir`] deep-copies the gift data into **table-owned** storage
/// (the A.1 copy boundary), so the table never holds a pointer into the calling
/// process's heap — the C `set_heir` (`erl_db.c:5344-5364`) copies the same way.
#[test]
fn install_heir_copies_the_gift_data_into_table_owned_storage() {
    let mut t = new_table(TableKind::Set);
    let heir_pid = OwnerId::new(9);

    // A boxed gift term on a caller-owned (process-like) heap.
    let mut source = Heap::new(64);
    let source_root = int_tuple(&mut source, &[1, 2, 3]);

    t.install_heir(heir_pid, Some(source_root))
        .expect("a plain tuple is copyable");

    let installed = t
        .heir_data()
        .expect("the heir (and its gift data) is installed");
    assert_ne!(
        installed.raw(),
        source_root.raw(),
        "the gift data is deep-copied, never aliased into the caller's heap"
    );
    assert_eq!(
        t.heir(),
        Some(Heir {
            owner: heir_pid,
            data: Some(installed),
        })
    );

    // The caller's heap dies here. The installed term is table-owned, so it still
    // reads back intact — the whole point of the copy.
    drop(source);
    let mut reference = Heap::new(64);
    let expected = int_tuple(&mut reference, &[1, 2, 3]);
    let atoms = AtomTable::new();
    assert!(
        eq(installed, expected, &atoms),
        "the table-owned gift copy survives the caller's heap"
    );
}

/// An immediate gift term is stored by value (it owns no storage), and the installed
/// data is exactly the term that was passed in.
#[test]
fn install_heir_stores_an_immediate_by_value() {
    let mut t = new_table(TableKind::Set);
    let heir_pid = OwnerId::new(4);

    t.install_heir(heir_pid, Some(make_small(42)))
        .expect("an immediate needs no storage");
    assert_eq!(t.heir_data(), Some(make_small(42)));
    assert_eq!(
        t.heir(),
        Some(Heir {
            owner: heir_pid,
            data: Some(make_small(42)),
        })
    );

    // An atom gift term behaves the same way (also an immediate).
    t.install_heir(heir_pid, Some(NIL)).expect("an immediate needs no storage");
    assert_eq!(t.heir_data(), Some(NIL));
}

/// Replacing the heir reclaims the previous copy's words (the C `free_heir_data`,
/// `erl_db.c:5367`), so repeated installs do not leak table-owned storage.
#[test]
fn install_heir_replacing_reclaims_the_previous_copy() {
    let mut t = new_table(TableKind::Set);
    let heir_pid = OwnerId::new(9);
    let mut heap = Heap::new(64);

    let first = int_tuple(&mut heap, &[1, 2, 3]);
    t.install_heir(heir_pid, Some(first)).expect("a tuple is copyable");
    let live = t.memory();
    assert!(live > 0, "the gift copy owns table-owned words");

    let second = int_tuple(&mut heap, &[4, 5, 6]);
    t.install_heir(heir_pid, Some(second)).expect("a tuple is copyable");
    assert_eq!(
        t.memory(),
        live,
        "the previous gift copy's words were reclaimed, not leaked"
    );

    let mut reference = Heap::new(64);
    let expected = int_tuple(&mut reference, &[4, 5, 6]);
    let atoms = AtomTable::new();
    assert!(eq(t.heir_data().unwrap(), expected, &atoms));
}

/// [`Table::heir_data`] is `None` while no heir is installed, and follows `set_heir`
/// (the raw, non-copying primitive) as well as `install_heir`.
#[test]
fn heir_data_tracks_the_installed_heir() {
    let owner = OwnerId::new(1);
    let mut t = table_with_owner(owner);
    assert_eq!(t.heir_data(), None, "a fresh table has no heir data");

    t.setopts(
        Some(owner),
        SetOpt::Heir(Heir {
            owner: OwnerId::new(2),
            data: Some(make_small(5)),
        }),
    )
    .unwrap();
    assert_eq!(t.heir_data(), Some(make_small(5)));

    t.setopts(Some(owner), SetOpt::ClearHeir).unwrap();
    assert_eq!(t.heir_data(), None);
}

// ---------------------------------------------------------------------------
// The reclaim path: every overwrite site frees the outgoing gift copy
// ---------------------------------------------------------------------------

/// The three overwrite sites that are **not** [`Table::install_heir`] — the raw
/// [`Table::set_heir`], [`Table::setopts`]'s `SetOpt::Heir(_)` and its
/// `SetOpt::ClearHeir` — all reclaim the outgoing heir's table-owned gift copy (the
/// C `free_heir_data`, `erl_db.c:5367`), so a host mixing them with `install_heir`
/// leaks nothing until `delete_all_objects`/drop.
#[test]
fn every_raw_overwrite_site_reclaims_the_previous_gift_copy() {
    let owner = OwnerId::new(1);
    let heir_pid = OwnerId::new(9);
    let mut heap = Heap::new(64);

    // The table's own footprint with no heir gift data installed.
    let mut t = table_with_owner(owner);
    let baseline = t.memory();

    // (a) `set_heir` replacing a copied gift with an immediate one.
    let gift = int_tuple(&mut heap, &[1, 2, 3]);
    t.install_heir(heir_pid, Some(gift))
        .expect("a tuple is copyable");
    assert!(t.memory() > baseline, "the gift copy owns table-owned words");
    t.set_heir(Some(Heir {
        owner: heir_pid,
        data: Some(make_small(7)),
    }));
    assert_eq!(t.memory(), baseline, "set_heir reclaimed the previous copy");

    // …and `set_heir(None)` clearing a copied gift.
    let gift = int_tuple(&mut heap, &[4, 5, 6]);
    t.install_heir(heir_pid, Some(gift))
        .expect("a tuple is copyable");
    assert!(t.memory() > baseline);
    t.set_heir(None);
    assert_eq!(
        t.memory(),
        baseline,
        "set_heir(None) reclaimed the previous copy"
    );

    // (b) `setopts(SetOpt::Heir(_))` replacing a copied gift.
    let gift = int_tuple(&mut heap, &[7, 8, 9]);
    t.install_heir(heir_pid, Some(gift))
        .expect("a tuple is copyable");
    assert!(t.memory() > baseline);
    t.setopts(
        Some(owner),
        SetOpt::Heir(Heir {
            owner: heir_pid,
            data: Some(make_small(1)),
        }),
    )
    .expect("the owner may replace the heir");
    assert_eq!(
        t.memory(),
        baseline,
        "setopts(Heir) reclaimed the previous copy"
    );

    // (c) `setopts(SetOpt::ClearHeir)` clearing a copied gift.
    let gift = int_tuple(&mut heap, &[10, 11, 12]);
    t.install_heir(heir_pid, Some(gift))
        .expect("a tuple is copyable");
    assert!(t.memory() > baseline);
    t.setopts(Some(owner), SetOpt::ClearHeir)
        .expect("the owner may clear the heir");
    assert_eq!(
        t.memory(),
        baseline,
        "setopts(ClearHeir) reclaimed the previous copy"
    );

    // (d) `install_heir(_, None)` — an arity-2 heir — reclaims as well.
    let gift = int_tuple(&mut heap, &[13, 14, 15]);
    t.install_heir(heir_pid, Some(gift))
        .expect("a tuple is copyable");
    assert!(t.memory() > baseline);
    t.install_heir(OwnerId::new(4), None)
        .expect("an absent gift data owns no storage");
    assert_eq!(
        t.memory(),
        baseline,
        "install_heir(_, None) reclaimed the previous copy"
    );
    assert_eq!(
        t.heir(),
        Some(Heir {
            owner: OwnerId::new(4),
            data: None,
        })
    );
}

/// Re-installing the heir that is already installed does **not** reclaim the very
/// root the incoming heir names — that would leave the table holding a dangling
/// term. (This can only happen through the raw primitives, which install the
/// caller's term verbatim.)
#[test]
fn reinstalling_the_installed_heir_keeps_its_gift_copy_alive() {
    let heir_pid = OwnerId::new(9);
    let mut t = new_table(TableKind::Set);
    let mut heap = Heap::new(64);
    let gift = int_tuple(&mut heap, &[1, 2, 3]);
    t.install_heir(heir_pid, Some(gift))
        .expect("a tuple is copyable");

    let installed = t.heir().expect("the heir is installed");
    let words = t.memory();

    t.set_heir(Some(installed));
    assert_eq!(t.memory(), words, "the live gift copy is not reclaimed");
    assert_eq!(t.heir(), Some(installed), "the heir is unchanged");

    // …and the term still reads back intact: nothing was freed out from under it.
    let mut reference = Heap::new(64);
    let expected = int_tuple(&mut reference, &[1, 2, 3]);
    assert!(eq(
        t.heir_data().expect("the gift data is installed"),
        expected,
        &AtomTable::new()
    ));
}

// ---------------------------------------------------------------------------
// Creation-time heir: the gift data is copied at build time
// ---------------------------------------------------------------------------

/// A **creation-time** `{heir, Pid, Data}` gift term is deep-copied into
/// table-owned storage by [`Table::build`] — exactly as [`Table::install_heir`]
/// does for a later `setopts/2` — so the table never holds a pointer into the
/// calling process's heap (the C `ets_new` → `insert_heir` copies the same way:
/// `erl_db.c` `insert_heir`:5286-5296 + `erts_db_alloc(ERTS_ALC_T_DB_HEIR_DATA, …)`).
#[test]
fn creation_time_heir_copies_the_gift_data_into_table_owned_storage() {
    // A boxed gift term on a caller-owned (process-like) heap.
    let mut source = Heap::new(64);
    let source_root = int_tuple(&mut source, &[1, 2, 3]);

    let t = table_with_creation_heir(TableKind::Set, OwnerId::new(9), Some(source_root));

    let installed = t
        .heir_data()
        .expect("the creation-time heir (and its gift data) is installed");
    assert_ne!(
        installed.raw(),
        source_root.raw(),
        "the creation-time gift data is deep-copied, never aliased into the caller's heap"
    );
    assert_eq!(
        t.heir(),
        Some(Heir {
            owner: OwnerId::new(9),
            data: Some(installed),
        })
    );

    // The caller's heap dies here. The installed term is table-owned, so it still
    // reads back intact — the whole point of the copy.
    drop(source);
    let mut reference = Heap::new(64);
    let expected = int_tuple(&mut reference, &[1, 2, 3]);
    assert!(
        eq(installed, expected, &AtomTable::new()),
        "the table-owned creation-time gift copy survives the caller's heap"
    );
}

/// A creation-time **immediate** gift term is stored by value (it owns no storage),
/// so the table's heir data is exactly the term the option carried.
#[test]
fn creation_time_heir_stores_an_immediate_by_value() {
    let t = table_with_creation_heir(TableKind::Set, OwnerId::new(4), Some(make_small(42)));
    assert_eq!(
        t.heir_data().map(|term| term.raw()),
        Some(make_small(42).raw()),
        "an immediate is stored by value, not copied"
    );
    assert_eq!(
        t.heir(),
        Some(Heir {
            owner: OwnerId::new(4),
            data: Some(make_small(42)),
        })
    );
}

/// Clearing or replacing a **creation-time** heir reclaims the copy's words — the
/// same reclaim path as a later `install_heir` — so the creation-time gift no longer
/// leaks and `Table::memory()` returns to the pre-heir baseline.
#[test]
fn clearing_or_replacing_a_creation_time_heir_reclaims_the_copy() {
    // The footprint of the same table with no heir at all.
    let baseline = new_table(TableKind::Set).memory();

    // (a) clearing the creation-time boxed gift via the raw `set_heir(None)`.
    let mut heap = Heap::new(64);
    let gift = int_tuple(&mut heap, &[1, 2, 3]);
    let mut t = table_with_creation_heir(TableKind::Set, OwnerId::new(9), Some(gift));
    assert!(
        t.memory() > baseline,
        "the creation-time gift copy owns table-owned words"
    );
    t.set_heir(None);
    assert_eq!(
        t.memory(),
        baseline,
        "set_heir(None) reclaimed the creation-time copy"
    );
    assert_eq!(t.heir_data(), None);

    // (b) replacing it with an arity-2 heir via `install_heir(_, None)`.
    let mut heap = Heap::new(64);
    let gift = int_tuple(&mut heap, &[4, 5, 6]);
    let mut t = table_with_creation_heir(TableKind::Set, OwnerId::new(9), Some(gift));
    assert!(t.memory() > baseline);
    t.install_heir(OwnerId::new(4), None)
        .expect("an absent gift data owns no storage");
    assert_eq!(
        t.memory(),
        baseline,
        "install_heir(_, None) reclaimed the creation-time copy"
    );
    assert_eq!(
        t.heir(),
        Some(Heir {
            owner: OwnerId::new(4),
            data: None,
        })
    );

    // (c) replacing it with an immediate gift via `install_heir` (also reclaims).
    let mut heap = Heap::new(64);
    let gift = int_tuple(&mut heap, &[7, 8, 9]);
    let mut t = table_with_creation_heir(TableKind::Set, OwnerId::new(9), Some(gift));
    assert!(t.memory() > baseline);
    t.install_heir(OwnerId::new(4), Some(make_small(1)))
        .expect("an immediate needs no storage");
    assert_eq!(
        t.memory(),
        baseline,
        "install_heir(_, immediate) reclaimed the creation-time copy"
    );
    assert_eq!(t.heir_data(), Some(make_small(1)));
}
// ---------------------------------------------------------------------------
// Live-oracle cross-check of the owner-death observations
// ---------------------------------------------------------------------------

/// Wall-clock bound for the oracle child, so a wedged oracle fails instead of hanging.
const ORACLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The embedded oracle program (an `escript`). It observes the four owner-death
/// outcomes the transition models, against the real `ets`:
///
/// * a live heir inherits and receives an arity-4 `{'ETS-TRANSFER', Tid, From,
///   Data}` whose `Tid` is the table's id, whose `From` is the dead owner, and whose
///   `Data` is the gift; the heir **stays installed** and the new owner (here a third
///   party, since the table is `public`) can insert/lookup;
/// * no heir, a self-heir and a (normalised-away) dead heir each leave the table gone
///   (`ets:info(T)` → `undefined`).
///
/// It prints one canonical line per group; the Rust assertions below pin the exact
/// observations.
const ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript
main(_) ->
    Parent = self(),
    Tag = list_to_atom("ETS-TRANSFER"),

    %% A live heir: the owner dies, the heir inherits.
    Heir = spawn(fun() -> receive M -> Parent ! {got, M} end,
                            receive stop -> ok end end),
    Owner = spawn(fun() ->
        T = ets:new(owned, [public, {heir, Heir, {gift, 42}}]),
        ets:insert(T, {a, 1}),
        Parent ! {tid, T},
        receive stop -> ok end
    end),
    T = receive {tid, X} -> X end,
    exit(Owner, kill),
    M = receive {got, Y} -> Y after 5000 -> timeout end,
    case is_tuple(M) andalso tuple_size(M) =:= 4 of
        true ->
            Ins = ets:insert(T, {b, 2}),
            Lk = ets:lookup(T, a),
            io:format("transfer ~p ~p ~p ~p ~p ~p ~p ~p ~p~n",
                      [element(1, M) =:= Tag, tuple_size(M), element(2, M) =:= T,
                       element(3, M) =:= Owner, element(4, M),
                       ets:info(T, heir) =:= Heir, ets:info(T, owner) =:= Heir,
                       Ins, Lk]);
        false ->
            io:format("transfer timeout~n", [])
    end,

    %% No heir: the table is dropped.
    Owner2 = spawn(fun() -> T2 = ets:new(noheir, [public]),
                             Parent ! {tid2, T2}, receive stop -> ok end end),
    T2 = receive {tid2, X2} -> X2 end,
    exit(Owner2, kill),
    timer:sleep(500),

    %% A self-heir: the table is dropped.
    Owner3 = spawn(fun() -> T3 = ets:new(selfheir, [public]),
                             _ = ets:setopts(T3, {heir, self(), d}),
                             Parent ! {tid3, T3}, receive stop -> ok end end),
    T3 = receive {tid3, X3} -> X3 end,
    SelfInstalled = ets:info(T3, heir) =:= ets:info(T3, owner),
    exit(Owner3, kill),
    timer:sleep(500),

    %% A dead heir: normalised to none at option-parse time, then the table is dropped.
    Dead = spawn(fun() -> ok end),
    timer:sleep(500),
    Owner4 = spawn(fun() -> T4 = ets:new(deadheir, [public]),
                             _ = ets:setopts(T4, {heir, Dead, d}),
                             Parent ! {tid4, T4}, receive stop -> ok end end),
    T4 = receive {tid4, X4} -> X4 end,
    DeadHeirNone = ets:info(T4, heir) =:= none,
    exit(Owner4, kill),
    timer:sleep(500),

    io:format("deaths no_heir_gone=~p self_installed=~p self_heir_gone=~p "
              "dead_heir_none=~p dead_heir_gone=~p~n",
              [ets:info(T2) =:= undefined, SelfInstalled, ets:info(T3) =:= undefined,
               DeadHeirNone, ets:info(T4) =:= undefined]),
    ok.
"#;

/// The owner-death observations the crate's [`Table::owner_died`] models must match
/// the live oracle: a live heir inherits (receiving the arity-4 transfer message with
/// the table id, the dead owner and the gift data) and the heir stays installed; no
/// heir / a self-heir / a dead heir each drop the table. Skips loudly without an
/// oracle; `BEAM_ETS_REQUIRE_ORACLE=1` turns the skip into a failure.
#[test]
fn owner_death_observations_match_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "owner_death_observations_match_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(&escript, ORACLE_PROGRAM, &[], "ownership", ORACLE_TIMEOUT);
    let lines: Vec<&str> = std::str::from_utf8(&output.stdout)
        .expect("the oracle prints UTF-8")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    assert_eq!(
        lines,
        vec![
            "transfer true 4 true true {gift,42} true true true [{a,1}]",
            "deaths no_heir_gone=true self_installed=true self_heir_gone=true \
             dead_heir_none=true dead_heir_gone=true",
        ],
        "the oracle's owner-death observations changed — the transition model is stale"
    );
}

// ---------------------------------------------------------------------------
// Live-oracle cross-check of the heir's two forms (arity 2 vs arity 3)
// ---------------------------------------------------------------------------

/// The embedded oracle program (an `escript`) for the two `{heir, …}` forms. It
/// installs the heir both ways the oracle allows — `ets:setopts/2` and the
/// creation-time `{heir, …}` option — kills the owner, and reports, per form, the
/// `Data` the heir received in `{'ETS-TRANSFER', …}` (or `no_message`), whether the
/// heir actually inherited the table, and whether the table survived.
///
/// The heir blocks on `keep_alive/0` for the whole observation, so nothing races the
/// heir's own death. It prints one canonical line per group; the Rust assertions
/// below pin the exact output — `{heir, Pid}` inherits **silently**, `{heir, Pid,
/// undefined}` gets a message carrying `undefined`.
const HEIR_FORM_ORACLE_PROGRAM: &str = r#"#!/usr/bin/env escript

main(_) ->
    Parent = self(),
    io:format("setopts ~p~n", [observe_forms(Parent, setopts_forms())]),
    io:format("creation ~p~n", [observe_forms(Parent, creation_forms())]),
    ok.

%% One entry per heir form: a label plus a fun that builds the table with that form
%% already installed.
setopts_forms() ->
    [{setopts_arity2, fun(H) ->
        T = ets:new(setopts_a2, [public]),
        _ = ets:setopts(T, {heir, H}),
        T
     end},
     {setopts_arity3u, fun(H) ->
        T = ets:new(setopts_a3u, [public]),
        _ = ets:setopts(T, {heir, H, undefined}),
        T
     end},
     {setopts_arity3d, fun(H) ->
        T = ets:new(setopts_a3d, [public]),
        _ = ets:setopts(T, {heir, H, {gift, 42}}),
        T
     end}].

creation_forms() ->
    [{creation_arity2, fun(H) -> ets:new(creation_a2, [public, {heir, H}]) end},
     {creation_arity3u, fun(H) -> ets:new(creation_a3u, [public, {heir, H, undefined}]) end},
     {creation_arity3d, fun(H) -> ets:new(creation_a3d, [public, {heir, H, {gift, 42}}]) end}].

observe_forms(Parent, Forms) ->
    [observe(Parent, Label, Make) || {Label, Make} <- Forms].

observe(Parent, Label, Make) ->
    Heir = spawn(fun() -> receive M -> Parent ! {Label, M}, keep_alive() end end),
    Owner = spawn(fun() ->
        T = Make(Heir),
        Parent ! {tid, Label, T},
        receive stop -> ok end
    end),
    T = receive {tid, Label, X} -> X end,
    timer:sleep(150),
    exit(Owner, kill),
    timer:sleep(200),
    %% The inheritance happens whether or not the heir is told about it.
    OwnerIsHeir = ets:info(T, owner) =:= Heir,
    Alive = ets:info(T) =/= undefined,
    Payload = receive {Label, Y} -> summarize(Y) after 2000 -> no_message end,
    Heir ! go,
    {Label, Payload, OwnerIsHeir, Alive}.

summarize({'ETS-TRANSFER', _Tid, _From, Data}) -> Data;
summarize(Other) -> Other.

keep_alive() -> receive go -> ok end.
"#;

/// The two heir forms, cross-checked against the live oracle: an arity-2
/// `{heir, Pid}` heir inherits the table **silently**, while an arity-3
/// `{heir, Pid, undefined}` heir receives `{'ETS-TRANSFER', …, undefined}` —
/// `undefined` is a *value*, the C's `THE_NON_VALUE` is not. Both forms install the
/// heir and move ownership; only the message differs. Skips loudly without an
/// oracle; `BEAM_ETS_REQUIRE_ORACLE=1` turns the skip into a failure.
#[test]
fn heir_forms_match_the_live_oracle() {
    let Some(escript) = oracle::resolve_escript() else {
        oracle::skip(
            "heir_forms_match_the_live_oracle",
            "no Erlang oracle found ($ERTS_ORACLE unset and no executable `erl`/`escript` on $PATH)",
        );
        return;
    };
    let output = oracle::run_escript(
        &escript,
        HEIR_FORM_ORACLE_PROGRAM,
        &[],
        "heir_forms",
        ORACLE_TIMEOUT,
    );
    let lines: Vec<&str> = std::str::from_utf8(&output.stdout)
        .expect("the oracle prints UTF-8")
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let expected = "setopts [{setopts_arity2,no_message,true,true},\
                    {setopts_arity3u,undefined,true,true},\
                    {setopts_arity3d,{gift,42},true,true}]";
    let expected_creation = "creation [{creation_arity2,no_message,true,true},\
                             {creation_arity3u,undefined,true,true},\
                             {creation_arity3d,{gift,42},true,true}]";
    // The oracle's `~p` wraps a long term across lines (indenting the continuation,
    // which the `trim` above removes), so glue the lines back together — the
    // rendering itself is deterministic.
    assert_eq!(
        lines.join(""),
        format!("{expected}{expected_creation}"),
        "the oracle's heir-form observations changed — the model is stale:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );

    // The model reproduces the same facts. The gift-data shapes the oracle reports.
    let owner = OwnerId::new(1);
    let heir_pid = OwnerId::new(9);
    let mut atoms = AtomTable::new();
    let undefined_gift = interned_atom(&mut atoms, "undefined");
    let gift_atom = interned_atom(&mut atoms, "gift");
    let mut heap = Heap::new(64);
    let gift_tuple = tuple_of(&mut heap, &[gift_atom, make_small(42)]);

    for (label, gift) in [
        ("arity2", None),
        ("arity3u", Some(undefined_gift)),
        ("arity3d", Some(gift_tuple)),
    ] {
        let mut t = table_with_owner(owner);
        t.install_heir(heir_pid, gift)
            .unwrap_or_else(|error| panic!("{label}: a copyable gift data: {error}"));

        let OwnerDeath::Transferred(transferred) = t.owner_died(owner, |_| true) else {
            panic!("{label}: a live heir inherits")
        };
        assert_eq!(transferred.owner, heir_pid, "{label}: the heir inherits");
        assert_eq!(t.owner(), Some(heir_pid), "{label}: the owner IS the heir");
        assert_eq!(
            t.info().heir,
            Some(heir_pid),
            "{label}: the heir stays installed"
        );
        match (transferred.data, gift) {
            (None, None) => {}
            (Some(delivered), Some(want)) => assert!(
                eq(delivered, want, &atoms),
                "{label}: the delivered payload is the installed gift data"
            ),
            (delivered, want) => panic!(
                "{label}: the payload's presence must match the option's arity: \
                 delivered={delivered:?}, expected={want:?}"
            ),
        }
    }
}