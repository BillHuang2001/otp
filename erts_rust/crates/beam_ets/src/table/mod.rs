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

//! The [`Table`] handle, its identifiers and the internal `Interior` dispatch
//! seam.
//!
//! [`Table`] owns the table metadata (id, kind, keypos, access, owner), a
//! table-owned [`TermStore`] (the copy boundary) and one interior behind the
//! private `Interior` trait (see the `interior` module). The interior shape is
//! *not* externally pinned (workspace `CONTEXT.md` §2): a hash index and an
//! ordered index implement the same trait, so a future CHAMP/HAMT/B-tree interior
//! slots in without touching this dispatch or any dependent.
//!
//! This module keeps the metadata, the constructors and the write operations;
//! the read/navigation surface lives in the `read` module, the `Interior` seam in
//! the `interior` module.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use beam_alloc::AllocRef;
use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::info::TableInfo;
use crate::key::{key_of, object_elements};
use crate::kind::{can_read, can_write, Access, OrderedInterior, SetInterior, TableKind, TableOptions};
use crate::select::SelectPredicate;
use crate::store::TermStore;

mod access;
mod element;
mod interior;
mod lifecycle;
mod ownership;
mod read;
mod rename;
mod shared;
mod update;
mod view;

pub use access::TableAccess;
pub use lifecycle::OwnerDeath;
pub use ownership::{Heir, SetOpt, Transfer};
pub use rename::RenameOutcome;
pub use shared::SharedTable;
pub use view::{HeirView, TableRead, TableTerm, TableWrite};
pub(crate) use interior::Interior;
use interior::build_interior;
pub(crate) use update::{add_counters, counter_slot, replace_elements};

/// The process-global source of table identifiers (`ets:new/2` hands one out per
/// table). Monotonic and never reused; the identifier itself is opaque to
/// callers.
static NEXT_TABLE_ID: AtomicU64 = AtomicU64::new(1);

/// The `what` of the typed "unsupported" raise the **scalar**
/// `ets:lookup_element/3,4` produces for a multi-object kind: the oracle's answer
/// there is the element *list*, never a single element, so no scalar can be
/// returned for it (see [`Table::lookup_element_all`]).
const MULTI_ELEMENT_SCALAR: &str =
    "ets:lookup_element/3,4 on bag/duplicate_bag (the result is a list; \
     use lookup_element_all/3)";
/// The runtime identifier of a table (`ets:new/2` returns one).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TableId(u64);

impl TableId {
    /// Wrap a raw identifier.
    pub fn new(raw: u64) -> TableId {
        TableId(raw)
    }

    /// The raw identifier value.
    pub fn raw(self) -> u64 {
        self.0
    }

    /// Allocate the next process-global table identifier.
    pub fn next() -> TableId {
        TableId(NEXT_TABLE_ID.fetch_add(1, Ordering::Relaxed))
    }
}

impl fmt::Display for TableId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The owning process of a table.
///
/// An opaque identifier: this crate has no pid term and no process-liveness
/// concept. Ownership **is** enforced — [`can_read`](crate::can_read)/
/// [`can_write`](crate::can_write) compare the owner against the requester and
/// [`TableAccess`] re-checks every operation — so this is a real owner, just not
/// a BEAM pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OwnerId(u64);

impl OwnerId {
    /// Wrap a raw owner identifier.
    pub fn new(raw: u64) -> OwnerId {
        OwnerId(raw)
    }

    /// The raw owner identifier value.
    pub fn raw(self) -> u64 {
        self.0
    }
}

/// An ETS table handle: metadata plus table-owned storage and one interior.
pub struct Table {
    /// The table identifier.
    id: TableId,
    /// The parsed `ets:new/2` options. This is the **creation-time** model: the
    /// fine-locking normalization ([`TableOptions::write_concurrency`] &c.) reads
    /// [`TableOptions::access`] and is therefore never recomputed — see
    /// [`Table::access`].
    opts: TableOptions,
    /// The **current** access mode (`ets:info/2`'s `protection`).
    ///
    /// Initialised from [`TableOptions::access`] at build time and changed only by
    /// [`Table::set_access`] / [`Table::setopts`]'s `{protection, …}` option. It is
    /// deliberately *not* written back into the `opts` field: the oracle does **not**
    /// re-normalize the fine-locking flags when the protection changes at runtime,
    /// so a `public` table created with `{write_concurrency, true}` and then moved
    /// to `private` still reports `write_concurrency => true` (probed against the
    /// live `erl`), while a table *created* `private` with the same option reports
    /// `false`.
    access: Access,
    /// The owning process, if known.
    owner: Option<OwnerId>,
    /// The heir installed with `ets:setopts/2` / the creation-time `{heir, …}`
    /// option, if any. Its gift data is `Some(term)` for `{heir, Pid, Data}`
    /// (arity 3) and `None` for `{heir, Pid}` (arity 2) — see [`Heir::data`].
    heir: Option<Heir>,
    /// The table-owned term storage (the copy boundary).
    store: TermStore,
    /// Which `set` index this table uses (ignored by the other kinds). Recorded so
    /// [`delete_all_objects`](Table::delete_all_objects) rebuilds the *same*
    /// interior the table started with.
    set_interior: SetInterior,
    /// Which `ordered_set` index this table uses (ignored by the other kinds).
    /// Recorded for the same rebuild reason as [`Table::set_interior`].
    ordered_interior: OrderedInterior,
    /// The interior index dispatching the table ops.
    interior: Box<dyn Interior>,
    /// Whether the table is fixed for safe traversal
    /// (`ets:safe_fixtable(Tab, true)`). While fixed, the storage of every
    /// *deleted* object is held in [`Table::deferred_frees`] instead of being
    /// handed back to the store, so a traversal that still holds a deleted key's
    /// term never observes reclaimed (and possibly reused) words.
    fixed: bool,
    /// The roots deleted while [`Table::fixed`], whose storage is released when
    /// the table is unfixed (or by `Drop`, through the store).
    deferred_frees: Vec<Eterm>,
}

impl Table {
    /// A new table with the given identifier and options, allocating through the
    /// registered ETS allocator.
    ///
    /// A `set` table uses the crate's default `set` interior
    /// ([`SetInterior::default`]); use [`Table::with_set_interior`] to select the
    /// other one.
    pub fn new(id: TableId, opts: TableOptions) -> Result<Table> {
        Table::build(id, opts, TermStore::with_default_allocator())
    }

    /// A new table allocating its term storage through `alloc`.
    pub fn with_allocator(id: TableId, opts: TableOptions, alloc: AllocRef) -> Result<Table> {
        Table::build(id, opts, TermStore::new(alloc))
    }

    /// A new table whose **`set`** interior is chosen explicitly.
    ///
    /// The selection only affects [`TableKind::Set`]; the other kinds keep their
    /// own index (the `ordered_set` index is chosen by
    /// [`Table::with_ordered_interior`]). It exists so the two `set` interiors can
    /// be measured and cross-checked against each other; production callers use
    /// [`Table::new`], which selects [`SetInterior::default`].
    pub fn with_set_interior(
        id: TableId,
        opts: TableOptions,
        interior: SetInterior,
    ) -> Result<Table> {
        Table::build_with(
            id,
            opts,
            TermStore::with_default_allocator(),
            interior,
            OrderedInterior::default(),
        )
    }

    /// A new table whose **`ordered_set`** interior is chosen explicitly.
    ///
    /// The selection only affects [`TableKind::OrderedSet`]; the other kinds keep
    /// their own index (the `set` index is chosen by [`Table::with_set_interior`]).
    /// It exists so the two ordered interiors can be measured and cross-checked
    /// against each other; production callers use [`Table::new`], which selects
    /// [`OrderedInterior::default`].
    pub fn with_ordered_interior(
        id: TableId,
        opts: TableOptions,
        interior: OrderedInterior,
    ) -> Result<Table> {
        Table::build_with(
            id,
            opts,
            TermStore::with_default_allocator(),
            SetInterior::default(),
            interior,
        )
    }

    /// The shared constructor: assemble the metadata, the store and the interior
    /// for `opts`, with the default `set` and `ordered_set` interiors.
    ///
    /// A creation-time `{heir, Pid, Data}` option installs the initial heir, whose
    /// gift data is deep-copied into the table's store (see
    /// [`build_with`](Table::build_with)).
    fn build(id: TableId, opts: TableOptions, store: TermStore) -> Result<Table> {
        Table::build_with(
            id,
            opts,
            store,
            SetInterior::default(),
            OrderedInterior::default(),
        )
    }

    /// The shared constructor with explicit interior selections.
    fn build_with(
        id: TableId,
        opts: TableOptions,
        mut store: TermStore,
        set_interior: SetInterior,
        ordered_interior: OrderedInterior,
    ) -> Result<Table> {
        let interior = build_interior(
            opts.kind(),
            set_interior,
            ordered_interior,
            opts.keypos(),
        );
        // A creation-time `{heir, Pid, Data}` option (if any) is the table's
        // initial heir; `ets:setopts/2` replaces it later. Its gift data is
        // deep-copied into the table's own store through the same helper as
        // `install_heir`, so it is table-owned (it survives the caller's heap being
        // dropped) and its copy is reclaimed when the heir is replaced or cleared.
        let heir = Table::copy_heir(&mut store, opts.heir())?;
        Ok(Table {
            id,
            access: opts.access(),
            opts,
            owner: None,
            heir,
            store,
            set_interior,
            ordered_interior,
            interior,
            fixed: false,
            deferred_frees: Vec::new(),
        })
    }

    /// The `set` interior this table was built with (ignored by the other kinds,
    /// which have a single index each).
    pub fn set_interior(&self) -> SetInterior {
        self.set_interior
    }

    /// The `ordered_set` interior this table was built with (ignored by the other
    /// kinds, which have a single index each).
    pub fn ordered_interior(&self) -> OrderedInterior {
        self.ordered_interior
    }
    /// The table identifier.
    pub fn id(&self) -> TableId {
        self.id
    }

    /// The table kind.
    pub fn kind(&self) -> TableKind {
        self.opts.kind()
    }

    /// The registered table name, if any.
    ///
    /// Independent of `named_table`: a name is recorded for every table the
    /// `ets:new/2` option parser named, whether or not `named_table` was given.
    pub fn name(&self) -> Option<Atom> {
        self.opts.name()
    }

    /// The 1-based key position.
    pub fn keypos(&self) -> usize {
        self.opts.keypos()
    }

    /// The **current** access mode (`ets:info/2`'s `protection`).
    ///
    /// Initially the creation-time [`TableOptions::access`] and changed at runtime
    /// by [`set_access`](Table::set_access) / [`setopts`](Table::setopts). Note the
    /// fine-locking flags are *not* affected: they keep the creation-time
    /// normalization recorded in the `opts` field.
    pub fn access(&self) -> Access {
        self.access
    }

    /// Set the access mode directly (**owner-level primitive**; no access check).
    ///
    /// This is the primitive behind `ets:setopts(Tab, {protection, Access})`, the
    /// counterpart of [`set_owner`](Table::set_owner)/[`set_heir`](Table::set_heir):
    /// the BIF layer routes its access-checked form through
    /// [`setopts`](Table::setopts) (or [`TableAccess::set_access`](TableAccess)) so
    /// the owner gate is applied.
    ///
    /// **Only the access mode changes.** The creation-time fine-locking
    /// normalization in [`TableOptions`] is left untouched, exactly like the oracle:
    /// `ets:setopts(T, {protection, private})` on a `public` table created with
    /// `{write_concurrency, true}` leaves `ets:info(T, write_concurrency)` and
    /// `ets:info(T, decentralized_counters)` at `true`, whereas a table *created*
    /// `private` with that option reports `false` (both probed against the live
    /// `erl`).
    pub fn set_access(&mut self, access: Access) {
        self.access = access;
    }

    /// The owning process, if known.
    pub fn owner(&self) -> Option<OwnerId> {
        self.owner
    }

    /// Set (or clear) the owning process.
    pub fn set_owner(&mut self, owner: Option<OwnerId>) {
        self.owner = owner;
    }

    /// The three C-ordered `ets:give_away/3` checks, in one place: a bad/absent
    /// target (`new_owner` is `None` — the crate's stand-in for the oracle's
    /// non-pid/dead-pid target), then a caller that is not the current owner, then a
    /// **self-gift** (`new_owner` is the current owner). Each is `badarg` and
    /// mutates nothing.
    ///
    /// Returns the validated target so [`give_away`](Table::give_away) and
    /// [`give_away_with_gift`](Table::give_away_with_gift) share exactly this order
    /// (the C `ets_give_away_3`).
    fn transfer_target(
        &self,
        caller: Option<OwnerId>,
        new_owner: Option<OwnerId>,
    ) -> Result<OwnerId> {
        // Mirror the C `ets_give_away_3` order: bad-pid check, then the
        // not-owner check, then the self-gift check — all `badarg`.
        let Some(new_owner) = new_owner else {
            return Err(badarg());
        };
        if caller != self.owner {
            return Err(badarg());
        }
        if Some(new_owner) == self.owner {
            return Err(badarg());
        }
        Ok(new_owner)
    }

    /// `ets:give_away/3`. Transfers ownership from `caller` to `new_owner`.
    ///
    /// `badarg` (owner unchanged) when `caller` is not the current owner, when
    /// `new_owner` is `None` (no live target process — the crate's stand-in for the
    /// oracle's non-pid/dead-pid target), or when `new_owner` is the current owner
    /// (the caller's own pid). On success the owner becomes `new_owner`.
    ///
    /// This is the **no-gift** form. The gift-carrying
    /// `ets:give_away(T, NewOwner, GiftData)` is
    /// [`give_away_with_gift`](Table::give_away_with_gift), which additionally
    /// deep-copies the gift into table-owned storage and surfaces the
    /// `{'ETS-TRANSFER', Tid, FromPid, GiftData}` the host owes; the actual message
    /// *delivery* is the BIF/runtime layer's job, not this crate's (see
    /// [`super::ownership`]).
    pub fn give_away(
        &mut self,
        caller: Option<OwnerId>,
        new_owner: Option<OwnerId>,
    ) -> Result<()> {
        let new_owner = self.transfer_target(caller, new_owner)?;
        self.owner = Some(new_owner);
        Ok(())
    }

    /// `ets:give_away/3` **with the gift term**: transfers ownership and hands back
    /// the [`Transfer`] the host must deliver as
    /// `{'ETS-TRANSFER', Tid, FromPid, GiftData}` to the new owner.
    ///
    /// The ownership rules are identical to [`give_away`](Table::give_away) (the
    /// same [`transfer_target`](Table::transfer_target) check: a bad/absent target,
    /// a caller that is not the owner and a self-gift are each `badarg`). The gift
    /// is then deep-copied into the table's own store through the
    /// [`TermStore`](crate::store::TermStore) copy boundary — the A.1 seam
    /// [`take`](Table::take) uses — and checked out exactly like a `take` result:
    /// still valid for the table's lifetime and never handed out again, but no
    /// longer counted toward [`memory`](Table::memory). An immediate gift owns no
    /// storage, so it is stored by value.
    ///
    /// **The copy happens before the owner moves**, so a gift outside the store's
    /// copyable subset (see the crate `CONTEXT.md`, Known Issues) is the typed
    /// `badarg` with the owner and the store left untouched.
    ///
    /// The `Tid` and `FromPid` **terms** are the host's to build, never this
    /// crate's: it has no pid/ref term (the documented "pid gap"). [`Transfer::to`]
    /// names the recipient, [`Transfer::from`] the giver and [`Transfer::data`] the
    /// copied gift.
    #[must_use = "the host must deliver {'ETS-TRANSFER', Tid, FromPid, GiftData} to `to`"]
    pub fn give_away_with_gift(
        &mut self,
        caller: Option<OwnerId>,
        new_owner: Option<OwnerId>,
        gift: Eterm,
    ) -> Result<Transfer> {
        let new_owner = self.transfer_target(caller, new_owner)?;
        // Copy into table-owned storage **before** mutating the owner: a copy
        // failure must leave the owner (and the store) unchanged.
        let data = self.store.copy(gift)?;
        self.store.relinquish(data);
        self.owner = Some(new_owner);
        Ok(Transfer {
            to: new_owner,
            from: caller,
            data,
        })
    }

    /// The table's current heir, if any. Its `data` field distinguishes the
    /// oracle's two `{heir, …}` forms: `Some(term)` for `{heir, Pid, Data}`
    /// (arity 3), `None` for `{heir, Pid}` (arity 2, no gift data — and therefore
    /// no `{'ETS-TRANSFER', …}` message on owner death).
    pub fn heir(&self) -> Option<Heir> {
        self.heir
    }

    /// Set (or clear) the heir directly (owner-level primitive; no access check).
    ///
    /// Reclaiming the outgoing heir's table-owned gift copy is **not** the caller's
    /// job: the primitive funnels through the same
    /// [`replace_heir`](Table::replace_heir) reclaim path as
    /// [`install_heir`](Table::install_heir) and [`setopts`](Table::setopts), so no
    /// overwrite site leaks the copy (the C `free_heir_data`,
    /// `../../erts/emulator/beam/erl_db.c:5367`).
    ///
    /// Unlike [`install_heir`](Table::install_heir) it does **not** copy the gift
    /// data, so a non-immediate `heir.data` must be a term the caller keeps alive
    /// for the table's lifetime.
    pub fn set_heir(&mut self, heir: Option<Heir>) {
        self.replace_heir(heir);
    }

    /// `ets:setopts/2` for the modelled options: requires `caller` to be the
    /// current owner (`badarg` otherwise), then applies the option.
    ///
    /// `SetOpt::Heir` carries the option's arity in [`Heir::data`]: `Some(term)` for
    /// `{heir, Pid, Data}` and `None` for `{heir, Pid}`. Like the primitive it
    /// reclaims the outgoing heir's copy, and like the primitive it does **not** copy
    /// the incoming gift data (prefer [`install_heir`](Table::install_heir)).
    ///
    /// `SetOpt::Access` is `{protection, Access}`: it moves the table's *current*
    /// access mode ([`set_access`](Table::set_access)) and nothing else — the
    /// creation-time concurrency normalization is deliberately left alone, matching
    /// the oracle (see [`SetOpt::Access`] and [`Table::set_access`]).
    ///
    /// The owner gate runs **before** any mutation, so a denied call changes
    /// nothing.
    pub fn setopts(&mut self, caller: Option<OwnerId>, opt: SetOpt) -> Result<()> {
        if caller != self.owner {
            return Err(badarg());
        }
        match opt {
            SetOpt::Heir(heir) => self.replace_heir(Some(heir)),
            SetOpt::ClearHeir => self.replace_heir(None),
            SetOpt::Access(access) => self.set_access(access),
        }
        Ok(())
    }

    /// A snapshot of the table's introspection fields.
    pub fn info(&self) -> TableInfo {
        TableInfo {
            id: self.id,
            name: self.opts.name(),
            kind: self.opts.kind(),
            keypos: self.opts.keypos(),
            access: self.access,
            size: self.interior.size(),
            memory: self.memory(),
            owner: self.owner,
            heir: self.heir.map(|h| h.owner),
            named_table: self.opts.is_named(),
            compressed: self.opts.is_compressed(),
            write_concurrency: self.opts.write_concurrency(),
            read_concurrency: self.opts.read_concurrency(),
            decentralized_counters: self.opts.decentralized_counters(),
        }
    }

    /// Insert `object` (the whole tuple; the key is derived from `keypos`).
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        let stored = self.store_object(object)?;
        self.insert_stored(stored, atoms)
    }

    /// Validate `object` against the table's `keypos` and deep-copy it into
    /// table-owned storage (the A.1 copy boundary), returning the stored root.
    ///
    /// This is the whole fallible half of [`insert`](Table::insert): a non-tuple /
    /// too-short object is `badarg` **before** anything is allocated, and an object
    /// outside the store's supported subset (a fun, an external pid, …) rolls its
    /// copy back. Neither the index nor the store is left changed on failure, which
    /// is what lets [`insert_list`](Table::insert_list) validate a whole list
    /// before mutating anything.
    fn store_object(&mut self, object: Eterm) -> Result<Eterm> {
        key_of(object, self.opts.keypos())?;
        self.store.copy(object)
    }

    /// Insert an object that is **already** table-owned ([`store_object`]'s
    /// result): derive the stored key, apply the kind's duplicate rules, and
    /// reclaim whatever the insert replaced or rejected.
    ///
    /// Infallible for this crate's interiors (they answer with the retained flag,
    /// never an error); it returns `Result` only because [`Interior::insert`] does.
    ///
    /// [`store_object`]: Table::store_object
    fn insert_stored(&mut self, stored: Eterm, atoms: &AtomTable) -> Result<()> {
        let stored_key = key_of(stored, self.opts.keypos())?;
        // A single-object kind (`set`/`ordered_set`) *replaces* the object stored
        // under an existing key. Look the replaced root up **before** the insert so
        // its storage can be reclaimed afterwards (the multi kinds only ever add,
        // so there is nothing to reclaim).
        let replaced = if self.opts.kind().is_multi() {
            Vec::new()
        } else {
            self.interior.lookup(stored_key, atoms)
        };
        let retained = self.interior.insert(stored_key, stored, atoms)?;
        if !retained {
            // The interior rejected the copy as a duplicate no-op (a `bag` holding
            // an identical object): no entry references it, so reclaim it instead of
            // leaving it orphaned in the store. The copy was never *in* the table, so
            // it is released even while the table is fixed — no traversal can hold it.
            self.store.free(stored);
        }
        for old in replaced {
            // The replaced object *was* in the table (and its stored key term may be
            // the very term a fixed traversal is holding), so its storage is deferred
            // while the table is fixed.
            self.free_object(old);
        }
        Ok(())
    }

    /// Insert every object of `objects` (`ets:insert/2` over a list).
    ///
    /// **Atomic**, exactly like the oracle: `ets:insert(Tab, List)` validates the
    /// whole list first, so if **any** element is not a valid object for the
    /// table's `keypos` (a non-tuple, or a tuple shorter than `keypos`) the call is
    /// `badarg` and **none** of the elements is inserted — not even the valid ones
    /// that precede the bad one. Verified against the system `erl` (OTP 29 /
    /// erts-17.1): with `{1,10}` in `T`, `ets:insert(T, [{2,20}, foo])` raises
    /// `badarg` and `ets:tab2list(T)` is still `[{1,10}]`. An empty list inserts
    /// nothing and succeeds.
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()> {
        // Validate and copy every object **before** the first index mutation, so a
        // bad element cannot leave a half-applied list behind.
        let mut stored = Vec::with_capacity(objects.len());
        for &object in objects {
            match self.store_object(object) {
                Ok(one) => stored.push(one),
                Err(err) => {
                    // Nothing has been inserted: release the copies made so far and
                    // report, leaving the table exactly as it was.
                    for one in stored {
                        self.store.free(one);
                    }
                    return Err(err);
                }
            }
        }
        // Every object is valid and table-owned; insert them in list order.
        for one in stored {
            self.insert_stored(one, atoms)?;
        }
        Ok(())
    }
    /// Insert `object` only if its key is absent; returns whether it was
    /// inserted (`false` when the key already exists).
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        let keypos = self.opts.keypos();
        key_of(object, keypos)?;
        let stored = self.store.copy(object)?;
        let stored_key = key_of(stored, keypos)?;
        let inserted = self.interior.insert_new(stored_key, stored, atoms)?;
        if !inserted {
            // The key already exists, so the interior stored nothing: the copy is
            // unreferenced (never a table object) and is reclaimed immediately.
            self.store.free(stored);
        }
        Ok(inserted)
    }

    /// Delete every object stored under `key` (`ets:delete/2`).
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        // Capture the objects about to be removed so their table-owned storage is
        // reclaimed (a `set`/`ordered_set` entry holds one object, a `bag`/
        // `duplicate_bag` entry may hold many).
        let removed = self.interior.lookup(key, atoms);
        self.interior.delete(key, atoms)?;
        let removed_something = !removed.is_empty();
        for object in removed {
            self.free_object(object);
        }
        if removed_something {
            self.release_empty_interior();
        }
        Ok(())
    }

    /// Delete the single object matching `object` (`ets:delete_object/2`).
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        // The key is only used to locate the entry; the object itself is compared.
        let key = key_of(object, self.opts.keypos())?;
        // Capture exactly the objects the interior will remove — the stored objects
        // under `key` that equal `object` — so their storage is reclaimed afterwards.
        let removed: Vec<Eterm> = self
            .interior
            .lookup(key, atoms)
            .into_iter()
            .filter(|&stored| eq(stored, object, atoms))
            .collect();
        self.interior.delete_object(key, object, atoms)?;
        let removed_something = !removed.is_empty();
        for stored in removed {
            self.free_object(stored);
        }
        if removed_something {
            self.release_empty_interior();
        }
        Ok(())
    }

    /// Release the index's own storage once the last object has left the interior,
    /// so a table emptied one object at a time returns to **the same footprint** as
    /// one emptied by [`delete_all_objects`](Table::delete_all_objects) — and to the
    /// empty-table figure the oracle reports.
    ///
    /// Verified against the live system `erl` (OTP 29 / erts-17.1): `ets:info(T,
    /// memory)` returns *exactly* to the empty-table figure after the key set has
    /// been deleted (`set`/`bag`/`duplicate_bag` 306 words, `ordered_set` 140 words —
    /// the same figures as a freshly created table), so the oracle keeps no index
    /// storage for an empty table. The interiors' `memory` counts the index's
    /// *capacity* (a CHAMP root's slot vectors, a chained index's bucket array, the
    /// ordered arena's nodes), which only a rebuilt interior gives back.
    ///
    /// Only a removal that actually happened calls this, so a `delete` of an absent
    /// key — a common no-op — never churns the index.
    fn release_empty_interior(&mut self) {
        if self.interior.size() == 0 {
            self.interior = build_interior(
                self.opts.kind(),
                self.set_interior,
                self.ordered_interior,
                self.opts.keypos(),
            );
        }
    }
    /// Replace **every** object stored under `key` with `objects`, reclaiming the
    /// replaced objects' storage.
    ///
    /// This is the storage-faithful rebuild behind `ets:select_replace/2` on a
    /// `duplicate_bag` (see [`crate::select`]): several objects may share a key and
    /// a replacement must land on the *specific* matched object, so the key's entry
    /// is rebuilt rather than overwritten.
    ///
    /// Unlike [`Table::take`] — which *checks out* its copies and leaves their
    /// storage held for the table's lifetime — this path copies the new objects
    /// into table storage **first**, then frees the replaced objects through the
    /// ordinary removal path, so repeated replacement keeps the table's storage
    /// bounded.
    ///
    /// `objects` are the caller's terms (usually built on its scratch heap): every
    /// one is validated against `keypos` and deep-copied into table storage before
    /// any mutation, so a rejected object leaves the table unchanged (the existing
    /// entry intact). The copies are installed through the already-stored path, so
    /// the index key aliases the new table-owned object exactly like an `insert`.
    pub(crate) fn replace_key(
        &mut self,
        key: Eterm,
        objects: &[Eterm],
        atoms: &AtomTable,
    ) -> Result<()> {
        // Phase 1 — validate and deep-copy the whole new object list into table
        // storage *before* anything is removed, so the copy never reads storage the
        // subsequent removal reclaims.
        let mut stored = Vec::with_capacity(objects.len());
        for &object in objects {
            match self.store_object(object) {
                Ok(one) => stored.push(one),
                Err(err) => {
                    // Nothing has been removed yet: release the copies made so far
                    // and report, leaving the key's entry exactly as it was.
                    for one in stored {
                        self.store.free(one);
                    }
                    return Err(err);
                }
            }
        }
        // Phase 2 — remove the key's old objects and reclaim their storage.
        self.delete(key, atoms)?;
        // Phase 3 — install the already-copied roots (no second deep copy).
        for one in stored {
            self.insert_stored(one, atoms)?;
        }
        Ok(())
    }

    /// Delete every object in the table (`ets:delete_all_objects/1`).
    pub fn delete_all_objects(&mut self) {
        if self.fixed {
            // **Fixed-table semantics** (verified against the system `erl`, OTP 29 /
            // erts-17.1): the objects leave the index — `ets:info(Tab, size)` is `0`
            // — but their storage stays bundled with the arena a traversal is
            // walking, so `ets:info(Tab, memory)` does *not* drop until the table is
            // unfixed. Every stored object is therefore deferred (its storage stays
            // live and is never handed out again) and the index is rebuilt empty.
            let objects: Vec<Eterm> = self.interior.iter_objects().collect();
            self.deferred_frees.extend(objects);
            self.interior = build_interior(
                self.opts.kind(),
                self.set_interior,
                self.ordered_interior,
                self.opts.keypos(),
            );
            return;
        }
        // Not fixed: `reset` releases every block the store owns (so the per-object
        // runs of the deleted objects go with them) and leaves the store immediately
        // usable again; a fresh interior of the same kind and `set` shape is then
        // empty.
        self.store.reset();
        self.interior = build_interior(
            self.opts.kind(),
            self.set_interior,
            self.ordered_interior,
            self.opts.keypos(),
        );
    }

    /// `ets:safe_fixtable/2`: set (or clear) the table's *fix* flag.
    ///
    /// While fixed, the storage of every object removed from the table is held
    /// rather than reclaimed, so a traversal that began before the removal can keep
    /// reading the deleted object's key term without ever seeing reclaimed (or
    /// reused) words. Clearing the flag releases everything held, which is exactly
    /// the oracle's observable memory step (verified: memory *rises* by the deleted
    /// objects' storage while fixed and *falls* at
    /// `ets:safe_fixtable(Tab, false)`).
    ///
    /// The `Bool` argument's type check (`badarg` for a non-boolean) is the BIF
    /// layer's; this is the parsed effect. Like the oracle it is idempotent: fixing
    /// an already-fixed table (or clearing an unfixed one) is a no-op.
    pub fn set_fixation(&mut self, fixed: bool) {
        if fixed == self.fixed {
            return;
        }
        self.fixed = fixed;
        if !fixed {
            // Unfixing releases every held root's storage.
            let held = core::mem::take(&mut self.deferred_frees);
            for root in held {
                self.store.free(root);
            }
        }
    }

    /// Whether the table is currently fixed (`ets:safe_fixtable/2`).
    pub fn is_fixed(&self) -> bool {
        self.fixed
    }

    /// How many removed objects' storage the table is currently holding because it
    /// is fixed. Zero on an unfixed table; the count drops to zero at
    /// [`set_fixation`](Table::set_fixation)`(false)`.
    pub fn deferred_objects(&self) -> usize {
        self.deferred_frees.len()
    }

    /// Release the table-owned storage of an object that has just been **removed**
    /// from the table.
    ///
    /// Unfixed, this is an immediate [`TermStore::free`]. Fixed, the root's storage
    /// is held in [`Table::deferred_frees`] until the table is unfixed, so a
    /// traversal holding a term that lived inside the removed object cannot observe
    /// reclaimed words.
    fn free_object(&mut self, root: Eterm) {
        if self.fixed {
            self.deferred_frees.push(root);
        } else {
            self.store.free(root);
        }
    }

    /// Whether `requester` may **read** the table; an access violation is
    /// `BeamError::BadArg`, matching how `ets` reports it.
    pub fn check_read(&self, requester: Option<OwnerId>) -> Result<()> {
        if can_read(self.access, self.owner, requester) {
            Ok(())
        } else {
            Err(badarg())
        }
    }

    /// Whether `requester` may **write** the table; an access violation is
    /// `BeamError::BadArg`, matching how `ets` reports it.
    pub fn check_write(&self, requester: Option<OwnerId>) -> Result<()> {
        if can_write(self.access, self.owner, requester) {
            Ok(())
        } else {
            Err(badarg())
        }
    }

    /// An access-enforcing view of this table for the process `requester`.
    ///
    /// The [`Table`] methods are owner-level primitives — the operations the
    /// table's own runtime performs — and do not consult the access mode. A BIF
    /// acting for a specific process wraps the table in a [`TableAccess`], which
    /// rechecks `kind::can_read`/`can_write` before every operation and raises
    /// `BeamError::BadArg` on a violation, exactly like `ets`.
    pub fn as_requester(&mut self, requester: Option<OwnerId>) -> TableAccess<'_> {
        TableAccess {
            table: self,
            requester,
        }
    }

    /// Every object whose element slice `pred` accepts (the M1 select
    /// primitive).
    pub fn select_all(&self, pred: &mut SelectPredicate<'_>) -> Vec<Eterm> {
        let mut selected = Vec::new();
        for object in self.interior.iter_objects() {
            // SAFETY: every object in a table is a tuple validated by `key_of` on
            // insert (and the deep copy preserves that), so its header word is a
            // live, valid arity value; the returned slice borrows table storage,
            // which outlives this borrow.
            let elements = unsafe { object_elements(object) };
            if pred(elements) {
                selected.push(object);
            }
        }
        selected
    }

}
#[cfg(test)]
mod tests;
