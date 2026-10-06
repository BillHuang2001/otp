//! The access-enforcing table view: [`TableAccess`] rechecks the table's
//! `protection` against a requester process before every operation.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::{OwnerId, SetOpt, Table};
use crate::select::MatchSpec;

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

    /// `ets:setopts/2` (the `{heir, ...}` option): the requester is the caller.
    ///
    /// Requires the requester to be the table owner (`badarg` otherwise), exactly
    /// like the oracle; see [`Table::setopts`].
    pub fn setopts(&mut self, opt: SetOpt) -> Result<()> {
        self.table.setopts(self.requester, opt)
    }
}
