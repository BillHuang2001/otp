//! Ownership semantics: `ets:give_away/3` and the `{heir, ...}` option of
//! `ets:setopts/2` (plus the `heir` introspection field).
//!
//! The oracle behaviour was verified against the system `erl` (OTP 29 /
//! erts-17.1): a fresh table's `ets:info(T, heir)` is `none`; `setopts/2` returns
//! `true` and requires the owner; `give_away/3` is `true` only for a different
//! live target and `badarg` (`cause => owner`) otherwise; `ets:info(T, holder)`
//! is `badarg` (no such item).

mod common;

use beam_common::BeamError;
use beam_ets::{
    Access, Heir, OwnerId, SetOpt, Table, TableId, TableInfo, TableKind, TableOptions,
};
use beam_term::prelude::*;
use common::*;

fn table_with_owner(owner: OwnerId) -> Table {
    let mut t = new_table(TableKind::Set);
    t.set_owner(Some(owner));
    t
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
        data: make_small(7),
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
        data: make_small(7),
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
            data: make_small(0),
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
        data: make_small(0),
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
        data: make_small(3),
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
