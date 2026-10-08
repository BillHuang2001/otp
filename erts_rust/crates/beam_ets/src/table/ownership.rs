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

//! The table **ownership** surface: `ets:give_away/3` and the `{heir, ...}`
//! option of `ets:setopts/2`.
//!
//! The observable semantics mirror the oracle (`erl` OTP 29 / erts-17.1) and the
//! legacy C (`ets_give_away_3` / `ets_setopts_2` in `../../erts/emulator/beam/erl_db.c`):
//!
//! * `ets:give_away(T, NewOwner, GiftData)` transfers ownership when the caller
//!   **is** the current owner and `NewOwner` is a *different* live pid; every other
//!   case is `badarg` and leaves the owner unchanged.
//! * `ets:setopts(T, {heir, Pid, Data})` / `ets:setopts(T, {heir, none})` installs
//!   or clears the heir and requires the caller to be the owner.
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
//! The heir is stored as a full [`Heir`] (pid + gift data) so a caller can keep
//! the `{'ETS-TRANSFER', Tab, FromPid, GiftData}` payload, but the *observable*
//! `ets:info(T, heir)` renders it the same way the crate already renders `owner`:
//! the atom `undefined` (no pid term), never the pid. The observable facts this
//! crate **does** match are `{heir, none}` when unset and the `true`/`badarg`
//! returns.

use beam_term::prelude::*;

use super::OwnerId;

/// A table heir: the process that inherits the table when its owner dies, plus
/// the gift data delivered in the `{'ETS-TRANSFER', Tab, FromPid, GiftData}`
/// message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Heir {
    /// The heir process.
    pub owner: OwnerId,
    /// The gift data (`{heir, Pid, Data}`'s `Data`).
    pub data: Eterm,
}

/// The parsed `{heir, ...}` option of `ets:setopts/2` (the subset this crate
/// models).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOpt {
    /// `{heir, Pid, Data}` — install the heir with gift data.
    Heir(Heir),
    /// `{heir, none}` — clear the heir.
    ClearHeir,
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
            data: make_small(42),
        };
        let copied = heir;
        assert_eq!(copied, heir);
        assert_eq!(copied.owner, OwnerId::new(3));
        assert_eq!(copied.data, make_small(42));

        let opt = SetOpt::Heir(heir);
        assert_eq!(opt, SetOpt::Heir(copied));
        assert_ne!(opt, SetOpt::ClearHeir);
        assert!(format!("{opt:?}").contains("Heir"));
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
            data: make_small(7),
        });
        t.setopts(Some(owner), opt).expect("the owner may set the heir");
        assert_eq!(
            t.heir(),
            Some(Heir {
                owner: heir_pid,
                data: make_small(7),
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
            data: make_small(1),
        };
        t.set_heir(Some(heir));
        assert_eq!(t.heir(), Some(heir));
        t.set_heir(None);
        assert_eq!(t.heir(), None);
    }
}
