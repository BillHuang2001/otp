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

//! The private [`Interior`] dispatch seam shared by the crate's interiors.
//!
//! Split out of `table::mod` to keep every source file under the workspace's
//! ~1000-line budget. The trait is re-exported from `table::mod` as
//! `pub(crate) use interior::Interior` (so `crate::table::Interior` stays the
//! reachable path), and [`build_interior`] is the factory `Table` uses to pick
//! the index matching a [`TableKind`].

use beam_common::Result;
use beam_term::prelude::*;

use crate::champ_interior::ChampInterior;
use crate::error::badarg;
use crate::hash_interior::HashInterior;
use crate::key::key_of;
use crate::kind::{OrderedInterior, SetInterior, TableKind};
use crate::ordered_interior::{OrderedSetInterior, SortedOrderedInterior};

use super::update::{add_counters, object_arity, read_counter, replace_elements};

/// The internal dispatch seam implemented by the crate's interiors
/// (`HashInterior`, `OrderedSetInterior`).
///
/// Private: **not** part of the cross-crate API. [`Table`](super::Table) derives
/// the key from the object (`crate::key::key_of`) and passes both to the
/// interior, so the two interiors can share one dispatch surface despite
/// different shapes. All term ordering/hashing is delegated to `beam_term`.
///
/// The `Send + Sync` supertraits are what let a [`Table`](super::Table) — and so
/// a [`SharedTable`](super::SharedTable) — be moved to and shared across OS
/// threads: an interior holds only terms and node/bucket vectors, and every
/// *mutating* operation takes `&mut self`, so an interior read through `&self`
/// performs no writes. Both interiors are plain data structures over `Eterm`
/// (a `usize` newtype), so the bound holds without any manual `unsafe` impl.
pub(crate) trait Interior: Send + Sync {
    /// Insert `object` under `key`, applying the kind's duplicate rules
    /// (replace for `set`/`ordered_set`), and report whether the object was
    /// **retained**.
    ///
    /// `true` means the interior stored `object` (or replaced the entry with it)
    /// and now owns it; `false` means the interior rejected the object as a
    /// duplicate no-op (`bag`), so it did **not** store it and the object is
    /// unreferenced. The caller ([`Table::insert`](super::Table::insert))
    /// reclaims a rejected copy from the store, so a rejected duplicate never
    /// leaks storage. `set`/`ordered_set` always retain (a replace keeps the
    /// *new* object) and `duplicate_bag` always retains; only `bag` can answer
    /// `false`.
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool>;

    /// Insert `object` under `key` only if `key` is absent; returns whether the
    /// object was inserted.
    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool>;

    /// Remove every object stored under `key`.
    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()>;

    /// Remove the single object that equals `object` within `key`'s objects.
    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()>;

    /// All objects stored under `key` (0 or 1 for `set`/`ordered_set`).
    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm>;

    /// Whether any object is stored under `key`.
    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool;

    /// The `pos`-th element (0-based) of the single object stored under `key`;
    /// `badarg` when `key` is absent.
    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm>;

    /// The `pos`-th element (0-based) of **every** object stored under `key`, in
    /// this interior's stored order — `ets:lookup_element/3`'s result for a
    /// multi-object kind (`bag`/`duplicate_bag`), where the oracle returns a
    /// *list*.
    ///
    /// `badarg` when `key` is absent, and when **any** stored object's arity is at
    /// most `pos` — the oracle fails the whole call, it does not skip the short
    /// object (verified against the system `erl`, OTP 29 / erts-17.1). The
    /// single-object kinds (`set`/`ordered_set`) store at most one object per key,
    /// so their list has exactly one element.
    ///
    /// The default reads the objects through [`lookup`](Interior::lookup) and
    /// projects each with [`key_of`] (which is what rejects a short object); an
    /// interior may override it with a single-pass version.
    fn lookup_elements(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        let keypos = pos.checked_add(1).ok_or_else(badarg)?;
        let objects = self.lookup(key, atoms);
        if objects.is_empty() {
            return Err(badarg());
        }
        let mut elements = Vec::with_capacity(objects.len());
        for object in objects {
            elements.push(key_of(object, keypos)?);
        }
        Ok(elements)
    }
    /// The total number of stored objects (not keys, for the multi kinds).
    fn size(&self) -> usize;

    /// Every stored object, ordered for `ordered_set`.
    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_>;

    /// Every stored object in this interior's **reverse-navigation order** — the
    /// order an `ets:last/1` + `ets:prev/2` walk visits them, expanding each key's
    /// objects in stored order.
    ///
    /// The drop-in default is the `ordered_set` semantics: the exact reverse of
    /// [`iter_objects`](Interior::iter_objects) (an `ordered_set` key holds one
    /// object, so reversing the key order reverses the object order). The hash
    /// kinds (`set`/`bag`/`duplicate_bag`) override it to return
    /// [`iter_objects`](Interior::iter_objects) **unchanged**, because their
    /// `last`/`prev` are `first`/`next` (see
    /// [`last_key`](Interior::last_key)) — so their reverse traversal *is* the
    /// forward one.
    fn iter_objects_rev(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        Box::new(self.iter_objects().collect::<Vec<_>>().into_iter().rev())
    }

    // ------------------------------------- removal, navigation and storage

    /// Remove `key`'s entry and return **every** object stored under it (an empty
    /// vector when `key` is absent), mirroring `ets:take/2`.
    ///
    /// The returned objects **alias table storage** (they are the interior's own
    /// copies); [`Table::take`](super::Table::take), the only caller through the
    /// public API, deep-copies them out of the store and then reclaims these
    /// originals, so no returned pointer is ever a reclaimed word. The drop-in
    /// default locates the objects and then deletes the entry; the hash interior
    /// overrides it with its one-pass removal.
    fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        let objects = self.lookup(key, atoms);
        if !objects.is_empty() {
            self.delete(key, atoms)?;
        }
        Ok(objects)
    }

    /// Every stored key, in this interior's own iteration order (ascending
    /// `beam_term` order for `ordered_set`, bucket-scan order for the hash kinds).
    fn keys(&self) -> Vec<Eterm>;

    /// The first key in this interior's iteration order (`ets:first/1`); `None`
    /// when the table is empty (`'$end_of_table'`).
    fn first_key(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.keys().first().copied()
    }

    /// The last key in this interior's iteration order (`ets:last/1`); `None`
    /// when the table is empty.
    ///
    /// The drop-in default is the `ordered_set` semantics: a genuine backward step
    /// to the **largest** key of the (ascending) iteration order. The hash kinds
    /// (`set`/`bag`/`duplicate_bag`) override it, because ERTS implements
    /// `ets:last/1` as `ets:first/1` for **every** hash-table kind — the
    /// `db_first_hash, db_next_hash, db_first_hash /* last == first */,
    /// db_next_hash /* prev == next */` function table
    /// (`../../erts/emulator/beam/erl_db_hash.c:853-856`, repeated at `:891-894`) —
    /// so there is no backward walk and no greatest-key search for them.
    fn last_key(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.keys().last().copied()
    }

    /// The key following `key` in this interior's iteration order, or `None` at
    /// the end of that order.
    ///
    /// An **absent** `key` has no position in the iteration order, so the default
    /// falls back to the nearest key in `beam_term` term order (the first key
    /// strictly greater than `key`); that is exactly the `ordered_set` range
    /// semantics. The hash kinds keep the default and rely on `Table` rejecting an
    /// absent anchor with `badarg` before this is reached.
    fn next_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let keys = self.keys();
        match keys.iter().position(|&candidate| eq(candidate, key, atoms)) {
            Some(position) => keys.get(position + 1).copied(),
            None => keys
                .into_iter()
                .find(|&candidate| cmp(candidate, key, atoms).is_gt()),
        }
    }

    /// The key preceding `key` in this interior's iteration order, or `None` at
    /// the start of that order.
    ///
    /// The drop-in default is the `ordered_set` mirror of
    /// [`next_key`](Interior::next_key) (an absent `key` reads as a range bound:
    /// the nearest key below it). The hash kinds override it as
    /// `prev_key(key) = next_key(key)`, because ERTS's hash-table `ets:prev/2` is
    /// `ets:next/2` (see [`last_key`](Interior::last_key)).
    fn prev_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let keys = self.keys();
        match keys.iter().position(|&candidate| eq(candidate, key, atoms)) {
            Some(position) => position.checked_sub(1).map(|before| keys[before]),
            None => keys
                .into_iter()
                .rev()
                .find(|&candidate| cmp(candidate, key, atoms).is_lt()),
        }
    }

    /// The number of `Eterm`-sized words this interior has allocated for its own
    /// index (`ets:info(Tab, memory)`'s interior contribution).
    ///
    /// The default is `0` — an interior that does not account for its index
    /// storage contributes nothing; the hash and `ordered_set` interiors report
    /// their honest bucket/entry and B+-tree node-arena allocations respectively.
    fn memory(&self) -> usize {
        0
    }

    // ------------------------------------- element updates (`ets:update_*`)

    /// `ets:update_counter/3,4` on a single position: add `incr` to element `pos`
    /// (1-based) of the object stored under `key` and return the **new** counter
    /// value.
    ///
    /// The drop-in default reads the element, adds through the shared
    /// [`add_counters`] helper, rebuilds the object on `heap` and inserts it back
    /// — which is exactly what the B+-tree interior (no writable index slot)
    /// needs. The hash interior overrides it with a destructive element write.
    ///
    /// `badarg` when `key` is absent, `pos` is outside `1..=arity`, or the element
    /// / `incr` is not an integer. The `pos == keypos` rule is the caller's: the
    /// interior does not know `keypos`.
    fn update_counter(
        &mut self,
        key: Eterm,
        pos: usize,
        incr: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        let object = self.lookup(key, atoms).into_iter().next().ok_or_else(badarg)?;
        let arity = object_arity(object)?;
        let newcnt = add_counters(read_counter(object, pos)?, incr, heap)?;
        let updated = replace_elements(object, arity, &[(pos, newcnt)], heap)?;
        self.insert(key, updated, atoms)?;
        Ok(newcnt)
    }

    /// `ets:update_element/3`: replace element `pos` (1-based) of the object
    /// stored under `key` with each `[pos, value]` of `updates` (later entries win
    /// for a repeated position), building the replacement on `heap`. Returns
    /// whether `key` existed.
    ///
    /// The drop-in default rebuilds the object on `heap` and inserts it back,
    /// which is what the B+-tree interior needs; the hash interior overrides it.
    ///
    /// `Ok(false)` for an absent `key` (not an error), `badarg` for a `pos`
    /// outside `1..=arity` — with **nothing** written, since every position is
    /// validated before any is applied.
    fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<bool> {
        let object = match self.lookup(key, atoms).into_iter().next() {
            Some(object) => object,
            None => return Ok(false),
        };
        let arity = object_arity(object)?;
        for &(pos, _) in updates {
            if pos == 0 || pos > arity {
                return Err(badarg());
            }
        }
        if updates.is_empty() {
            // Nothing to change; the key existed, so `ets` reports `true`.
            return Ok(true);
        }
        let updated = replace_elements(object, arity, updates, heap)?;
        self.insert(key, updated, atoms)?;
        Ok(true)
    }
}

/// Build the interior matching `kind`: the CHAMP or chained hash index for
/// `set` (per `set_interior`), the chained hash index for `bag`/`duplicate_bag`,
/// the key-sorted B+-tree or sorted `Vec` for `ordered_set` (per
/// `ordered_interior`).
///
/// `keypos` is the table's 1-based key position, threaded into the hash interior
/// so it can re-anchor a surviving entry's stored key term to one of its remaining
/// table-owned objects (`Interior::delete_object`). The CHAMP and ordered
/// interiors never keep an entry whose anchor object was removed, so they need no
/// `keypos`.
pub(super) fn build_interior(
    kind: TableKind,
    set_interior: SetInterior,
    ordered_interior: OrderedInterior,
    keypos: usize,
) -> Box<dyn Interior> {
    match kind {
        TableKind::Set => match set_interior {
            SetInterior::Champ => Box::new(ChampInterior::new()),
            SetInterior::Chained => Box::new(HashInterior::with_keypos(kind, keypos)),
        },
        TableKind::Bag | TableKind::DuplicateBag => {
            Box::new(HashInterior::with_keypos(kind, keypos))
        }
        TableKind::OrderedSet => match ordered_interior {
            OrderedInterior::BPlusTree => Box::new(OrderedSetInterior::new()),
            OrderedInterior::Sorted => Box::new(SortedOrderedInterior::new()),
        },
    }
}
