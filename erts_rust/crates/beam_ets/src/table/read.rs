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

//! The [`Table`] read and navigation surface.
//!
//! Split out of `table::mod` to keep every source file under the workspace's
//! ~1000-line budget: `lookup`/`member`, the `lookup_element*` family, the
//! `first`/`last`/`next`/`prev` walk, `take`, and the `size`/`is_empty`/
//! `tab2list`/`memory` accessors. The methods are plain inherent `impl Table`
//! blocks, so the public API (and the `crate::table::Table` path) is unchanged.

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::{badarg, unsupported};

use super::{Table, MULTI_ELEMENT_SCALAR};

impl Table {
    /// The objects stored under `key` (0 or 1 for `set`/`ordered_set`).
    pub fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        self.interior.lookup(key, atoms)
    }

    /// Whether `key` is present.
    pub fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        self.interior.member(key, atoms)
    }

    /// The `pos`-th element (1-based) of the object stored under `key`;
    /// `badarg` when `key` is absent or `pos` is out of range.
    ///
    /// Only `set`/`ordered_set` — which store at most one object per key — have a
    /// scalar answer. The multi kinds (`bag`/`duplicate_bag`) hold **several**
    /// objects under one key and `ets:lookup_element/3` returns a *list* of their
    /// `pos`-th elements; a scalar is therefore undefined for them, and this method
    /// raises the crate's typed
    /// [`unsupported`](crate::error::unsupported) instead of a wrong element. Use
    /// [`lookup_element_all`](Table::lookup_element_all) for those kinds.
    ///
    /// The oracle's *failures* are still answered exactly: `pos == 0`, an absent
    /// `key`, and a stored object whose arity is `< pos` are all `badarg`.
    pub fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        // The public position is 1-based; `0` is `badarg` (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        if self.opts.kind().is_multi() {
            // `lookup_elements` reproduces the oracle's `badarg`s (absent key, a
            // short object); where the oracle has an answer it is the *list*, so a
            // scalar would be a wrong result.
            return self
                .interior
                .lookup_elements(key, pos, atoms)
                .and_then(|_| Err(unsupported(MULTI_ELEMENT_SCALAR)));
        }
        self.interior.lookup_element(key, pos, atoms)
    }

    /// `ets:lookup_element/4`: exactly [`lookup_element`](Table::lookup_element),
    /// except that an absent `key` yields `default` instead of `badarg`.
    ///
    /// Verified against the system `erl` (OTP 29 / erts-17.1): `pos` is 1-based and
    /// `pos == 0` is `badarg` whatever the key; an **absent** key returns `default`
    /// even when `pos` is beyond the object's arity; a **present** key with a `pos`
    /// outside the object's arity is `badarg`.
    ///
    /// Like the `/3` form it is scalar, so for `bag`/`duplicate_bag` it raises the
    /// typed [`unsupported`](crate::error::unsupported) for a present key (an absent
    /// key still yields `default`); use
    /// [`lookup_element_all_or`](Table::lookup_element_all_or) there.
    pub fn lookup_element_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Eterm> {
        // `pos` is validated before the key is consulted (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        let present = self.interior.member(key, atoms);
        match (self.opts.kind().is_multi(), present) {
            // An absent key is `default` whatever the kind.
            (_, false) => Ok(default),
            // A present key on a multi kind: the oracle's answer is the element
            // *list* (or `badarg` for an object too short to have `pos`).
            (true, true) => self
                .interior
                .lookup_elements(key, pos, atoms)
                .and_then(|_| Err(unsupported(MULTI_ELEMENT_SCALAR))),
            (false, true) => self.interior.lookup_element(key, pos, atoms),
        }
    }

    /// `ets:lookup_element/3` for a multi-object kind (`bag`/`duplicate_bag`): the
    /// `pos`-th element (1-based) of **every** object stored under `key`, in
    /// stored order.
    ///
    /// Heap-free, like [`lookup`](Table::lookup): the returned elements read
    /// table-owned storage (`Immediate` elements are copied by value), so no
    /// allocation or host heap is involved. Mirrors the system `erl` (OTP 29 /
    /// erts-17.1), where the multi kinds answer with a list: a key holding a single
    /// object yields a one-element list, duplicates (`duplicate_bag`) are
    /// preserved, an **absent** `key` is `badarg`, `pos == 0` is `badarg`, and one
    /// object whose arity is `< pos` makes the **whole** call `badarg`.
    ///
    /// `set`/`ordered_set` store at most one object per key, so for them the vector
    /// always holds exactly one element — the same element
    /// [`lookup_element`](Table::lookup_element) returns.
    pub fn lookup_element_all(
        &self,
        key: Eterm,
        pos: usize,
        atoms: &AtomTable,
    ) -> Result<Vec<Eterm>> {
        // The public position is 1-based; `0` is `badarg` (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        self.interior.lookup_elements(key, pos, atoms)
    }

    /// `ets:lookup_element/4` for a multi-object kind: exactly
    /// [`lookup_element_all`](Table::lookup_element_all), except that an absent
    /// `key` is reported as `Ok(None)` — the caller yields `default` — instead of
    /// `badarg`.
    ///
    /// The split mirrors the oracle exactly: `pos == 0` is `badarg` **before** the
    /// key is consulted, an absent key yields `default` whatever `pos`, and a
    /// present key keeps every failure of
    /// [`lookup_element_all`](Table::lookup_element_all). It is heap-free and
    /// cannot build `default` itself, which is why the absent key is a `None`
    /// rather than a term.
    pub fn lookup_element_all_or(
        &self,
        key: Eterm,
        pos: usize,
        default: Eterm,
        atoms: &AtomTable,
    ) -> Result<Option<Vec<Eterm>>> {
        // `pos` is validated before the key is consulted (mirroring `ets`).
        let pos = pos.checked_sub(1).ok_or_else(badarg)?;
        // `default` belongs to the caller: this method reports the absent key as
        // `None` and the BIF layer substitutes the term.
        let _ = default;
        if !self.interior.member(key, atoms) {
            return Ok(None);
        }
        Ok(Some(self.interior.lookup_elements(key, pos, atoms)?))
    }

    /// The number of words allocated to the table (`ets:info(Tab, memory)`): the
    /// table-owned term storage plus the interior's own index storage.
    pub fn memory(&self) -> usize {
        self.store.words() + self.interior.memory()
    }

    /// Whether the table was created as a `named_table` (the oracle's
    /// `ets:info(Tab, named_table)`).
    ///
    /// This is not merely introspection: it determines a continuation's identity.
    /// The oracle's `ets:select/3` family stores the **name atom** in the
    /// continuation of a `named_table` (and a runtime table id for every other
    /// table), so a `named_table`'s previously-issued continuations become
    /// unresolvable — `error:badarg` — once the table is renamed, while an unnamed
    /// table's survive a rename. [`Table::select_limit`](Table::select_limit) and
    /// its siblings read this flag to reproduce that rule.
    pub fn is_named_table(&self) -> bool {
        self.opts.is_named()
    }

    // --------------------------------------------------------------- navigation

    /// The first key in the table's iteration order (`ets:first/1`); `None` is the
    /// `'$end_of_table'` result and an empty table always yields it.
    ///
    /// The hash kinds (`set`/`bag`/`duplicate_bag`) walk an internal, unspecified
    /// order; an `ordered_set` walks ascending arithmetic key order.
    pub fn first(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.interior.first_key(atoms)
    }

    /// The last key in the table's iteration order (`ets:last/1`); `None` is the
    /// `'$end_of_table'` result and an empty table always yields it.
    ///
    /// Mirrors the verified `erl` (OTP 29 / erts-17.1): for the hash kinds
    /// (`set`/`bag`/`duplicate_bag`) `ets:last/1` is `ets:first/1` — ERTS's hash
    /// tables implement `last` as `first` (`erl_db_hash.c:853-856`), so `last`
    /// returns the same key `first` does, not the greatest key. Only an
    /// `ordered_set` has a real backward step: `last` is its arithmetic-maximum key.
    pub fn last(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.interior.last_key(atoms)
    }

    /// The key following `key` (`ets:next/2`); `Ok(None)` is the
    /// `'$end_of_table'` result.
    ///
    /// Mirrors the verified `erl` (OTP 29 / erts-17.1) behaviour: an `ordered_set`
    /// reads `key` as a **range bound** — an absent key is fine and the answer is
    /// the nearest stored key above it (so an empty table yields `'$end_of_table'`)
    /// — whereas the hash kinds (`set`/`bag`/`duplicate_bag`) walk an internal
    /// order anchored at a *present* entry and raise `badarg` for an unknown key,
    /// which includes every key of an empty table.
    pub fn next(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.check_navigable(key, atoms)?;
        Ok(self.interior.next_key(key, atoms))
    }

    /// The key preceding `key` (`ets:prev/2`); `Ok(None)` is the
    /// `'$end_of_table'` result.
    ///
    /// Mirrors the verified `erl` (OTP 29 / erts-17.1) behaviour: an `ordered_set`
    /// reads `key` as a **range bound** (an absent key is fine, an empty table
    /// yields `'$end_of_table'`), whereas the hash kinds (`set`/`bag`/
    /// `duplicate_bag`) require the anchor key to be *present* (`badarg` otherwise)
    /// and, being hash tables whose `prev` **is** `next` (`erl_db_hash.c:853-856`),
    /// answer exactly what [`next`](Table::next) answers — there is no backward
    /// step and the walk ends at `'$end_of_table'` with the same last key.
    pub fn prev(&self, key: Eterm, atoms: &AtomTable) -> Result<Option<Eterm>> {
        self.check_navigable(key, atoms)?;
        Ok(self.interior.prev_key(key, atoms))
    }

    /// The guard shared by [`next`](Table::next)/[`prev`](Table::prev): only the
    /// hash kinds need the anchor key to be present.
    fn check_navigable(&self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        if self.opts.kind().is_ordered() || self.interior.member(key, atoms) {
            Ok(())
        } else {
            Err(badarg())
        }
    }

    /// Remove `key`'s entry and return every object stored under it
    /// (`ets:take/2`); an absent key yields an empty vector.
    ///
    /// The removed objects are **handed to the caller**: each is deep-copied out of
    /// table storage and the table's own original copy is reclaimed
    /// ([`TermStore::free`](crate::store::TermStore::free)), so a churn of `insert` +
    /// `take` does not grow the table's memory. The returned copies are
    /// [`relinquish`](crate::store::TermStore::relinquish)ed: they stay valid for the
    /// table's lifetime and their words are never reused (a later `copy` can never
    /// hand the caller's term out again), but they no longer count as table memory —
    /// exactly the real `ets:take/2`, where the objects move off the table.
    ///
    /// Every object is copied **before** any original is reclaimed: a copy reads its
    /// original's words, which `free` would return to the store's free lists.
    pub fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        let removed = self.interior.take(key, atoms)?;
        if removed.is_empty() {
            return Ok(Vec::new());
        }
        // Deep-copy every removed object out first (see the doc above).
        let mut taken = Vec::with_capacity(removed.len());
        for &original in &removed {
            match self.store.copy(original) {
                Ok(copy) => taken.push(copy),
                Err(err) => {
                    // The entry is gone either way, so nothing may stay counted:
                    // reclaim the copies taken so far **and** every original before
                    // reporting. (In practice unreachable — a table-owned object is
                    // always a copy of a supported term — but the failure path must
                    // not leak.) The copies were never table objects, so they are
                    // released immediately; the originals were, so a fixed table
                    // defers them like any other removal.
                    for copy in taken.drain(..) {
                        self.store.free(copy);
                    }
                    for &original in &removed {
                        self.free_object(original);
                    }
                    return Err(err);
                }
            }
        }
        // Hand the copies to the caller (no longer table memory, never reusable) and
        // reclaim the table's own originals.
        for &copy in &taken {
            self.store.relinquish(copy);
        }
        for &original in &removed {
            self.free_object(original);
        }
        Ok(taken)
    }

    /// The number of objects in the table.
    ///
    /// The `ets` accessor is `ets:info(T, size)`; `ets:size/1` does not exist in
    /// this OTP (OTP 29 / erts-17.1), so the differential battery reads it through
    /// `ets:info/2`.
    pub fn size(&self) -> usize {
        self.interior.size()
    }

    /// Whether the table holds no objects.
    pub fn is_empty(&self) -> bool {
        self.size() == 0
    }

    /// Every object in the table (ordered for `ordered_set`).
    pub fn tab2list(&self) -> Vec<Eterm> {
        self.interior.iter_objects().collect()
    }

    /// Every object in the table's **reverse-navigation** order (`ets:last/1` +
    /// `ets:prev/2`), expanding each key's objects in stored order.
    ///
    /// This is the crate's single source of truth for a reverse traversal: the
    /// interior decides the order, so an `ordered_set` yields the exact reverse of
    /// [`tab2list`](Table::tab2list) (descending key order), while the hash kinds
    /// (`set`/`bag`/`duplicate_bag`) yield the **same** order as
    /// [`tab2list`](Table::tab2list) — their `last`/`prev` are `first`/`next`
    /// (`erl_db_hash.c:853-856`), so a hash reverse walk *is* a forward walk.
    ///
    /// [`select_reverse`](Table::select_reverse) drives this instead of reversing
    /// [`tab2list`](Table::tab2list) itself, which would wrongly mirror the hash
    /// kinds.
    pub(crate) fn tab2list_rev(&self) -> Vec<Eterm> {
        self.interior.iter_objects_rev().collect()
    }
}
