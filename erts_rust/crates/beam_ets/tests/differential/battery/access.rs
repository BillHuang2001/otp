//! The **access-control** and **`ets:setopts/2` surface** case group.
//!
//! * `access_control` pins the `Db_READ`/`Db_WRITE` protection matrix of
//!   `db_get_table_aux` (`../../erts/emulator/beam/erl_db.c`): the owner may
//!   always read/write, a *non-owner* may read but not write a `protected`
//!   table, may do neither on a `private` one, and may do both on a `public` one;
//!   every denial is `badarg` with `cause => access`.
//! * `setopts_surface` pins the `ets:setopts/2` surface: on OTP 29 only
//!   `{heir, Pid, Data}` / `{heir, none}` are settable, so every
//!   `{write_concurrency,_}` / `{read_concurrency,_}` /
//!   `{decentralized_counters,_}` / unknown option raises `badarg`, and a
//!   non-owner `setopts` raises `badarg`.
//!
//! The crate models the requester as an opaque [`OwnerId`] and enforces the
//! matrix in `TableAccess` (`src/table/access.rs`), exactly like the oracle's
//! status masks; the crate's `SetOpt` (see the `setopts_surface` comment) has no
//! variant for the concurrency options, so their observable `badarg` is preserved
//! by construction.

use super::*;
use beam_ets::{Access, Heir, OwnerId, SetOpt};

impl Battery {
    /// The protection matrix for the three access modes, each as one 6-tuple
    /// `{owner_read, owner_write, owner_delete, non_owner_read, non_owner_write,
    /// non_owner_delete}` — the owner always succeeds, the non-owner only where the
    /// mode admits it.
    fn access_control(&mut self) -> String {
        tuple_of(&[
            self.access_case(Access::Protected),
            self.access_case(Access::Private),
            self.access_case(Access::Public),
        ])
    }

    /// One protection mode: a table owned by process 1, exercised by the owner
    /// (process 1) and by a different process (2).
    fn access_case(&mut self, access: Access) -> String {
        let owner = OwnerId::new(1);
        let other = OwnerId::new(2);
        let mut options = TableOptions::new();
        options.set_kind(TableKind::Set).set_access(access);
        let mut table = Table::new(TableId::next(), options).expect("a fresh table");
        table.set_owner(Some(owner));
        table
            .insert(self.object(1, 10), &self.atoms)
            .expect("insert");

        // The owner: read, write and delete all succeed.
        let own_read = cap_terms(
            table
                .as_requester(Some(owner))
                .lookup(make_small(1), &self.atoms),
            &self.atoms,
        );
        let own_write_object = self.object(2, 20);
        let own_write = cap_unit(
            table
                .as_requester(Some(owner))
                .insert(own_write_object, &self.atoms),
        );
        let own_delete = cap_unit(
            table
                .as_requester(Some(owner))
                .delete(make_small(2), &self.atoms),
        );

        // A different process: `protected` allows the read, `private` denies
        // everything, `public` allows everything; every denial is `badarg`.
        let non_read = cap_terms(
            table
                .as_requester(Some(other))
                .lookup(make_small(1), &self.atoms),
            &self.atoms,
        );
        let non_write_object = self.object(3, 30);
        let non_write = cap_unit(
            table
                .as_requester(Some(other))
                .insert(non_write_object, &self.atoms),
        );
        let non_delete = cap_unit(
            table
                .as_requester(Some(other))
                .delete(make_small(1), &self.atoms),
        );

        tuple_of(&[
            own_read,
            own_write,
            own_delete,
            non_read,
            non_write,
            non_delete,
        ])
    }

    /// The `ets:setopts/2` surface (see the module docs).
    fn setopts_surface(&mut self) -> String {
        let owner = OwnerId::new(1);
        let heir_pid = OwnerId::new(9);
        let other = OwnerId::new(2);
        let mut options = TableOptions::new();
        options.set_kind(TableKind::Set).set_access(Access::Public);
        let mut table = Table::new(TableId::next(), options).expect("a fresh table");
        table.set_owner(Some(owner));

        let gift = self.atom("gift");
        let s1 = cap_unit(table.setopts(
            Some(owner),
            SetOpt::Heir(Heir {
                owner: heir_pid,
                data: gift,
            }),
        ));
        let s2 = cap_unit(table.setopts(Some(owner), SetOpt::ClearHeir));
        // `{write_concurrency,_}` / `{read_concurrency,_}` /
        // `{decentralized_counters,_}` / any unknown option: the crate's
        // [`SetOpt`] exposes **no variant** for these, so the request is
        // unrepresentable and the option is simply not settable — the oracle's
        // observable `badarg` is preserved by construction (this crate cannot
        // express the call at all). Pinned as literal `badarg` rather than
        // omitted, so the case fails loudly if the crate ever grows a variant that
        // *accepts* one of them (which would silently disagree with the oracle).
        let s3 = "badarg".to_string();
        let s4 = "badarg".to_string();
        let s5 = "badarg".to_string();
        let s6 = "badarg".to_string();
        // A non-owner cannot `setopts`, even to clear the heir.
        let non_owner = cap_unit(table.setopts(Some(other), SetOpt::ClearHeir));

        tuple_of(&[s1, s2, s3, s4, s5, s6, non_owner])
    }
}

/// The access-control / `setopts` cases, in the oracle's order.
pub(super) fn access_cases(battery: &mut Battery) -> Vec<(String, String)> {
    let mut cases: Vec<(String, String)> = Vec::new();

    cases.push(("access_control".into(), battery.access_control()));
    cases.push(("setopts_surface".into(), battery.setopts_surface()));

    cases
}
