//! The table **ownership** surface: `ets:give_away/3` and the `{heir, ...}`
//! option of `ets:setopts/2`.
//!
//! The observable semantics mirror the oracle (`erl` OTP 29 / erts-17.1) and the
//! legacy C (`ets_give_away_3` / `ets_setopts_2` in `../../erts/emulator/beam/erl_db.c`):
//!
//! * `ets:give_away(T, NewOwner, GiftData)` transfers ownership when the caller
//!   **is** the current owner and `NewOwner` is a *different* live pid; every other
//!   case is `badarg` and leaves the owner unchanged.
//! * `ets:setopts(T, {heir, Pid})` / `ets:setopts(T, {heir, Pid, Data})` /
//!   `ets:setopts(T, {heir, none})` installs or clears the heir and requires the
//!   caller to be the owner.
//!
//! # The heir has two forms, and the difference is observable
//!
//! The oracle distinguishes the arity-2 and arity-3 `{heir, …}` options, and the
//! difference survives all the way to the owner-death transfer
//! (probed against the live `erl`; each owner installs the heir and is then
//! killed, the heir staying alive):
//!
//! | option | heir installed | owner becomes the heir | `{'ETS-TRANSFER', …}` |
//! |---|---|---|---|
//! | `{heir, Pid}` (arity 2) | yes | yes | **none** |
//! | `{heir, Pid, undefined}` (arity 3) | yes | yes | `{…, From, undefined}` |
//! | `{heir, Pid, {gift, 42}}` (arity 3) | yes | yes | `{…, From, {gift, 42}}` |
//!
//! The C says why: the option parser resolves arity 2 to the private sentinel
//! `THE_NON_VALUE` and arity 3 to `tp[3]` (`ets_setopts_2`:3181-3191,
//! `ets_new`:2675-2691), and the death path only sends the message
//! `if (is_value(heir_data))` (`give_away_to_heir`:4924-4935). `THE_NON_VALUE` is
//! not a value, so an arity-2 heir inherits the table in **silence**, while an
//! arity-3 heir *always* gets a message — `undefined` included, since that is an
//! immediate *value*.
//!
//! [`Heir::data`] models exactly that: `Some(term)` for arity 3 (delivered
//! verbatim, `undefined` included) and `None` for arity 2 (no gift data, so the
//! runtime must deliver **no** message). A host must therefore send
//! `{'ETS-TRANSFER', Tid, FromPid, data}` **only** when `data` is `Some`.
//!
//! # The pid gap (a documented modelling limit)
//!
//! This crate has **no pid term** and **no process-liveness concept**: a
//! [`OwnerId`] is an opaque `u64` (see the crate `CONTEXT.md`, Known Issues).
//! Consequently the two oracle failures that need a *process* to reason about are
//! modelled structurally rather than semantically:
//!
//! * a **non-pid** or a **dead pid** gift target is *not representable* — a `None`
//!   new owner (no live target process) maps to the `badarg` case;
//! * an **unknown table id** is the registry/BIF layer's concern (there is no
//!   [`Table`](super::Table) to call), so it never reaches this module.
//!
//! The *observable* `ets:info(T, heir)` renders a set heir the same way the crate
//! already renders `owner`: the atom `undefined` (no pid term), never the pid, and
//! it is the same for both heir forms (probed). The observable facts this crate
//! **does** match are `{heir, none}` when unset and the `true`/`badarg` returns.
//!
//! # The gift-data lifetime: install it, don't alias it
//!
//! The raw [`Table::setopts`]/[`Table::set_heir`] primitives store the caller's
//! `Eterm` **as-is**. A host that hands them a term living on a process heap
//! therefore leaves the table holding a term that dies with that heap — the same
//! aliasing hazard the insert path already closes with the
//! [`TermStore`](crate::store::TermStore) copy boundary, and which the oracle
//! closes too: the C `set_heir` deep-copies a boxed gift term into a table-owned
//! `DbTerm` (`../../erts/emulator/beam/erl_db.c` `set_heir`:5344-5364) and reclaims
//! it with `free_heir_data` (`:5367`) when the heir is replaced.
//!
//! [`Table::install_heir`] is the host-facing entry point that reproduces this: it
//! deep-copies the gift data into the table's own store (immediates are stored by
//! value, and an absent gift data owns nothing) and frees the previous copy, so the
//! [`OwnerDeath::Transferred`](super::OwnerDeath) payload carries a term that stays
//! valid for as long as the table does — exactly what the runtime needs to build the
//! `{'ETS-TRANSFER', Tid, FromPid, Data}` message.
//!
//! A **creation-time** `{heir, Pid, Data}` option is copied the same way, at build
//! time: [`TableOptions::set_heir`](crate::TableOptions::set_heir) records the raw
//! term on the options (an option builder has no store), and
//! [`Table::build_with`](super::Table) deep-copies it into the table's store through
//! the **same** [`Table::copy_heir`] helper as `install_heir`, so a creation-time
//! heir and one installed through `install_heir` behave identically downstream: the
//! data is table-owned, survives the caller's heap, and its copy is reclaimed when
//! the heir is replaced or cleared.
//!
//! # One reclaim path, whatever overwrites the heir
//!
//! Every overwrite site — [`Table::set_heir`], [`Table::setopts`]'s `{heir, …}` and
//! `{heir, none}` forms, and [`Table::install_heir`] — funnels through
//! [`Table::replace_heir`], which reclaims the outgoing heir's table-owned copy the
//! way the C `free_heir_data` does. Concentrating it in one place is what keeps a
//! host that mixes the raw primitives with `install_heir` from leaking the copy
//! until `delete_all_objects`/drop.

use beam_common::Result;
use beam_term::prelude::*;

use crate::store::TermStore;

use super::{OwnerId, Table};

/// A table heir: the process that inherits the table when its owner dies, plus
/// the gift data delivered in the `{'ETS-TRANSFER', Tab, FromPid, GiftData}`
/// message.
///
/// [`data`](Heir::data) is `Some(term)` for `{heir, Pid, Data}` (arity 3) and
/// `None` for `{heir, Pid}` (arity 2) — the two forms the oracle distinguishes by
/// whether it delivers a transfer message at all (see the [module
/// docs](self)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heir {
    /// The heir process.
    pub owner: OwnerId,
    /// The gift data (`{heir, Pid, Data}`'s `Data`), or `None` for the arity-2
    /// `{heir, Pid}` form, which carries no gift data — and for which the oracle
    /// delivers **no** `{'ETS-TRANSFER', …}` message.
    pub data: Option<Eterm>,
}

/// The parsed `{heir, ...}` option of `ets:setopts/2` (the subset this crate
/// models).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpt {
    /// `{heir, Pid}` (no gift data) or `{heir, Pid, Data}` — install the heir; see
    /// [`Heir::data`] for what the two forms mean to a host.
    Heir(Heir),
    /// `{heir, none}` — clear the heir.
    ClearHeir,
}

impl Table {
    /// Deep-copy a **raw** heir's gift data into `store` (the A.1 copy boundary),
    /// returning the [`Heir`] the table should hold.
    ///
    /// This is the **one** place the `{heir, …}` arity is interpreted for storage:
    /// the arity-2 `None` form copies nothing and owns no storage, while the
    /// arity-3 `Some(term)` form stores a table-owned copy of `term` (an immediate
    /// is stored by value, and a `data` outside the store's copyable subset — a fun,
    /// an off-heap bin ref, … — is the typed `badarg`). Both the **creation-time**
    /// option ([`Table::build_with`](super::Table)) and [`Table::install_heir`] go
    /// through it, so the two paths cannot drift.
    pub(super) fn copy_heir(store: &mut TermStore, heir: Option<Heir>) -> Result<Option<Heir>> {
        match heir {
            Some(Heir { owner, data }) => {
                let data = match data {
                    Some(term) => Some(store.copy(term)?),
                    None => None,
                };
                Ok(Some(Heir { owner, data }))
            }
            None => Ok(None),
        }
    }

    /// Install `owner` as the heir with `data` as the gift data, **deep-copying
    /// `data` into table-owned storage** (the A.1 copy boundary) when it is present.
    ///
    /// This is the host-facing heir entry point and the counterpart of the C
    /// `set_heir` (`../../erts/emulator/beam/erl_db.c:5324`), which copies a boxed
    /// gift term into a table-owned `DbTerm`. The installed term therefore stays
    /// valid for as long as the table does, whatever happens to the calling
    /// process's heap — which is what lets the runtime build the
    /// `{'ETS-TRANSFER', Tid, FromPid, Data}` message from the
    /// [`OwnerDeath::Transferred`](super::OwnerDeath) payload. The raw
    /// [`setopts`](Table::setopts)/[`set_heir`](Table::set_heir) primitives do **not**
    /// copy, so they must only ever be given a term the caller keeps alive for the
    /// table's lifetime.
    ///
    /// `data` is the option's arity: `Some(term)` is `{heir, Pid, Data}` and owns a
    /// copy of `term`; `None` is `{heir, Pid}`, which carries **no** gift data and
    /// therefore owns no storage — the runtime delivers no transfer message for it
    /// (see the [module docs](self)). Immediates (`data` needs no storage) are
    /// stored by value.
    ///
    /// The previously installed gift data — if it was a copy this table's store
    /// owns — is reclaimed first, exactly as the C `free_heir_data` (`:5367`) does;
    /// a `data` outside the store's copyable subset (a fun, an off-heap bin ref, …)
    /// returns the typed `badarg` and leaves the current heir untouched.
    pub fn install_heir(&mut self, owner: OwnerId, data: Option<Eterm>) -> Result<()> {
        let heir = Table::copy_heir(&mut self.store, Some(Heir { owner, data }))?;
        self.replace_heir(heir);
        Ok(())
    }

    /// Install `heir` (or clear it with `None`), reclaiming the outgoing heir's
    /// table-owned gift copy first — the **one** reclaim path every overwrite site
    /// funnels through, so no site can forget it (the C `free_heir_data`,
    /// `../../erts/emulator/beam/erl_db.c:5367`).
    ///
    /// The reclaim is [`TermStore::free`](crate::store::TermStore::free), which is a
    /// no-op unless the outgoing data really was a store-owned copy — so this is
    /// safe whichever primitive installed it.
    ///
    /// The outgoing copy is **not** reclaimed when the incoming heir names the very
    /// same stored root as its gift data: dropping it would leave the table holding a
    /// dangling term (`free`'s own documentation warns that a freed root is nameable
    /// again). That can only happen through the raw primitives, which install the
    /// caller's term verbatim; [`install_heir`](Table::install_heir) always copies
    /// into fresh words first.
    pub(super) fn replace_heir(&mut self, heir: Option<Heir>) {
        if let Some(previous) = self.heir {
            if let Some(old) = previous.data {
                let reuses_the_same_stored_root =
                    matches!(heir, Some(incoming) if incoming.data == Some(old));
                if !reuses_the_same_stored_root {
                    self.store.free(old);
                }
            }
        }
        self.heir = heir;
    }

    /// The installed heir's gift data — the term
    /// [`install_heir`](Table::install_heir) copied into table-owned storage — or
    /// `None`.
    ///
    /// `None` covers **both** "no heir is installed" and "the heir was installed by
    /// an arity-2 `{heir, Pid}`, so it carries no gift data at all"; use
    /// [`heir`](Table::heir) to tell the two apart. This is the `Data` a host places
    /// in `{'ETS-TRANSFER', Tid, FromPid, Data}` when it is `Some` — and `Some` is
    /// exactly when the oracle sends that message. It is table-owned, so it stays
    /// valid for as long as the table does.
    pub fn heir_data(&self) -> Option<Eterm> {
        self.heir.and_then(|heir| heir.data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    use crate::kind::TableOptions;
    use crate::table::{Table, TableId};

    fn new_table() -> Table {
        Table::new(TableId::next(), TableOptions::new()).expect("a fresh table is constructible")
    }

    /// `Heir`/`SetOpt` are plain value types: `Copy` + structural `Eq`/`Debug`.
    #[test]
    fn heir_and_set_opt_are_value_types() {
        let heir = Heir {
            owner: OwnerId::new(3),
            data: Some(make_small(42)),
        };
        let copied = heir;
        assert_eq!(copied, heir);
        assert_eq!(copied.owner, OwnerId::new(3));
        assert_eq!(copied.data, Some(make_small(42)));

        let opt = SetOpt::Heir(heir);
        assert_eq!(opt, SetOpt::Heir(copied));
        assert_ne!(opt, SetOpt::ClearHeir);
        assert!(format!("{opt:?}").contains("Heir"));
    }

    /// The arity-2 form is representable and is **not** the same heir as the
    /// arity-3 `undefined` form: `None` vs `Some(undefined-atom)`.
    #[test]
    fn the_absent_gift_data_differs_from_an_undefined_gift() {
        let owner = OwnerId::new(3);
        let arity2 = Heir { owner, data: None };
        let undefined_atom = make_atom(AtomTable::new().intern("undefined"));
        let arity3 = Heir {
            owner,
            data: Some(undefined_atom),
        };
        assert_ne!(arity2, arity3, "the two heir forms are distinguishable");
        assert_eq!(arity2.data, None);
        assert_eq!(arity3.data, Some(undefined_atom));
    }

    /// A successful transfer: the owner moves to the (different) target and the
    /// former owner loses ownership.
    #[test]
    fn give_away_transfers_ownership_to_a_different_owner() {
        let mut t = new_table();
        let from = OwnerId::new(1);
        let to = OwnerId::new(2);
        t.set_owner(Some(from));

        t.give_away(Some(from), Some(to)).expect("a valid transfer");
        assert_eq!(t.owner(), Some(to));
    }

    /// Every representable failure is `badarg` and leaves the owner **unchanged**.
    #[test]
    fn give_away_rejects_bad_callers_and_targets() {
        let mut t = new_table();
        let owner = OwnerId::new(1);
        let other = OwnerId::new(2);
        t.set_owner(Some(owner));

        // (a) caller is not the current owner.
        assert!(matches!(
            t.give_away(Some(other), Some(other)),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.owner(), Some(owner), "owner unchanged after not-owner");

        // (b) target is the caller's own pid (== current owner).
        assert!(matches!(
            t.give_away(Some(owner), Some(owner)),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.owner(), Some(owner), "owner unchanged after self-gift");

        // (c) target is `None` — the crate's stand-in for a non-pid/dead pid.
        assert!(matches!(
            t.give_away(Some(owner), None),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.owner(), Some(owner), "owner unchanged after bad pid");

        // A `None` caller (no live calling process) is likewise not the owner.
        assert!(matches!(t.give_away(None, Some(other)), Err(BeamError::BadArg)));
        assert_eq!(t.owner(), Some(owner));
    }

    /// `setopts/2` installs and clears the heir, and requires the owner.
    #[test]
    fn setopts_installs_and_clears_the_heir_for_the_owner() {
        let mut t = new_table();
        let owner = OwnerId::new(1);
        let heir_pid = OwnerId::new(9);
        t.set_owner(Some(owner));

        assert_eq!(t.heir(), None, "a fresh table has no heir");

        let opt = SetOpt::Heir(Heir {
            owner: heir_pid,
            data: Some(make_small(7)),
        });
        t.setopts(Some(owner), opt).expect("the owner may set the heir");
        assert_eq!(
            t.heir(),
            Some(Heir {
                owner: heir_pid,
                data: Some(make_small(7)),
            })
        );
        // The introspectable heir is only the pid.
        assert_eq!(t.info().heir, Some(heir_pid));

        t.setopts(Some(owner), SetOpt::ClearHeir)
            .expect("the owner may clear the heir");
        assert_eq!(t.heir(), None);
        assert_eq!(t.info().heir, None);

        // A non-owner cannot change the heir.
        assert!(matches!(
            t.setopts(Some(OwnerId::new(8)), opt),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.heir(), None, "heir unchanged after a denied setopts");
    }

    /// The owner-level primitive bypasses the owner check (it is what the runtime
    /// itself uses, e.g. when a table's owner dies and the heir steps in).
    #[test]
    fn set_heir_is_the_owner_level_primitive() {
        let mut t = new_table();
        let heir = Heir {
            owner: OwnerId::new(5),
            data: Some(make_small(1)),
        };
        t.set_heir(Some(heir));
        assert_eq!(t.heir(), Some(heir));
        t.set_heir(None);
        assert_eq!(t.heir(), None);
    }

    /// The arity-2 heir is representable end to end: `None` gift data installs and
    /// reads back, `heir_data()` is `None` while `heir()` is `Some`, and the
    /// introspected pid is unaffected.
    #[test]
    fn the_arity_two_heir_is_representable_end_to_end() {
        let mut t = new_table();
        let owner = OwnerId::new(1);
        let heir_pid = OwnerId::new(9);
        t.set_owner(Some(owner));

        t.setopts(Some(owner), SetOpt::Heir(Heir { owner: heir_pid, data: None }))
            .expect("the owner may install an arity-2 heir");
        assert_eq!(t.heir(), Some(Heir { owner: heir_pid, data: None }));
        assert_eq!(
            t.heir_data(),
            None,
            "an arity-2 heir has no gift data (and no transfer message)"
        );
        assert_eq!(t.info().heir, Some(heir_pid), "the pid is still reported");

        // …and installing it through `install_heir` copies nothing.
        let before = t.memory();
        t.install_heir(OwnerId::new(4), None)
            .expect("an absent gift data owns no storage");
        assert_eq!(t.heir(), Some(Heir { owner: OwnerId::new(4), data: None }));
        assert_eq!(t.heir_data(), None);
        assert_eq!(t.memory(), before, "nothing was allocated for the heir");
    }
}
