//! The access-enforcing table view: [`TableAccess`] rechecks the table's
//! `protection` against a requester process before every operation.

use beam_common::Result;
use beam_term::prelude::*;

use super::{OwnerId, Table};

/// An access-enforcing view over a [`Table`] for a specific requester process.
///
/// Obtained from [`Table::as_requester`]. Every operation first rechecks the
/// table's `protection` against the requester (`kind::can_read`/`can_write`) and
/// raises `BeamError::BadArg` on a violation, exactly like `ets`. The
/// underlying [`Table`] methods remain the owner-level, non-checking primitives.
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

    /// `ets:insert/2`, requiring write access.
    pub fn insert(&mut self, object: Eterm, atoms: &AtomTable) -> Result<()> {
        self.table.check_write(self.requester)?;
        self.table.insert(object, atoms)
    }

    /// `ets:insert_new/2`, requiring write access.
    pub fn insert_new(&mut self, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        self.table.check_write(self.requester)?;
        self.table.insert_new(object, atoms)
    }

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

    /// `ets:delete/2`, requiring write access.
    pub fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        self.table.check_write(self.requester)?;
        self.table.delete(key, atoms)
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
        heap: &mut beam_alloc::Heap,
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
        heap: &mut beam_alloc::Heap,
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
        heap: &mut beam_alloc::Heap,
    ) -> Result<bool> {
        self.table.check_write(self.requester)?;
        self.table.update_element(key, updates, atoms, heap)
    }
}
