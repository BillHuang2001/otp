//! The table **lifecycle** surface: what happens to a table when its owner dies.
//!
//! An ETS table belongs to the process that created it. When that process dies the
//! table is either **handed to its heir** or **dropped**; there is no third
//! outcome. [`Table::owner_died`] is the crate's entry point for that transition:
//! the runtime (which owns process death and message delivery, both absent from
//! this crate) calls it when it observes the death of a table's owner and acts on
//! the resulting [`OwnerDeath`].
//!
//! # Oracle evidence (probed against the live system `erl`, OTP 29 / erts-17.1)
//!
//! * **owner dies with a live heir** — the heir inherits the table (`ets:info(T,
//!   owner)` becomes the heir) and afterwards `ets:info(T, heir)` is **still** the
//!   heir (it is *not* reset to `none`). So the transition moves the owner and
//!   leaves the heir installed.
//! * **owner dies with a live heir installed by `{heir, Pid}` (arity 2)** — the
//!   inheritance still happens, but the heir receives **no**
//!   `{'ETS-TRANSFER', …}` message at all (the C `THE_NON_VALUE` gift data); an
//!   arity-3 `{heir, Pid, undefined}` heir *does* get one carrying the atom
//!   `undefined`. See [`super::ownership`] and [`Heir::data`].
//! * **owner dies with no heir** — the table disappears (`ets:info/1` →
//!   `undefined`).
//! * **owner dies after the heir already died** — the table disappears.
//! * **a self-heir** (`ets:setopts(T, {heir, self(), Data})`, i.e. the owner is its
//!   own heir) — the table disappears; the owner cannot inherit from itself.
//! * the option *parsing* is the BIF layer's, not this module's: the oracle accepts
//!   `{heir, Pid}`, `{heir, Pid, Data}` and `{heir, none}`, accepts a **dead** pid
//!   as a heir (normalising it to no heir), and rejects
//!   `{heir, none, Data}` with `badarg`. None of that is modelled here.
//!
//! The legacy C mirrors this: the death path in `erl_db.c` only calls
//! `give_away_to_heir/2` when `tb->common.heir != am_none && tb->common.heir != pid`
//! (so a self-heir short-circuits, exactly as below) and falls through to deleting
//! the table when the heir cannot take it (`to_proc == NULL` ⇒ "heir not alive,
//! table still mine"). Whether the transfer message is sent is a *separate* test —
//! `if (is_value(heir_data))` at `:4925`, false for the arity-2 `THE_NON_VALUE` —
//! which is why inheriting and being notified are two independent facts.
//!
//! # The pid gap (a documented modelling limit)
//!
//! This crate has **no process-liveness concept**: an [`OwnerId`] is an opaque
//! `u64` (see the crate `CONTEXT.md`, Known Issues). Liveness therefore enters
//! through the caller's `is_alive` closure — the process layer's oracle — and the
//! actual `{'ETS-TRANSFER', …}` delivery (or table drop) is the runtime's job,
//! since this crate has no message or process machinery.

use super::{Heir, OwnerId, Table};

/// What the runtime must do with a table after one of its owners died
/// ([`Table::owner_died`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerDeath {
    /// The dead process did not own this table — nothing changed. The runtime has
    /// nothing to do for this table.
    NotOwner,
    /// The owner died and no heir took the table: the runtime must **drop** it (the
    /// oracle reports `ets:info(T)` → `undefined` afterwards).
    Deleted,
    /// The owner died and the table passed to the heir: the runtime delivers
    /// `{'ETS-TRANSFER', Tid, FromPid, data}` to `heir.owner` **only when
    /// [`heir.data`] is `Some(data)`** — an arity-2 `{heir, Pid}` heir inherits in
    /// silence (the oracle's `THE_NON_VALUE` gift data; see
    /// [`Heir::data`]). The table is now owned by `heir.owner`.
    Transferred(Heir),
}

impl Table {
    /// The owner-death transition: `dead` is the process the runtime just observed
    /// die; `is_alive` reports whether an [`OwnerId`] is still alive (the process
    /// layer's liveness oracle).
    ///
    /// Exactly matching the probed oracle:
    ///
    /// * `dead` is not the owner ⇒ [`OwnerDeath::NotOwner`], nothing changes;
    /// * the owner died and there is no heir ⇒ [`OwnerDeath::Deleted`];
    /// * the owner died and the heir is the owner itself (a **self-heir**) or the
    ///   heir is no longer alive ⇒ [`OwnerDeath::Deleted`];
    /// * otherwise the owner becomes the heir and the heir **stays installed** (the
    ///   oracle still reports it afterwards) ⇒ [`OwnerDeath::Transferred`].
    ///
    /// `is_alive` is consulted **at most once**, and only when a live heir could
    /// still take the table: the not-owner and no-heir paths never need it, and a
    /// self-heir short-circuits before it (the owner cannot inherit from itself).
    ///
    /// The returned [`OwnerDeath`] is an *obligation*, not a side effect: this crate
    /// has no message or process machinery, so the runtime must drop the table for
    /// [`Deleted`](OwnerDeath::Deleted) and, for
    /// [`Transferred`](OwnerDeath::Transferred), deliver the
    /// `{'ETS-TRANSFER', …}` message **iff** [`Heir::data`] is `Some` (an arity-2
    /// heir inherits without a message).
    #[must_use = "the runtime must act on the outcome: deliver {'ETS-TRANSFER', …} or drop the table"]
    pub fn owner_died<F>(&mut self, dead: OwnerId, is_alive: F) -> OwnerDeath
    where
        F: FnOnce(OwnerId) -> bool,
    {
        if self.owner != Some(dead) {
            return OwnerDeath::NotOwner;
        }
        let Some(heir) = self.heir else {
            return OwnerDeath::Deleted;
        };
        // A self-heir can never take the table, so the liveness oracle is not
        // needed for it (`||` short-circuits, matching the C's
        // `heir != pid` guard).
        if heir.owner == dead || !is_alive(heir.owner) {
            return OwnerDeath::Deleted;
        }
        // The heir steps in and the heir stays installed: the oracle still reports
        // it from `ets:info(T, heir)` after the transfer.
        self.owner = Some(heir.owner);
        OwnerDeath::Transferred(heir)
    }
}

#[cfg(test)]
mod tests {
    use core::cell::Cell;

    use beam_term::prelude::*;

    use super::*;
    use crate::kind::TableOptions;
    use crate::table::TableId;

    fn new_table() -> Table {
        Table::new(TableId::next(), TableOptions::new()).expect("a fresh table is constructible")
    }

    fn heir_of(pid: u64, data: i64) -> Heir {
        Heir {
            owner: OwnerId::new(pid),
            data: Some(make_small(data)),
        }
    }

    /// Drive one owner-death transition and report the outcome, how many times the
    /// liveness oracle was consulted, and the table afterwards.
    fn run_death(
        owner: Option<OwnerId>,
        heir: Option<Heir>,
        dead: OwnerId,
        heir_alive: bool,
    ) -> (OwnerDeath, usize, Table) {
        let mut t = new_table();
        t.set_owner(owner);
        t.set_heir(heir);
        let calls = Cell::new(0usize);
        let outcome = t.owner_died(dead, |_| {
            calls.set(calls.get() + 1);
            heir_alive
        });
        (outcome, calls.get(), t)
    }

    /// A death that does not concern this table is a complete no-op — and it costs
    /// nothing: the liveness oracle is never consulted.
    #[test]
    fn not_owner_is_a_complete_no_op() {
        let owner = OwnerId::new(1);
        let heir = heir_of(9, 7);
        let (outcome, calls, t) = run_death(Some(owner), Some(heir), OwnerId::new(2), false);

        assert_eq!(outcome, OwnerDeath::NotOwner);
        assert_eq!(calls, 0, "the liveness oracle is never consulted");
        assert_eq!(t.owner(), Some(owner), "owner unchanged");
        assert_eq!(t.heir(), Some(heir), "heir unchanged");
        assert_eq!(t.info().owner, Some(owner));

        // …and a table with no owner at all is likewise unaffected.
        let (outcome, calls, t) = run_death(None, Some(heir), OwnerId::new(2), false);
        assert_eq!(outcome, OwnerDeath::NotOwner);
        assert_eq!(calls, 0);
        assert_eq!(t.owner(), None);
        assert_eq!(t.heir(), Some(heir));
    }

    /// The owner died with no heir: the table must be dropped.
    #[test]
    fn no_heir_deletes_the_table() {
        let owner = OwnerId::new(1);
        let (outcome, calls, t) = run_death(Some(owner), None, owner, true);

        assert_eq!(outcome, OwnerDeath::Deleted);
        assert_eq!(calls, 0, "there is no heir, so nothing to ask about");
        assert_eq!(t.heir(), None);
        // The runtime drops the table, so its ownership is moot; the transition
        // deliberately leaves the metadata untouched rather than inventing a state.
        assert_eq!(t.owner(), Some(owner));
    }

    /// The owner died and its (different, live) heir inherits: the owner moves to
    /// the heir and the heir **stays installed** (the oracle still reports it).
    #[test]
    fn a_live_heir_inherits_the_table_and_the_heir_is_kept() {
        let owner = OwnerId::new(1);
        let heir = heir_of(9, 7);
        let (outcome, calls, t) = run_death(Some(owner), Some(heir), owner, true);

        assert_eq!(
            outcome,
            OwnerDeath::Transferred(heir),
            "the runtime gets the heir (with its gift data) to build the \
             ETS-TRANSFER message"
        );
        assert_eq!(calls, 1, "liveness is consulted exactly once");
        assert_eq!(t.owner(), Some(OwnerId::new(9)), "the heir is the new owner");
        assert_eq!(t.heir(), Some(heir), "the heir is NOT cleared");
        assert_eq!(t.info().owner, Some(OwnerId::new(9)));
        assert_eq!(t.info().heir, Some(OwnerId::new(9)));
    }

    /// An arity-2 heir inherits the table but carries **no** gift data, so the
    /// runtime must deliver no `{'ETS-TRANSFER', …}` message (the oracle's
    /// `THE_NON_VALUE`).
    #[test]
    fn an_arity_two_heir_inherits_without_gift_data() {
        let owner = OwnerId::new(1);
        let heir = Heir {
            owner: OwnerId::new(9),
            data: None,
        };
        let (outcome, calls, t) = run_death(Some(owner), Some(heir), owner, true);

        assert_eq!(outcome, OwnerDeath::Transferred(heir));
        let OwnerDeath::Transferred(transferred) = outcome else {
            unreachable!("the assertion above just proved the variant")
        };
        assert_eq!(
            transferred.data, None,
            "an arity-2 heir means no ETS-TRANSFER message"
        );
        assert_eq!(calls, 1, "liveness is consulted exactly once");
        assert_eq!(t.owner(), Some(OwnerId::new(9)), "the heir is the new owner");
        assert_eq!(t.heir(), Some(heir), "the heir is NOT cleared");
    }

    /// An owner cannot inherit from itself: a self-heir means the table is dropped,
    /// and the liveness oracle is not even needed.
    #[test]
    fn a_self_heir_deletes_the_table() {
        let owner = OwnerId::new(1);
        let heir = heir_of(owner.raw(), 3);
        let (outcome, calls, t) = run_death(Some(owner), Some(heir), owner, true);

        assert_eq!(outcome, OwnerDeath::Deleted);
        assert_eq!(calls, 0, "the self-heir short-circuits the liveness check");
        assert_eq!(t.owner(), Some(owner));
        assert_eq!(t.heir(), Some(heir), "the heir is untouched");
    }

    /// The heir died before the owner: the table is dropped.
    #[test]
    fn a_dead_heir_deletes_the_table() {
        let owner = OwnerId::new(1);
        let heir = heir_of(9, 7);
        let (outcome, calls, t) = run_death(Some(owner), Some(heir), owner, false);

        assert_eq!(outcome, OwnerDeath::Deleted);
        assert_eq!(calls, 1, "liveness is consulted exactly once");
        assert_eq!(t.owner(), Some(owner));
        assert_eq!(t.heir(), Some(heir), "the heir is untouched");
    }

    /// The whole matrix at once: the liveness oracle is consulted exactly once on
    /// each heir-liveness path and never on any other path.
    #[test]
    fn the_liveness_oracle_is_consulted_once_on_the_heir_paths() {
        let owner = OwnerId::new(1);
        let heir = heir_of(9, 7);

        // Consulted exactly once: a live heir and a dead heir.
        assert_eq!(run_death(Some(owner), Some(heir), owner, true).1, 1);
        assert_eq!(run_death(Some(owner), Some(heir), owner, false).1, 1);

        // Never consulted: not the owner, no heir, and the self-heir short-circuit.
        assert_eq!(run_death(Some(owner), Some(heir), OwnerId::new(2), true).1, 0);
        assert_eq!(run_death(Some(owner), None, owner, true).1, 0);
        assert_eq!(run_death(Some(owner), Some(heir_of(1, 3)), owner, true).1, 0);
    }

    /// The "never" half of the matrix, proved by a panicking closure.
    #[test]
    fn the_liveness_oracle_is_never_consulted_when_it_is_not_needed() {
        let owner = OwnerId::new(1);
        let heir = heir_of(9, 7);

        let mut t = new_table();
        t.set_owner(Some(owner));
        t.set_heir(Some(heir));
        assert_eq!(
            t.owner_died(OwnerId::new(2), |_| panic!("not-owner: no liveness check")),
            OwnerDeath::NotOwner
        );

        let mut t = new_table();
        t.set_owner(Some(owner));
        assert_eq!(
            t.owner_died(owner, |_| panic!("no heir: no liveness check")),
            OwnerDeath::Deleted
        );

        let mut t = new_table();
        t.set_owner(Some(owner));
        t.set_heir(Some(heir_of(owner.raw(), 3)));
        assert_eq!(
            t.owner_died(owner, |_| panic!("self-heir: no liveness check")),
            OwnerDeath::Deleted
        );
    }

    /// `OwnerDeath` is a plain value type: `Copy` + structural `Eq`/`Debug`.
    #[test]
    fn owner_death_is_a_plain_value_type() {
        let heir = heir_of(9, 7);
        let transferred = OwnerDeath::Transferred(heir);
        let copied = transferred;
        assert_eq!(copied, transferred);
        assert_ne!(transferred, OwnerDeath::Deleted);
        assert_ne!(OwnerDeath::Deleted, OwnerDeath::NotOwner);
        assert!(format!("{transferred:?}").contains("Transferred"));
        assert_eq!(transferred, OwnerDeath::Transferred(heir));
    }
}
