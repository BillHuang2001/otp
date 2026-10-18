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

//! Chained-bucket hash interior for `bag` and `duplicate_bag` (and an opt-in
//! `set` interior, [`crate::SetInterior::Chained`]; `set` defaults to the CHAMP
//! interior in [`crate::champ_interior`]).
//!
//! A bucket array (`Vec` of buckets) indexed by `beam_term::hashing::term_hash`
//! and searched with `beam_term::compare::eq`; buckets grow (double + rehash)
//! past a load factor and give capacity back as objects leave. `set` keeps at
//! most one object per key; `bag` and `duplicate_bag` keep many (a `bag` rejects
//! an *identical* object within a key, a `duplicate_bag` does not). Nothing here
//! re-implements term order or hashing.
//!
//! Public for testing and reuse, but **not** the cross-crate seam: dependents go
//! through [`crate::Table`].
//!
//! Beyond the private `Interior` contract the type also carries the inherent
//! `take` / counter / element-update operations (`ets:take/2`,
//! `ets:update_counter/3,4`, `ets:update_element/3`) for the hash kinds. Those
//! mirror the real ETS semantics, checked against the system `erl`
//! (OTP 29 / erts-17.1); they share the counter arithmetic (`add_counters`,
//! arbitrary precision) and the tuple-rebuild helpers with the `Interior` trait
//! defaults in [`crate::table`]. The `pos == keypos` rule is enforced by
//! [`crate::Table`], which owns `keypos`.
//!
//! # Layout
//!
//! - `mod.rs` (this file) — the [`HashInterior`] type, its bucket lookup, the
//!   inherent `take`/counter/element-update operations and the `Interior` impl.
//! - `capacity.rs` — the bucket array's **growth and release policy**: the
//!   load-factor constants, the rebuild-at-exact-capacity `rehash_into`,
//!   `grow`/`maybe_grow`/`maybe_shrink` and the per-entry `objs` trim.
//! - `tests.rs` — the interior's unit tests (`capacity.rs` carries the
//!   shrink/churn ones next to the policy they pin).

use core::mem::size_of;

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::key_of;
use crate::kind::TableKind;
use crate::table::{add_counters, counter_slot, replace_elements, Interior};

mod capacity;
#[cfg(test)]
mod tests;

/// One hash bucket entry: a key and the objects stored under it.
struct Entry {
    /// The key.
    key: Eterm,
    /// The objects stored under `key` (`set` keeps at most one).
    objs: Vec<Eterm>,
}

/// The bucket-hash interior shared by `set`, `bag` and `duplicate_bag`.
pub struct HashInterior {
    /// Which multi-object rules apply.
    kind: TableKind,
    /// The 1-based key position, used to re-derive a surviving entry's stored key
    /// from one of its objects (see [`Interior::delete_object`]). It is the only
    /// thing this interior needs `keypos` for; `Table` owns the key-position rules.
    keypos: usize,
    /// The bucket array.
    buckets: Vec<Vec<Entry>>,
    /// The total number of stored objects.
    len: usize,
}
impl HashInterior {
    /// A new empty interior for the given (hash-indexed) `kind`, with the default
    /// key position (`1`).
    ///
    /// The degenerate `object == key` callers (unit tests, `interior_bench`) drive
    /// the interior directly without a table, so the default keeps them working;
    /// a [`Table`](crate::Table) always uses [`with_keypos`](HashInterior::with_keypos).
    pub fn new(kind: TableKind) -> HashInterior {
        HashInterior::with_keypos(kind, 1)
    }

    /// A new empty interior for `kind` with an explicit 1-based `keypos`.
    ///
    /// `keypos` is remembered so [`Interior::delete_object`] can re-anchor a
    /// surviving entry's stored key term to one of its remaining table-owned
    /// objects. It never changes the index's shape or hashing.
    pub fn with_keypos(kind: TableKind, keypos: usize) -> HashInterior {
        HashInterior {
            kind,
            keypos,
            buckets: capacity::initial_buckets(),
            len: 0,
        }
    }

    /// The bucket a key hashes to. `buckets` is never empty (it starts at
    /// [`capacity::INITIAL_BUCKETS`], doubles past the load factor and never
    /// shrinks below the starting count), so the modulo is well defined.
    fn bucket_index(&self, key: Eterm, atoms: &AtomTable) -> usize {
        (term_hash(key, atoms) as usize) % self.buckets.len()
    }

    /// The number of hash slots this index exposes (`ets:slot/2`'s
    /// `'$end_of_table'` boundary): the bucket-array length.
    ///
    /// The C's `nactive` (`NACTIVE(tb)`, the count of active buckets); it is this
    /// interior's own, unpinned bucket *count* — only the resulting
    /// partition/end-of-table/badarg contract is.
    fn bucket_count(&self) -> usize {
        self.buckets.len()
    }

    /// The position of `key`'s entry in `bucket`, if any. Term equality is
    /// delegated to `beam_term::compare::eq`.
    fn entry_pos(bucket: &[Entry], key: Eterm, atoms: &AtomTable) -> Option<usize> {
        bucket.iter().position(|e| eq(e.key, key, atoms))
    }

    // -----------------------------------------------------------------------
    // Inherent operation set (beyond the private `Interior` contract)
    // -----------------------------------------------------------------------
    //
    // These mirror `ets:take/2`, `ets:update_counter/3,4` and
    // `ets:update_element/3` for the hash kinds. Their `pos` arguments are
    // **1-based** ETS tuple positions (unlike the trait's 0-based
    // `lookup_element`). Every behaviour below was checked against the system
    // `erl` (OTP 29 / erts-17.1); the residual gaps are called out per method.

    /// Remove `key`'s entry and return **every** object stored under it, in
    /// insertion order (an empty vector when `key` is absent). Mirrors
    /// `ets:take/2`.
    pub fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        let bi = self.bucket_index(key, atoms);
        match Self::entry_pos(&self.buckets[bi], key, atoms) {
            Some(pos) => {
                let entry = self.buckets[bi].remove(pos);
                self.len -= entry.objs.len();
                self.maybe_shrink(atoms);
                entry.objs
            }
            None => Vec::new(),
        }
    }
    /// Add integer `incr` (which may be negative, and may be a bignum) to element
    /// `pos` (1-based) of the object stored under `key`, writing the result back,
    /// and return the new counter value. Mirrors `ets:update_counter/3,4` for
    /// `set`.
    ///
    /// Failure modes, matching `ets:update_counter/3` verified against `erl`:
    /// * `bag`/`duplicate_bag` → `badarg` (only `set`/`ordered_set` have counters);
    /// * absent `key` → `badarg` — real ETS raises when "no object with the correct
    ///   key exists and no default object was supplied", and a *default object* is
    ///   not expressible through this signature (inserting one would also need
    ///   table-owned storage the interior does not own);
    /// * `pos` outside `1..=arity` (including `0`) → `badarg`;
    /// * a non-integer element at `pos`, or a non-integer `incr` → `badarg`.
    ///
    /// The arithmetic is exact and arbitrary precision (shared with the `Interior`
    /// default): a sum that leaves the 60-bit small range is **promoted to a
    /// bignum**, exactly as real ETS does (`erl_db_util.c:3203` `db_add_counter`).
    ///
    /// Gaps (documented, never a silently-wrong value):
    /// * `pos == keypos` is `badarg`, but the interior carries no `keypos`, so the
    ///   caller ([`crate::Table`], which owns `keypos`) rejects it — this method
    ///   is the `Interior` override and is only reached through that caller;
    /// * the `{Pos, Incr, Threshold, SetValue}` threshold form has no parameter
    ///   here and is not applied (the caller applies it).
    ///
    /// A small result is written **destructively** into the stored object's element
    /// slot (an immediate needs no allocation). A **bignum** result does not fit
    /// one element word, so the object is rebuilt on `heap` with
    /// [`replace_elements`] and the rebuilt root replaces the stored one; the
    /// caller ([`crate::Table::update_counter`]) then copies it into table-owned
    /// storage (the A.1 copy boundary) via its `relocate_rebuilt_object` step.
    pub fn update_counter(
        &mut self,
        key: Eterm,
        pos: usize,
        incr: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        if self.kind != TableKind::Set {
            return Err(badarg());
        }
        // The key must exist first: real ETS raises `badarg` here before it even
        // inspects the update operand.
        let bi = self.bucket_index(key, atoms);
        let entry = match Self::entry_pos(&self.buckets[bi], key, atoms) {
            Some(entry) => entry,
            None => return Err(badarg()),
        };
        let object = *self.buckets[bi][entry].objs.first().ok_or_else(badarg)?;
        // Validate the position against the object's arity and locate the element.
        let slot = counter_slot(object, pos)?;
        // SAFETY: `slot` is element `pos` of a live tuple owned by the table (an
        // entry always stores a deep copy), so the word is an initialised term.
        let oldcnt = unsafe { *slot };
        let newcnt = add_counters(oldcnt, incr, heap)?;
        if is_small(newcnt) {
            // Destructive in-place update, exactly as `ets:update_counter` mutates
            // the stored object (the table owns this copy). The new value is a small
            // integer — an immediate — so it needs no allocation and cannot dangle.
            // SAFETY: as above — `slot` is a writable element word of a table-owned
            // tuple, and `newcnt` is an immediate that owns no external storage.
            unsafe { *slot = newcnt };
        } else {
            // A bignum does not fit one element word: rebuild the tuple on the
            // caller's `heap` and store the new root. `counter_slot` established
            // that `object` is a live tuple and `1 <= pos <= arity`.
            // SAFETY: `object` is a live tuple (checked by `counter_slot`), so its
            // header word holds a valid arity.
            let arity = unsafe { tuple_size(object) };
            let rebuilt = replace_elements(object, arity, &[(pos, newcnt)], heap)?;
            self.buckets[bi][entry].objs[0] = rebuilt;
        }
        Ok(newcnt)
    }

    /// Replace element `pos` (1-based) of the object stored under `key` with a new
    /// value for each `(pos, value)` in `updates` (later entries win for a
    /// repeated position, matching ETS), building the replacement object tuple on
    /// `heap`. Returns whether `key` existed (`false` for an absent key, like
    /// `ets:update_element/3`). Only `set` is supported.
    ///
    /// Matches the verified `erl` behaviour:
    /// * absent `key` → `Ok(false)` (not an error);
    /// * `bag`/`duplicate_bag` → `badarg`;
    /// * any `pos` outside `1..=arity` (including `0`) → `badarg`, with **nothing**
    ///   written (every update is validated before any is applied);
    /// * an empty `updates` is a no-op that reports whether the key existed.
    ///
    /// Gaps: `pos == keypos` should be `badarg`, but the interior has no `keypos`
    /// (the caller must reject it). The returned object lives on `heap`, so the
    /// caller must keep `heap` alive and — per the A.1 copy boundary — copy the
    /// object into table-owned storage when `heap` is not the table's own store.
    pub fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        heap: &mut beam_alloc::Heap,
        atoms: &AtomTable,
    ) -> Result<bool> {
        if self.kind != TableKind::Set {
            return Err(badarg());
        }
        let bi = self.bucket_index(key, atoms);
        let entry = match Self::entry_pos(&self.buckets[bi], key, atoms) {
            Some(pos) => pos,
            None => return Ok(false),
        };
        let object = *self.buckets[bi][entry].objs.first().ok_or_else(badarg)?;
        // SAFETY: a stored object is a live tuple (validated before it was
        // inserted), so reading its header word / arity is well defined.
        if !unsafe { is_tuple(object) } {
            return Err(badarg());
        }
        // SAFETY: `object` is a live tuple (checked above).
        let arity = unsafe { tuple_size(object) };
        // Validate every position up front: ETS applies no update at all if any
        // element spec is invalid.
        for &(pos, _) in updates {
            if pos == 0 || pos > arity {
                return Err(badarg());
            }
        }
        if updates.is_empty() {
            // Nothing to change; the key existed, so ETS reports `true`.
            return Ok(true);
        }
        let new_object = replace_elements(object, arity, updates, heap)?;
        // Replace the stored object with the freshly built tuple.
        self.buckets[bi][entry].objs[0] = new_object;
        Ok(true)
    }

    /// An honest, non-negative word count of the interior's backing storage.
    ///
    /// Counts the bucket array, every bucket's entry vector and each entry's key
    /// plus its object words, all measured at **capacity**, so the figure never
    /// under-reports what is actually allocated. It rises as objects are inserted
    /// (the bucket array doubles past the load factor and each `Vec` grows) and
    /// **falls again as they are removed**: a delete that drops live occupancy below
    /// an eighth of the bucket array rebuilds it at a proportional size, and an
    /// entry that keeps far fewer objects than it can releases the surplus — so a
    /// table that grew to N objects and then shrank to one reports storage
    /// proportional to the one it still holds, not to the high-water N.
    /// `_atoms` is accepted for signature symmetry with the other `&AtomTable`
    /// operations: atom storage is global (the runtime's `AtomTable`), not owned by
    /// the interior, so it contributes no words here.
    pub fn memory(&self, _atoms: &AtomTable) -> usize {
        self.memory_words()
    }

    /// The interior's word count, shared by the inherent
    /// [`memory`](HashInterior::memory) and the [`Interior::memory`] seam (which
    /// has no `AtomTable` parameter).
    fn memory_words(&self) -> usize {
        let mut words = self.buckets.capacity() * words_of::<Vec<Entry>>();
        for bucket in &self.buckets {
            words += bucket.capacity() * words_of::<Entry>();
            for entry in bucket {
                // The key word plus the object words.
                words += 1 + entry.objs.capacity();
            }
        }
        words
    }
}

impl Interior for HashInterior {
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        let bi = self.bucket_index(key, atoms);
        // Whether the interior now owns `object`. `set`/`duplicate_bag` always
        // retain; only a `bag` can reject an *identical* object as a no-op, in which
        // case the caller reclaims the rejected copy (see the trait doc).
        let mut retained = true;
        match Self::entry_pos(&self.buckets[bi], key, atoms) {
            Some(pos) => {
                let entry = &mut self.buckets[bi][pos];
                match self.kind {
                    // `set`/`ordered_set` keep at most one object per key.
                    TableKind::Set | TableKind::OrderedSet => {
                        match entry.objs.first_mut() {
                            Some(slot) => *slot = object,
                            None => {
                                entry.objs.push(object);
                                self.len += 1;
                            }
                        }
                        // The replacement object now anchors the entry: re-point the
                        // stored key term at the *incoming* object's own key element,
                        // which the caller (`Table::insert_stored`) guarantees is a
                        // term inside `object` (table-owned and live). Leaving the old
                        // key — which aliased the object the caller is about to
                        // reclaim — would make it dangle and later read a reused word
                        // as a different term.
                        entry.key = key;
                    }
                    // `bag` keeps *distinct* objects: an identical object is a no-op.
                    TableKind::Bag => {
                        if entry.objs.iter().any(|o| eq(*o, object, atoms)) {
                            retained = false;
                        } else {
                            entry.objs.push(object);
                            self.len += 1;
                        }
                    }
                    // `duplicate_bag` appends unconditionally.
                    TableKind::DuplicateBag => {
                        entry.objs.push(object);
                        self.len += 1;
                    }
                }
            }
            None => {
                self.buckets[bi].push(Entry {
                    key,
                    objs: vec![object],
                });
                self.len += 1;
            }
        }
        self.maybe_grow(atoms);
        Ok(retained)
    }

    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        let bi = self.bucket_index(key, atoms);
        if Self::entry_pos(&self.buckets[bi], key, atoms).is_some() {
            return Ok(false);
        }
        self.buckets[bi].push(Entry {
            key,
            objs: vec![object],
        });
        self.len += 1;
        self.maybe_grow(atoms);
        Ok(true)
    }

    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        let bi = self.bucket_index(key, atoms);
        if let Some(pos) = Self::entry_pos(&self.buckets[bi], key, atoms) {
            let entry = self.buckets[bi].remove(pos);
            self.len -= entry.objs.len();
            // An object left the index, so the bucket capacity may now be far
            // larger than the live contents justify (see `maybe_shrink`).
            self.maybe_shrink(atoms);
        }
        Ok(())
    }

    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        let bi = self.bucket_index(key, atoms);
        if let Some(pos) = Self::entry_pos(&self.buckets[bi], key, atoms) {
            let keypos = self.keypos;
            let (empty, removed) = {
                let entry = &mut self.buckets[bi][pos];
                let before = entry.objs.len();
                let previous_key = entry.key;
                entry.objs.retain(|o| !eq(*o, object, atoms));
                let removed = before - entry.objs.len();
                self.len -= removed;
                if let Some(&survivor) = entry.objs.first() {
                    // The entry survives on other objects. Its stored key may have
                    // aliased the object just removed (which the caller is about to
                    // reclaim), so re-anchor it to a surviving — hence live,
                    // table-owned — object's own key element. In the degenerate
                    // `object == key` callers (unit tests, `interior_bench`) the
                    // object is not a tuple and `key_of` cannot derive a key; keep
                    // the existing key term there.
                    entry.key = key_of(survivor, keypos).unwrap_or(previous_key);
                    // A key that kept many objects now holds far fewer: give the
                    // surplus words back (hysteresis: only past a 4× ratio).
                    capacity::trim_objs(&mut entry.objs);
                }
                (entry.objs.is_empty(), removed)
            };
            if empty {
                self.buckets[bi].remove(pos);
            }
            if removed > 0 {
                self.maybe_shrink(atoms);
            }
        }
        Ok(())
    }

    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {        let bi = self.bucket_index(key, atoms);
        match Self::entry_pos(&self.buckets[bi], key, atoms) {
            Some(pos) => self.buckets[bi][pos].objs.clone(),
            None => Vec::new(),
        }
    }

    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        let bi = self.bucket_index(key, atoms);
        Self::entry_pos(&self.buckets[bi], key, atoms).is_some()
    }

    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        let bi = self.bucket_index(key, atoms);
        let entry = Self::entry_pos(&self.buckets[bi], key, atoms)
            .map(|pos| &self.buckets[bi][pos])
            .ok_or_else(badarg)?;
        // A multi kind may hold several objects under `key`; `lookup_element/3`
        // reads the first, matching the per-key "the object" of the hash kinds.
        let object = *entry.objs.first().ok_or_else(badarg)?;
        // `pos` is a 0-based element index; `key_of` takes a 1-based position and
        // rejects a non-tuple object or an out-of-range position with `badarg`.
        let keypos = pos.checked_add(1).ok_or_else(badarg)?;
        key_of(object, keypos)
    }

    fn size(&self) -> usize {
        self.len
    }

    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        Box::new(
            self.buckets
                .iter()
                .flat_map(|bucket| bucket.iter())
                .flat_map(|entry| entry.objs.iter().copied()),
        )
    }

    fn take(&mut self, key: Eterm, atoms: &AtomTable) -> Result<Vec<Eterm>> {
        // The inherent operation removes the entry in one pass and hands back its
        // objects, which is exactly the `ets:take/2` result.
        Ok(HashInterior::take(self, key, atoms))
    }

    fn keys(&self) -> Vec<Eterm> {
        // The interior's own iteration order: the bucket scan `iter_objects` walks,
        // with each entry's key reported once (the multi kinds store several
        // objects per key).
        self.buckets
            .iter()
            .flat_map(|bucket| bucket.iter())
            .map(|entry| entry.key)
            .collect()
    }

    /// `ets:last/1` for a hash kind is `ets:first/1`
    /// (`erl_db_hash.c:853-856`: `db_first_hash, db_next_hash, db_first_hash /* last
    /// == first */, db_next_hash /* prev == next */`), so `last` is the first key of
    /// the bucket-scan order — no backward walk, no greatest-key search.
    fn last_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.first_key(atoms)
    }

    /// `ets:prev/2` for a hash kind is `ets:next/2` (the same C function table).
    fn prev_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.next_key(key, atoms)
    }

    /// The hash kinds' reverse walk is their forward one (`last`/`prev` are
    /// `first`/`next`), so this is [`iter_objects`](Interior::iter_objects) as-is.
    fn iter_objects_rev(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        self.iter_objects()
    }

    /// `ets:slot/2` for a hash kind: the objects of hash **bucket** `slot` — the
    /// list `erl_db_hash.c:1733` `db_slot_hash` builds from `BUCKET(tb, slot)`.
    ///
    /// The bucket-array length ([`Self::bucket_count`], the C `nactive`) is this
    /// interior's own: slots `0..len` **partition every** stored object (each
    /// object belongs to the one bucket its key hashes to, and a bucket holds all
    /// objects of every key that hashes to it), slot `len` is `'$end_of_table'`,
    /// and `slot > len` is `badarg` — the C's `slot < nactive` /
    /// `slot == nactive` / `slot > nactive` cases exactly.
    ///
    /// The bucket *count* is an interior freedom (`../CONTEXT.md` §2): ERTS
    /// derives its `nactive` from its own bucket allocation, so which slot a given
    /// object lands in is not reproducible — but the partition, the single
    /// end-of-table slot and the out-of-range failure are.
    fn slot(&self, slot: usize, _atoms: &AtomTable) -> Result<Option<Vec<Eterm>>> {
        let nactive = self.bucket_count();
        if slot > nactive {
            return Err(badarg());
        }
        if slot == nactive {
            return Ok(None);
        }
        // `slot < nactive` because of the two guards above.
        Ok(Some(
            self.buckets[slot]
                .iter()
                .flat_map(|entry| entry.objs.iter().copied())
                .collect(),
        ))
    }

    fn memory(&self) -> usize {
        self.memory_words()
    }

    fn update_counter(
        &mut self,
        key: Eterm,
        pos: usize,
        incr: Eterm,
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<Eterm> {
        // The hash index owns the stored object, so a small element is written
        // destructively; a bignum result rebuilds the tuple on `heap` (see the
        // inherent method).
        HashInterior::update_counter(self, key, pos, incr, atoms, heap)
    }

    fn update_element(
        &mut self,
        key: Eterm,
        updates: &[(usize, Eterm)],
        atoms: &AtomTable,
        heap: &mut beam_alloc::Heap,
    ) -> Result<bool> {
        HashInterior::update_element(self, key, updates, heap, atoms)
    }
}

/// The number of `Eterm`-sized words a value of type `T` occupies (rounded up).
fn words_of<T>() -> usize {
    size_of::<T>().div_ceil(size_of::<Eterm>())
}

