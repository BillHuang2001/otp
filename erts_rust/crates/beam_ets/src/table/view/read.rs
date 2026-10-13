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

//! [`TableRead`] — a read guard that hands out lifetime-tagged terms.
//!
//! It owns the `RwLockReadGuard`, so every [`TableTerm`] it produces is valid for
//! as long as the view is borrowed and no longer. Owned metadata (`size`,
//! `memory`, `info`, …) escapes freely — it is copied out by value — while the
//! term reads stay tagged. The select/match family, whose results may alias table
//! storage, is exposed only as an explicit **copy-out** into a caller-owned
//! [`TermStore`]. See the [module docs](super) for the hazard this prevents.

use std::sync::RwLockReadGuard;

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use super::{is_heap_exhaustion, TableTerm, SCRATCH_MAX_WORDS, SCRATCH_MIN_WORDS};
use crate::info::TableInfo;
use crate::kind::{Access, TableKind};
use crate::select::MatchSpec;
use crate::store::TermStore;
use crate::table::{OwnerId, Table, TableId};

/// A read guard over a [`Table`]: the safe, term-tagging alternative to holding
/// the raw `RwLockReadGuard`.
///
/// Obtained from [`SharedTable::read`](super::super::SharedTable::read) or
/// [`SharedTable::with_read`](super::super::SharedTable::with_read). Many threads
/// may hold one at a time; a writer is excluded for as long as it is alive.
///
/// Its term reads are **owner-level primitives** — like the underlying
/// [`Table`] methods, they do not re-check the table's `protection`. A caller that
/// needs the access check takes a
/// [`TableWrite`](super::TableWrite) and uses its
/// [`as_requester`](super::TableWrite::as_requester) view.
pub struct TableRead<'g> {
    /// The read guard. Holding it is what makes the tagged terms valid.
    guard: RwLockReadGuard<'g, Table>,
    /// The select family's scratch heap, allocated lazily so the default read
    /// path (`lookup`, `tab2list`, navigation) never allocates. Reset per call;
    /// nothing handed out ever aliases it (results are copied out first).
    scratch: Option<Heap>,
}

impl<'g> TableRead<'g> {
    /// Wrap a freshly acquired read guard.
    ///
    /// `pub(in crate::table)`: the sibling [`SharedTable`](super::super::SharedTable)
    /// is the only caller, and keeping this out of the public API keeps the tagged
    /// terms coming from exactly one place.
    pub(in crate::table) fn new(guard: RwLockReadGuard<'g, Table>) -> TableRead<'g> {
        TableRead {
            guard,
            scratch: None,
        }
    }

    // ------------------------------------------------------------------ metadata
    //
    // All of these return owned, copyable data, so they may escape the view.

    /// The underlying table, for the sibling traversal iterators in `view::iter`.
    ///
    /// `pub(super)`: the `view` module's own `iter` child is the only caller. It
    /// hands out **no terms** — every key/object the iterators read is wrapped in a
    /// [`TableTerm`] tagged to this view's borrow — and the table itself cannot
    /// escape, because this method borrows `&self` (which owns the guard).
    pub(super) fn table(&self) -> &Table {
        &self.guard
    }

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

    /// The words allocated to the table (`ets:info(Tab, memory)`).
    pub fn memory(&self) -> usize {
        self.guard.memory()
    }

    /// A snapshot of the table's introspection fields.
    pub fn info(&self) -> TableInfo {
        self.guard.info()
    }

    /// Whether the table is fixed (`ets:safe_fixtable/2`).
    pub fn is_fixed(&self) -> bool {
        self.guard.is_fixed()
    }

    // --------------------------------------------------------------- term reads

    /// The objects stored under `key` (0 or 1 for `set`/`ordered_set`).
    pub fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<TableTerm<'_>> {
        self.guard
            .lookup(key, atoms)
            .into_iter()
            .map(|term| TableTerm::new(term))
            .collect()
    }

    /// Whether `key` is present.
    pub fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        self.guard.member(key, atoms)
    }

    /// The `pos`-th element (1-based) of the object stored under `key`.
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<TableTerm<'_>> {
        self.guard
            .lookup_element(key, pos, atoms)
            .map(|term| TableTerm::new(term))
    }

    /// `ets:lookup_element/4`: like [`lookup_element`](Self::lookup_element), but an
    /// absent `key` yields `default` (`default` is the caller's own term, so it is
    /// tagged with the view's lifetime for uniformity).
    pub fn lookup_element_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<TableTerm<'_>> {
        self.guard
            .lookup_element_or(key, pos, default, atoms)
            .map(|term| TableTerm::new(term))
    }

    /// `ets:lookup_element/3` for a multi-object kind: the `pos`-th element of every
    /// object stored under `key`.
    pub fn lookup_element_all(
        &self,
        key: Eterm,
        pos: usize,
        atoms: &AtomTable,
    ) -> Result<Vec<TableTerm<'_>>> {
        Ok(self
            .guard
            .lookup_element_all(key, pos, atoms)?
            .into_iter()
            .map(|term| TableTerm::new(term))
            .collect())
    }

    /// `ets:lookup_element/4` for a multi-object kind: an absent key is `Ok(None)`.
    pub fn lookup_element_all_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Option<Vec<TableTerm<'_>>>> {
        Ok(self
            .guard
            .lookup_element_all_or(key, pos, default, atoms)?
            .map(|elements| {
                elements
                    .into_iter()
                    .map(|term| TableTerm::new(term))
                    .collect()
            }))
    }

    /// The first key in iteration order (`ets:first/1`); `None` is
    /// `'$end_of_table'`.
    pub fn first(&self, atoms: &AtomTable) -> Option<TableTerm<'_>> {
        self.guard.first(atoms).map(|term| TableTerm::new(term))
    }

    /// The last key in iteration order (`ets:last/1`); `None` is
    /// `'$end_of_table'`.
    pub fn last(&self, atoms: &AtomTable) -> Option<TableTerm<'_>> {
        self.guard.last(atoms).map(|term| TableTerm::new(term))
    }

    /// The key following `key` (`ets:next/2`); `None` is `'$end_of_table'`.
    pub fn next(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<TableTerm<'_>>> {
        Ok(self
            .guard
            .next(key, atoms)?
            .map(|term| TableTerm::new(term)))
    }

    /// The key preceding `key` (`ets:prev/2`); `None` is `'$end_of_table'`.
    pub fn prev(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<TableTerm<'_>>> {
        Ok(self
            .guard
            .prev(key, atoms)?
            .map(|term| TableTerm::new(term)))
    }

    /// `ets:slot/2`: the objects in slot `slot`, tagged to this view's lifetime
    /// like every other term read here.
    ///
    /// `Ok(Some(objects))` is the slot's object list, `Ok(None)` the
    /// `'$end_of_table'` slot and `Err` every out-of-range slot (a negative one
    /// included). See [`Table::slot`] for the exact semantics.
    pub fn slot(&self, slot: i64, atoms: &AtomTable) -> Result<Option<Vec<TableTerm<'_>>>> {
        Ok(self.guard.slot(slot, atoms)?.map(|objects| {
            objects
                .into_iter()
                .map(|term| TableTerm::new(term))
                .collect()
        }))
    }

    /// Every object in the table (ordered for `ordered_set`).
    pub fn tab2list(&self) -> Vec<TableTerm<'_>> {
        self.guard
            .tab2list()
            .into_iter()
            .map(|term| TableTerm::new(term))
            .collect()
    }

    // ------------------------------------------------------ select / match family
    //
    // A clause body may be `'$_'` — the matched object itself — so a select result
    // can alias table storage. These forms therefore do **not** return terms: they
    // deep-copy every result into the caller's `dest` and hand back owned roots.
    // The cost is one `TermStore::copy` per result, plus a view-owned scratch heap
    // (reset per call, grown on exhaustion), which no returned value aliases.

    /// `ets:select/2`, copying every result into `dest`.
    ///
    /// The results are the body value of each matched object, in iteration order;
    /// each is deep-copied, so the returned roots are caller-owned and may outlive
    /// the guard. Cost: one deep copy per matching object.
    pub fn select(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
        dest: &mut TermStore,
    ) -> Result<Vec<Eterm>> {
        self.copy_out(dest, |table, heap| table.select(spec, atoms, heap))
    }

    /// `ets:select_count/2`: how many objects the spec's clause value marks `true`.
    pub fn select_count(
        &mut self,
        spec: &MatchSpec,
        atoms: &AtomTable,
    ) -> Result<usize> {
        self.run_with_scratch(|table, heap| table.select_count(spec, atoms, heap))
    }

    /// `ets:match_object/2`, copying every matching object into `dest`.
    pub fn match_object(
        &mut self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        dest: &mut TermStore,
    ) -> Result<Vec<Eterm>> {
        self.copy_out(dest, |table, heap| table.match_object(pattern_term, atoms, heap))
    }

    /// `ets:match/2`, copying every result into `dest`.
    pub fn match_(
        &mut self,
        pattern_term: Eterm,
        atoms: &AtomTable,
        dest: &mut TermStore,
    ) -> Result<Vec<Eterm>> {
        self.copy_out(dest, |table, heap| table.match_(pattern_term, atoms, heap))
    }

    // -------------------------------------------------------------- scratch heap

    /// The capacity the scratch heap should have for the next call: its current
    /// capacity, or the floor.
    fn scratch_capacity(&self) -> usize {
        self.scratch
            .as_ref()
            .map_or(SCRATCH_MIN_WORDS, |heap| heap.capacity().max(SCRATCH_MIN_WORDS))
    }

    /// Make sure the scratch heap can hold at least `capacity` words.
    fn ensure_scratch(&mut self, capacity: usize) -> Result<()> {
        if self.scratch.as_ref().map_or(0, |heap| heap.capacity()) < capacity {
            self.scratch = Some(Heap::new(capacity)?);
        }
        Ok(())
    }

    /// Run a **pure** select evaluation against the scratch heap, retrying with a
    /// larger heap when the evaluation runs out of scratch space.
    ///
    /// Retrying is sound only because the callback does not mutate the table; the
    /// read family it serves (`select_count`, and the `copy_out` results) is pure.
    fn run_with_scratch<R>(
        &mut self,
        mut run: impl FnMut(&Table, &mut Heap) -> Result<R>,
    ) -> Result<R> {
        let mut capacity = self.scratch_capacity();
        loop {
            self.ensure_scratch(capacity)?;
            // The scratch heap was just ensured above; if it is somehow absent, that
            // is a typed capability failure, never a panic on a runtime path.
            let scratch = self
                .scratch
                .as_mut()
                .ok_or_else(|| crate::error::unsupported("read view scratch heap unavailable"))?;
            scratch.reset();
            match run(&self.guard, scratch) {
                Ok(value) => return Ok(value),
                Err(err) if is_heap_exhaustion(&err) && capacity < SCRATCH_MAX_WORDS => {
                    capacity = capacity.saturating_mul(2).min(SCRATCH_MAX_WORDS);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Run a **pure** select evaluation and deep-copy every result out of the
    /// scratch heap (and out of table storage) into `dest`.
    fn copy_out(
        &mut self,
        dest: &mut TermStore,
        mut run: impl FnMut(&Table, &mut Heap) -> Result<Vec<Eterm>>,
    ) -> Result<Vec<Eterm>> {
        let mut capacity = self.scratch_capacity();
        loop {
            self.ensure_scratch(capacity)?;
            // As `run_with_scratch`: an absent scratch heap is a typed failure, not a
            // panic on a runtime path.
            let scratch = self
                .scratch
                .as_mut()
                .ok_or_else(|| crate::error::unsupported("read view scratch heap unavailable"))?;
            scratch.reset();
            match run(&self.guard, scratch) {
                Ok(raw) => {
                    // Copy out *before* the scratch heap is reused: a fresh result
                    // aliases the scratch heap, and an `'$_'` result aliases table
                    // storage. Both are read here, under the guard, and the copies
                    // go into `dest` — caller-owned storage.
                    let mut owned = Vec::with_capacity(raw.len());
                    for term in raw {
                        owned.push(dest.copy(term)?);
                    }
                    return Ok(owned);
                }
                Err(err) if is_heap_exhaustion(&err) && capacity < SCRATCH_MAX_WORDS => {
                    capacity = capacity.saturating_mul(2).min(SCRATCH_MAX_WORDS);
                }
                Err(err) => return Err(err),
            }
        }
    }
}
