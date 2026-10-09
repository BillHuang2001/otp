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

//! The access-enforcing table view: [`TableAccess`] rechecks the table's
//! `protection` against a requester process before every operation.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::{OwnerId, RenameOutcome, SetOpt, Table};
use crate::kind::Access;
use crate::select::{MatchContinuation, MatchSpec};

/// An access-enforcing view over a [`Table`] for a specific requester process.
///
/// Obtained from [`Table::as_requester`]. Every operation first rechecks the
/// table's `protection` against the requester (`kind::can_read`/`can_write`) and
/// raises `BeamError::BadArg` on a violation, exactly like `ets`. The
/// underlying [`Table`] methods remain the owner-level, non-checking primitives.
///
/// The whole observable `ets` surface is routed through this view: the
/// read/write split (`protected` admits any reader but only the owner writer,
/// `private` admits only the owner, `public` admits anyone) is verified against
/// the system `erl` (OTP 29 / erts-17.1) and reproduces the `DB_READ`/`DB_WRITE`
/// status masks of `db_get_table_aux` (`../../erts/emulator/beam/erl_db.c`).
///
/// Ordered navigation needs no interior support: `ordered_set`
/// `first`/`last`/`next`/`prev` are table-level operations, so the read-checked
/// wrappers here cover them just like the hash kinds.
pub struct TableAccess<'a> {
    /// The table being accessed.
    pub(super) table: &'a mut Table,
    /// The process on whose behalf the operations run.
    pub(super) requester: Option<OwnerId>,
}

impl TableAccess<'_> {
    /// The requester this view authenticates as.
    pub fn requester(&self) -> Option<OwnerId> {
        self.requester
    }

    // ------------------------------------------------------------------- writes

    /// `ets:insert/2`, requiring write access.
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        self.table.check_write(self.requester)?;
        self.table.insert(object, atoms)
    }

    /// `ets:insert/2` with a list of objects, requiring write access.
    pub fn insert_list(&mut self, objects: &[Eterm], atoms: &AtomTable) -> Result<()> {
        self.table.check_write(self.requester)?;
        self.table.insert_list(objects, atoms)
    }

    /// `ets:insert_new/2`, requiring write access.
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        self.table.check_write(self.requester)?;
        self.table.insert_new(object, atoms)
    }

    /// `ets:delete/2`, requiring write access.
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        self.table.check_write(self.requester)?;
        self.table.delete(key, atoms)
    }

    /// `ets:delete_object/2`, requiring write access.
    pub fn delete_object(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        self.table.check_write(self.requester)?;
        self.table.delete_object(object, atoms)
    }

    /// `ets:delete_all_objects/1`, requiring write access.
    ///
    /// The underlying [`Table::delete_all_objects`] returns `()`; the checked
    /// view returns `Result` so a denied write surfaces as `BeamError::BadArg`
    /// (and, crucially, does **not** clear the table).
    pub fn delete_all_objects(&mut self) -> Result<()> {
        self.table.check_write(self.requester)?;
        self.table.delete_all_objects();
        Ok(())
    }

    /// `ets:take/2`, requiring write access (it removes the entry).
    pub fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        self.table.check_write(self.requester)?;
        self.table.take(key, atoms)
    }

    /// `ets:update_counter/3`, requiring write access.
    pub fn update_counter(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Eterm> {
        self.table.check_write(self.requester)?;
        self.table.update_counter(key, update_op, atoms, heap)
    }

    /// `ets:update_counter/4`, requiring write access.
    pub fn update_counter_with(
        &mut self,
        key: Eterm,
        update_op: Eterm,
        default: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Eterm> {
        self.table.check_write(self.requester)?;
        self.table
            .update_counter_with(key, update_op, default, atoms, heap)
    }

    /// `ets:update_element/3`, requiring write access.
    pub fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<bool> {
        self.table.check_write(self.requester)?;
        self.table.update_element(key, updates, atoms, heap)
    }

    /// `ets:select_delete/2`, requiring write access.
    pub fn select_delete(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        self.table.check_write(self.requester)?;
        self.table.select_delete(spec, atoms, heap)
    }

    /// `ets:match_delete/2`, requiring write access.
    pub fn match_delete(
        &mut self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        self.table.check_write(self.requester)?;
        self.table.match_delete(pattern_term, atoms, heap)
    }

    /// `ets:select_replace/2`, requiring write access (it rewrites the objects).
    pub fn select_replace(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        self.table.check_write(self.requester)?;
        self.table.select_replace(spec, atoms, heap)
    }

    /// `ets:rename/2`, requiring **write** access.
    ///
    /// The oracle resolves the table with the `DB_WRITE` status mask
    /// (`db_get_table_aux`), so a rename follows the same gate as every other
    /// write: a non-owner may rename a `public` table, while a non-owner rename
    /// of a `protected`/`private` table is `badarg`. It is deliberately **not**
    /// owner-gated the way `ets:give_away/3` is — a rename is a name write, not
    /// an ownership operation (`src/table/rename.rs` rule 3).
    ///
    /// The gate runs **before** the name write, so a denial leaves the table's
    /// recorded name — like its objects, its [`TableId`](super::TableId), its
    /// owner and its access mode — untouched. See [`Table::rename`] for the
    /// name-conflict rules and the registry-rekey obligation the
    /// [`RenameOutcome`] carries back to the caller.
    pub fn rename(
        &mut self,
        name: Atom,
        is_taken: impl FnOnce(Atom) -> bool,
    ) -> Result<RenameOutcome> {
        self.table.check_write(self.requester)?;
        self.table.rename(name, is_taken)
    }

    // -------------------------------------------------------------------- reads

    /// `ets:lookup/2`, requiring read access.
    pub fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        self.table.check_read(self.requester)?;
        Ok(self.table.lookup(key, atoms))
    }

    /// `ets:member/2`, requiring read access.
    pub fn member(&self, key: Eterm, atoms: &AtomTable) -> Result<bool> {
        self.table.check_read(self.requester)?;
        Ok(self.table.member(key, atoms))
    }

    /// `ets:lookup_element/3`, requiring read access.
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        self.table.check_read(self.requester)?;
        self.table.lookup_element(key, pos, atoms)
    }

    /// `ets:lookup_element/4`, requiring read access.
    pub fn lookup_element_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Eterm> {
        self.table.check_read(self.requester)?;
        self.table.lookup_element_or(key, pos, default, atoms)
    }

    /// `ets:lookup_element/3` for a multi-object kind (`bag`/`duplicate_bag`),
    /// whose result is the element *list*; requiring read access.
    ///
    /// See [`Table::lookup_element_all`].
    pub fn lookup_element_all(
        &self,
        key: Eterm,
        pos: usize,
        atoms: &AtomTable,
    ) -> Result<Vec<Eterm>> {
        self.table.check_read(self.requester)?;
        self.table.lookup_element_all(key, pos, atoms)
    }

    /// `ets:lookup_element/4` for a multi-object kind: `Ok(None)` for an absent
    /// key (the caller yields `default`); requiring read access.
    ///
    /// See [`Table::lookup_element_all_or`].
    pub fn lookup_element_all_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Option<Vec<Eterm>>> {
        self.table.check_read(self.requester)?;
        self.table.lookup_element_all_or(key, pos, default, atoms)
    }
    /// `ets:select/2`, requiring read access.
    pub fn select(
        &self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Vec<Eterm>> {
        self.table.check_read(self.requester)?;
        self.table.select(spec, atoms, heap)
    }

    /// `ets:select_count/2`, requiring read access.
    pub fn select_count(
        &self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<usize> {
        self.table.check_read(self.requester)?;
        self.table.select_count(spec, atoms, heap)
    }

    /// `ets:match_object/2`, requiring read access.
    pub fn match_object(
        &self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Vec<Eterm>> {
        self.table.check_read(self.requester)?;
        self.table.match_object(pattern_term, atoms, heap)
    }

    /// `ets:match/2`, requiring read access.
    pub fn match_(
        &self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Vec<Eterm>> {
        self.table.check_read(self.requester)?;
        self.table.match_(pattern_term, atoms, heap)
    }

    // --------------------------------------------------------------- navigation

    /// `ets:first/1`, requiring read access. `Ok(None)` is `'$end_of_table'`.
    pub fn first(&self, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.table.check_read(self.requester)?;
        Ok(self.table.first(atoms))
    }

    /// `ets:last/1`, requiring read access. `Ok(None)` is `'$end_of_table'`.
    pub fn last(&self, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.table.check_read(self.requester)?;
        Ok(self.table.last(atoms))
    }

    /// `ets:next/2`, requiring read access. `Ok(None)` is `'$end_of_table'`.
    pub fn next(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.table.check_read(self.requester)?;
        self.table.next(key, atoms)
    }

    /// `ets:prev/2`, requiring read access. `Ok(None)` is `'$end_of_table'`.
    pub fn prev(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.table.check_read(self.requester)?;
        self.table.prev(key, atoms)
    }

    /// `ets:tab2list/1`, requiring read access.
    pub fn tab2list(&self) -> Result<Vec<Eterm>> {
        self.table.check_read(self.requester)?;
        Ok(self.table.tab2list())
    }

    /// `ets:info(T, size)`, requiring read access.
    pub fn size(&self) -> Result<usize> {
        self.table.check_read(self.requester)?;
        Ok(self.table.size())
    }

    /// Whether the table holds no objects, requiring read access.
    pub fn is_empty(&self) -> Result<bool> {
        self.table.check_read(self.requester)?;
        Ok(self.table.is_empty())
    }

    /// `ets:safe_fixtable/2`, requiring **read** access.
    ///
    /// The oracle gates `safe_fixtable/2` as a *read*, not a write (`db_get_table_aux`
    /// with the `DB_READ` status mask): a non-owner may fix a `protected` or `public`
    /// table but not a `private` one (verified against the system `erl`, OTP 29 /
    /// erts-17.1). The underlying [`Table::set_fixation`] returns `()`; the checked
    /// view returns `Result` so a denied call surfaces as `BeamError::BadArg` and, as
    /// with every other check here, does **not** change the table's fixation state.
    pub fn set_fixation(&mut self, fixed: bool) -> Result<()> {
        self.table.check_read(self.requester)?;
        self.table.set_fixation(fixed);
        Ok(())
    }
    // --------------------------------------------- limited / continuation forms
    //
    // The `/3` forms and their `/1` resumptions are all **reads**: they scan the
    // table and build results, they never mutate it. They are therefore
    // read-checked, like `select`/`match`. The same holds for the
    // `select_reverse` family below (the reverse-direction `select`).

    /// `ets:select/3`, requiring read access. `Ok(None)` ⇒ `'$end_of_table'`.
    pub fn select_limit(
        &self,
        spec: &MatchSpec,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.select_limit(spec, limit, atoms, heap)
    }

    /// `ets:select/1`, requiring read access.
    pub fn select_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.select_continue(cont, atoms, heap)
    }

    /// `ets:select_reverse/2`, requiring read access.
    ///
    /// The reverse-direction analogue of [`Self::select`]: it only scans and
    /// builds results, so it is gated as a **read**, exactly like `select`.
    pub fn select_reverse(
        &self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<Vec<Eterm>> {
        self.table.check_read(self.requester)?;
        self.table.select_reverse(spec, atoms, heap)
    }

    /// `ets:select_reverse/3`, requiring read access. `Ok(None)` ⇒ `'$end_of_table'`.
    pub fn select_reverse_limit(
        &self,
        spec: &MatchSpec,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.select_reverse_limit(spec, limit, atoms, heap)
    }

    /// `ets:select_reverse/1`, requiring read access.
    pub fn select_reverse_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.select_reverse_continue(cont, atoms, heap)
    }

    /// `ets:match/3`, requiring read access.
    pub fn match_limit(
        &self,
        pattern_term: Eterm,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.match_limit(pattern_term, limit, atoms, heap)
    }

    /// `ets:match/1`, requiring read access.
    pub fn match_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.match_continue(cont, atoms, heap)
    }

    /// `ets:match_object/3`, requiring read access.
    pub fn match_object_limit(
        &self,
        pattern_term: Eterm,
        limit: usize,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.match_object_limit(pattern_term, limit, atoms, heap)
    }

    /// `ets:match_object/1`, requiring read access.
    pub fn match_object_continue(
        &self,
        cont: &MatchContinuation,
        atoms: &AtomTable,
        heap: &mut Heap,
    ) -> Result<(Vec<Eterm>, Option<MatchContinuation>)> {
        self.table.check_read(self.requester)?;
        self.table.match_object_continue(cont, atoms, heap)
    }

    // -------------------------------------------------------------- ownership

    /// `ets:give_away/3`: the requester is the caller.
    ///
    /// The ownership rules (`badarg` unless the requester is the current owner and
    /// `new_owner` is a different live one) live in [`Table::give_away`]; this view
    /// only supplies the caller. `ets:give_away/3` does **not** consult the
    /// `protection` mode — it is an ownership operation, not a read/write one.
    pub fn give_away(&mut self, new_owner: Option<OwnerId>) -> Result<()> {
        self.table.give_away(self.requester, new_owner)
    }

    /// `ets:setopts/2` (the `{heir, ...}` and `{protection, Access}` options): the
    /// requester is the caller.
    ///
    /// Requires the requester to be the table owner (`badarg` otherwise), exactly
    /// like the oracle; see [`Table::setopts`].
    pub fn setopts(&mut self, opt: SetOpt) -> Result<()> {
        self.table.setopts(self.requester, opt)
    }

    /// `ets:setopts(Tab, {protection, Access})`: an owner-gated access change.
    ///
    /// The oracle routes `{protection, _}` through `ets:setopts/2`, so this is
    /// exactly [`SetOpt::Access`] with the requester supplied as the caller: the
    /// requester must be the table owner (`badarg` otherwise, with the current
    /// access mode unchanged — the gate runs before the mutation). Changing the
    /// protection does **not** re-normalize the creation-time fine-locking flags;
    /// see [`Table::set_access`].
    pub fn set_access(&mut self, access: Access) -> Result<()> {
        self.table.setopts(self.requester, SetOpt::Access(access))
    }
}
