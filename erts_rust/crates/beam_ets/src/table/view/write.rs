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

//! [`TableWrite`] — an exclusive guard that mutates a table and tags anything it
//! hands back.
//!
//! Mutations take **caller-owned** input terms, so nothing a caller passes in can
//! alias table storage. The operations that need a term-construction arena
//! (`update_counter`/`update_element`, and the mutating select family) take the
//! caller's own [`Heap`], exactly like the raw [`Table`] API — and their results
//! live on that caller heap (or are immediates), never in table storage, so they
//! are safe to own. [`take`](TableWrite::take) is the one exception: it hands back
//! table storage (checked out, never reused, but released by
//! `delete_all_objects`/drop), so its results stay
//! [tagged](super::TableTerm). See the [module docs](super) for the hazard.

use std::sync::RwLockWriteGuard;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::TableTerm;
use crate::info::TableInfo;
use crate::kind::{Access, TableKind};
use crate::select::MatchSpec;
use crate::table::{Heir, OwnerId, SetOpt, Table, TableAccess, TableId};

/// An exclusive guard over a [`Table`]: the synchronous writer half of a
/// [`SharedTable`](super::super::SharedTable).
///
/// Obtained from [`SharedTable::write`](super::super::SharedTable::write) or
/// [`SharedTable::with_write`](super::super::SharedTable::with_write). Every
/// operation takes `&mut self`, so a whole check-then-act sequence runs inside one
/// exclusive lock hold and no two writers interleave (`insert_new`,
/// `update_counter`, `select_replace`, …).
pub struct TableWrite<'g> {
    /// The write guard. Holding it is what makes the tagged terms valid.
    guard: RwLockWriteGuard<'g, Table>,
}

impl<'g> TableWrite<'g> {
    /// Wrap a freshly acquired write guard.
    ///
    /// `pub(in crate::table)`: the sibling [`SharedTable`](super::super::SharedTable)
    /// is the only caller, and keeping this out of the public API keeps the tagged
    /// terms coming from exactly one place.
    pub(in crate::table) fn new(guard: RwLockWriteGuard<'g, Table>) -> TableWrite<'g> {
        TableWrite { guard }
    }

    // ------------------------------------------------------------------ metadata

    /// The table identifier.
    pub fn id(&self) -> TableId {
        self.guard.id()
    }

    /// The table kind.
    pub fn kind(&self) -> TableKind {
        self.guard.kind()
    }

    /// The 1-based key position.
    pub fn keypos(&self) -> usize {
        self.guard.keypos()
    }

    /// The access mode.
    pub fn access(&self) -> Access {
        self.guard.access()
    }

    /// The owning process, if known.
    pub fn owner(&self) -> Option<OwnerId> {
        self.guard.owner()
    }

    /// The installed heir, if any. Its [`data`](Heir::data) is `Some(term)` for an
    /// arity-3 `{heir, Pid, Data}` and `None` for an arity-2 `{heir, Pid}`.
    pub fn heir(&self) -> Option<Heir> {
        self.guard.heir()
    }

    /// The registered table name, if any (independent of `named_table`).
    pub fn name(&self) -> Option<Atom> {
        self.guard.name()
    }

    /// Whether the table was created with `named_table`.
    pub fn named_table(&self) -> bool {
        self.guard.info().named_table
    }

    /// The number of objects in the table.
    pub fn size(&self) -> usize {
        self.guard.size()
    }

    /// Whether the table holds no objects.
    pub fn is_empty(&self) -> bool {
        self.guard.is_empty()
    }

    /// The words allocated to the table.
    pub fn memory(&self) -> usize {
        self.guard.memory()
    }

    /// A snapshot of the table's introspection fields.
    pub fn info(&self) -> TableInfo {
        self.guard.info()
    }

    /// Whether the table is fixed.
    pub fn is_fixed(&self) -> bool {
        self.guard.is_fixed()
    }

    /// An access-enforcing view over the table for `requester`, which rechecks the
    /// table's `protection` on every operation (read **and** write) and raises
    /// `badarg` on a violation.
    ///
    /// **Residual hazard.** [`TableAccess`] is the *raw* table view: it still hands
    /// back bare `Eterm`s that alias table storage, so a result read through it can
    /// outlive the guard. The returned value borrows `self`, so the guard cannot be
    /// dropped while it is alive — but the terms it yields can be copied out of that
    /// scope. Prefer the tagged reads on [`TableRead`](super::TableRead); this method
    /// exists so an access-checked operation is reachable from a view at all.
    pub fn as_requester(&mut self, requester: Option<OwnerId>) -> TableAccess<'_> {
        self.guard.as_requester(requester)
    }

    // -------------------------------------------------------------- insert / delete

    /// Insert `object` (upsert). `object` is the caller's own term; it is
    /// deep-copied into table storage (the A.1 copy boundary).
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        self.guard.insert(object, atoms)
    }

    /// Insert `object` only if its key is absent; returns whether it was inserted.
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        self.guard.insert_new(object, atoms)
    }

    /// Insert every object of `objects` atomically.
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()> {
        self.guard.insert_list(objects, atoms)
    }

    /// Delete every object stored under `key`.
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        self.guard.delete(key, atoms)
    }

    /// Delete the single object matching `object`.
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        self.guard.delete_object(object, atoms)
    }

    /// Delete every object in the table.
    pub fn delete_all_objects(&mut self) {
        self.guard.delete_all_objects()
    }

    // -------------------------------------------------------------- element updates
    //
    // The results are built on the caller's `out` heap (or are immediates), never
    // in table storage — so, like the raw `Table` methods, they are safe to own.
    // `out` must therefore outlive every value read out of it.

    /// `ets:update_counter/3`: add the increment to the element following the key
    /// and return the new value. A list operand yields a list built on `out`.
    pub fn update_counter(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        atoms: &AtomTable,
        out: &mut Heap,
    ) -> Result<Eterm> {
        self.guard.update_counter(key, update_op, atoms, out)
    }

    /// `ets:update_counter/4`: like [`update_counter`](Self::update_counter) with a
    /// `default` object used when `key` is absent.
    pub fn update_counter_with(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        default: Eterm,
        atoms: &AtomTable,
        out: &mut Heap,
    ) -> Result<Eterm> {
        self.guard.update_counter_with(key, update_op, default, atoms, out)
    }

    /// `ets:update_element/3`: replace element `pos` of the object stored under
    /// `key` with each `Value`, returning whether `key` was present.
    pub fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        atoms: &AtomTable,
        out: &mut Heap,
    ) -> Result<bool> {
        self.guard.update_element(key, updates, atoms, out)
    }

    // ---------------------------------------------------------------- select family

    /// `ets:select_delete/2`: delete every object whose clause value is `true`,
    /// returning how many were deleted.
    pub fn select_delete(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        out: &mut Heap,
    ) -> Result<usize> {
        self.guard.select_delete(spec, atoms, out)
    }

    /// `ets:match_delete/2`: delete every object matching the pattern, returning
    /// how many were deleted (the BIF reports `true`).
    pub fn match_delete(
        &mut self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        out: &mut Heap,
    ) -> Result<usize> {
        self.guard.match_delete(pattern_term, atoms, out)
    }

    /// `ets:select_replace/2`: replace every object the spec selects, returning how
    /// many were replaced.
    pub fn select_replace(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        out: &mut Heap,
    ) -> Result<usize> {
        self.guard.select_replace(spec, atoms, out)
    }

    // ------------------------------------------------------------------- `take`

    /// `ets:take/2`: remove `key`'s entry and hand its objects back.
    ///
    /// The objects are **checked out** of the table's store: their words are never
    /// handed out again, so they cannot be silently overwritten by a later insert —
    /// but they are still table storage, released by `delete_all_objects`/drop.
    /// They are therefore returned as lifetime-tagged views; use
    /// [`copy_into`](TableTerm::copy_into) to own one beyond the guard.
    pub fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<TableTerm<'_>>> {
        Ok(self
            .guard
            .take(key, atoms)?
            .into_iter()
            .map(|term| TableTerm::new(term))
            .collect())
    }

    // ---------------------------------------------------------------- fixation

    /// `ets:safe_fixtable/2`: set (or clear) the table's fix flag.
    pub fn set_fixation(&mut self, fixed: bool) {
        self.guard.set_fixation(fixed)
    }

    // ---------------------------------------------------------------- ownership

    /// Set (or clear) the owning process directly (owner-level primitive).
    pub fn set_owner(&mut self, owner: Option<OwnerId>) {
        self.guard.set_owner(owner)
    }

    /// Set (or clear) the heir directly (owner-level primitive).
    ///
    /// The outgoing heir's table-owned gift copy is reclaimed by the same shared
    /// path the raw table uses, so a view mixing this with `set_heir` does not leak
    /// (see [`Table::set_heir`]). The new heir's [`data`](Heir::data) is stored
    /// verbatim — `Some(term)` for `{heir, Pid, Data}`, `None` for the arity-2
    /// `{heir, Pid}` — and is **not** copied into table storage.
    pub fn set_heir(&mut self, heir: Option<Heir>) {
        self.guard.set_heir(heir)
    }

    /// `ets:give_away/3`, with `caller` supplied explicitly (the ownership rules
    /// live in [`Table::give_away`]).
    pub fn give_away(
        &mut self,
        caller: Option<OwnerId>,
        new_owner: Option<OwnerId>,
    ) -> Result<()> {
        self.guard.give_away(caller, new_owner)
    }

    /// `ets:setopts/2` for the modelled options (`{heir, ...}` and
    /// `{protection, Access}`), with `caller` supplied explicitly.
    pub fn setopts(&mut self, caller: Option<OwnerId>, opt: SetOpt) -> Result<()> {
        self.guard.setopts(caller, opt)
    }

    /// Set the current access mode directly (owner-level primitive, no gate).
    ///
    /// The access-checked `ets:setopts(Tab, {protection, Access})` form is
    /// [`TableWrite::setopts`] with [`SetOpt::Access`]; see [`Table::set_access`]
    /// for why the creation-time fine-locking flags are unaffected.
    pub fn set_access(&mut self, access: Access) {
        self.guard.set_access(access)
    }
}
