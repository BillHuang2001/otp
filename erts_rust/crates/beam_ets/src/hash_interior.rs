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
//! past a load factor. `set` keeps at most one object per key; `bag` and
//! `duplicate_bag` keep many (a `bag` rejects an *identical* object within a key,
//! a `duplicate_bag` does not). Nothing here re-implements term order or hashing.
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

use core::mem::size_of;

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::key_of;
use crate::kind::TableKind;
use crate::table::{add_counters, counter_slot, replace_elements, Interior};

/// The starting bucket count of a fresh table (a power of two, so the modulo
/// index distributes evenly).
const INITIAL_BUCKETS: usize = 8;

/// Load-factor numerator: grow past 3/4 of a bucket per stored object.
const LOAD_FACTOR_NUM: usize = 3;
/// Load-factor denominator.
const LOAD_FACTOR_DEN: usize = 4;

/// The **shrink** threshold's numerator: release bucket capacity while the object
/// load stays below 1/8 of the bucket count.
///
/// The grow threshold is 3/4, so the two leave a **6× band** (1/8 … 3/4) in which
/// neither happens. That hysteresis is what keeps a steady insert/delete churn at
/// a stable size from thrashing the bucket array: one delete cannot undo the array
/// a whole series of inserts paid for, and a rebuild is only ever amortized.
const SHRINK_NUM: usize = 1;
/// The **shrink** threshold's denominator (see [`SHRINK_NUM`]).
const SHRINK_DEN: usize = 8;

/// An entry's object vector is released back to its live length only once it holds
/// fewer than a quarter of the objects it can — the same hysteresis idea as
/// [`SHRINK_NUM`], applied to a single key's `bag`/`duplicate_bag` object list.
const OBJS_SHRINK_FACTOR: usize = 4;
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
            buckets: (0..INITIAL_BUCKETS).map(|_| Vec::new()).collect(),
            len: 0,
        }
    }

    /// The bucket a key hashes to. `buckets` is never empty (it starts at
    /// [`INITIAL_BUCKETS`], doubles past the load factor and never shrinks below
    /// the starting count), so the modulo is well defined.
    fn bucket_index(&self, key: Eterm, atoms: &AtomTable) -> usize {        (term_hash(key, atoms) as usize) % self.buckets.len()
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

    /// Double the bucket array and reinsert every entry at its new index.
    fn grow(&mut self, atoms: &AtomTable) {
        self.rehash_into(self.buckets.len() * 2, atoms);
    }

    /// Build a fresh bucket array of exactly `target` buckets and move every entry
    /// into it.
    ///
    /// A rebuild — rather than an in-place resize — is what makes the interior's
    /// stored capacity track its **live** contents: the new bucket vector is
    /// allocated at exactly `target`, and the buckets a stale (now empty) bucket
    /// vector used to hold are dropped entirely instead of lingering as empty
    /// minimum-capacity vectors. It serves both directions: [`grow`](Self::grow)
    /// doubles through it and [`maybe_shrink`](Self::maybe_shrink) halves through it.
    fn rehash_into(&mut self, target: usize, atoms: &AtomTable) {
        let mut rebuilt: Vec<Vec<Entry>> = Vec::with_capacity(target);
        rebuilt.resize_with(target, Vec::new);
        for bucket in self.buckets.drain(..) {
            for entry in bucket {
                let idx = (term_hash(entry.key, atoms) as usize) % target;
                rebuilt[idx].push(entry);
            }
        }
        self.buckets = rebuilt;
    }

    /// Grow once the object load factor passes ~3/4. `len` is the object count
    /// (what `size` reports); for the multi kinds it grows a little earlier than
    /// the entry count alone would, which is harmless.
    fn maybe_grow(&mut self, atoms: &AtomTable) {
        if self.len * LOAD_FACTOR_DEN > self.buckets.len() * LOAD_FACTOR_NUM {
            self.grow(atoms);
        }
    }

    /// Release bucket-array capacity once the object load has fallen well below it
    /// (`len / buckets < 1/8`), so a table that grew large and then shrank reports —
    /// and holds — storage proportional to what it still **stores**, exactly like
    /// the system `ets` (whose hash tables give their buckets back on deletion).
    ///
    /// Only a bucket array above [`INITIAL_BUCKETS`] is a candidate, and the
    /// hysteresis band documented on [`SHRINK_NUM`] means a stable-size
    /// insert/delete churn never triggers a rebuild: the array only shrinks after
    /// live occupancy has fallen to an eighth of it.
    fn maybe_shrink(&mut self, atoms: &AtomTable) {
        let buckets = self.buckets.len();
        if buckets <= INITIAL_BUCKETS {
            return;
        }
        if self.len * SHRINK_DEN >= buckets * SHRINK_NUM {
            return;
        }
        // Halve while the load stays under the threshold, so one long teardown
        // reaches a bucket count proportional to the surviving objects instead of
        // paying a rebuild per deletion.
        let mut target = buckets / 2;
        while target > INITIAL_BUCKETS && self.len * SHRINK_DEN < target * SHRINK_NUM {
            target /= 2;
        }
        self.rehash_into(target, atoms);
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
    pub fn memory(&self, _atoms: &AtomTable) -> usize {        self.memory_words()
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
                    if entry.objs.capacity() > OBJS_SHRINK_FACTOR * entry.objs.len() {
                        entry.objs.shrink_to_fit();
                    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_term::heap::Heap;

    /// A `{Key, Value}` object tuple on `heap` (the key element repeated as the
    /// tuple's first element, mirroring `keypos = 1`).
    fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    #[test]
    fn set_insert_lookup_member_delete() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = HashInterior::new(TableKind::Set);

        let k = make_small(1);
        let o1 = object(&mut heap, 1, 10);
        assert!(!t.member(k, &atoms));
        assert!(t.lookup(k, &atoms).is_empty());

        t.insert(k, o1, &atoms).unwrap();
        assert!(t.member(k, &atoms));
        assert_eq!(t.lookup(k, &atoms), vec![o1]);
        assert_eq!(t.size(), 1);

        // Re-inserting the same key *replaces* the object (size stays 1).
        let o2 = object(&mut heap, 1, 20);
        t.insert(k, o2, &atoms).unwrap();
        assert_eq!(t.lookup(k, &atoms), vec![o2]);
        assert_eq!(t.size(), 1);

        // `insert_new` on an existing key is a no-op returning `false`.
        let o3 = object(&mut heap, 1, 30);
        assert!(!t.insert_new(k, o3, &atoms).unwrap());
        assert_eq!(t.lookup(k, &atoms), vec![o2]);
        assert_eq!(t.size(), 1);

        // `insert_new` on a fresh key inserts and returns `true`.
        let k2 = make_small(2);
        let o4 = object(&mut heap, 2, 40);
        assert!(t.insert_new(k2, o4, &atoms).unwrap());
        assert_eq!(t.lookup(k2, &atoms), vec![o4]);
        assert_eq!(t.size(), 2);

        // `delete` drops the whole entry, and is a no-op for an absent key.
        t.delete(k, &atoms).unwrap();
        assert!(!t.member(k, &atoms));
        assert_eq!(t.size(), 1);
        t.delete(make_small(99), &atoms).unwrap();
        assert_eq!(t.size(), 1);

        // `delete_object` removes only a matching object.
        t.delete_object(k2, o4, &atoms).unwrap();
        assert!(!t.member(k2, &atoms));
        assert_eq!(t.size(), 0);
        assert!(t.iter_objects().next().is_none());
    }

    #[test]
    fn rehash_keeps_every_key_lookupable() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(4096);
        let mut t = HashInterior::new(TableKind::Set);

        const N: i64 = 300;
        let keys: Vec<Eterm> = (0..N).map(make_small).collect();
        let objs: Vec<Eterm> = (0..N).map(|i| object(&mut heap, i, i * 2)).collect();

        for i in 0..N as usize {
            t.insert(keys[i], objs[i], &atoms).unwrap();
        }
        assert_eq!(t.size(), N as usize);
        // The bucket array must have grown (and rehashed) well past its start.
        assert!(t.buckets.len() > INITIAL_BUCKETS);

        for i in 0..N as usize {
            assert_eq!(t.lookup(keys[i], &atoms), vec![objs[i]], "key {i}");
            assert!(t.member(keys[i], &atoms), "key {i}");
        }
        assert_eq!(t.iter_objects().count(), N as usize);

        // Deletion still locates every key after the rehash.
        for i in 0..N as usize {
            t.delete(keys[i], &atoms).unwrap();
        }
        assert_eq!(t.size(), 0);
        assert!(!t.member(make_small(0), &atoms));
    }

    #[test]
    fn bag_rejects_identical_objects_while_duplicate_bag_allows_them() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(7);
        let o1 = object(&mut heap, 7, 1);
        let o2 = object(&mut heap, 7, 2);

        let mut bag = HashInterior::new(TableKind::Bag);
        bag.insert(k, o1, &atoms).unwrap();
        // An identical object is a no-op for a bag.
        bag.insert(k, o1, &atoms).unwrap();
        assert_eq!(bag.lookup(k, &atoms), vec![o1]);
        assert_eq!(bag.size(), 1);
        // A structurally different object is appended.
        bag.insert(k, o2, &atoms).unwrap();
        assert_eq!(bag.lookup(k, &atoms), vec![o1, o2]);
        assert_eq!(bag.size(), 2);
        // `delete_object` removes only the equal object.
        bag.delete_object(k, o1, &atoms).unwrap();
        assert_eq!(bag.lookup(k, &atoms), vec![o2]);
        assert_eq!(bag.size(), 1);

        let mut dup = HashInterior::new(TableKind::DuplicateBag);
        dup.insert(k, o1, &atoms).unwrap();
        // Identical objects are all kept by a duplicate_bag.
        dup.insert(k, o1, &atoms).unwrap();
        assert_eq!(dup.lookup(k, &atoms), vec![o1, o1]);
        assert_eq!(dup.size(), 2);
        // `delete_object` removes every equal object (the entry then disappears).
        dup.delete_object(k, o1, &atoms).unwrap();
        assert_eq!(dup.size(), 0);
        assert!(!dup.member(k, &atoms));
    }

    #[test]
    fn size_counts_objects_not_keys() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = HashInterior::new(TableKind::DuplicateBag);
        let k = make_small(1);
        for v in 0..5 {
            t.insert(k, object(&mut heap, 1, v), &atoms).unwrap();
        }
        assert_eq!(t.size(), 5);
        assert_eq!(t.lookup(k, &atoms).len(), 5);
        assert_eq!(t.iter_objects().count(), 5);
    }

    #[test]
    fn lookup_element_reads_the_object_and_rejects_out_of_range() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = HashInterior::new(TableKind::Set);

        let k = make_small(1);
        let o = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        t.insert(k, o, &atoms).unwrap();

        // `pos` is 0-based.
        assert_eq!(t.lookup_element(k, 0, &atoms).unwrap(), k);
        assert_eq!(t.lookup_element(k, 1, &atoms).unwrap(), make_small(10));
        assert_eq!(t.lookup_element(k, 2, &atoms).unwrap(), make_small(20));

        // Out-of-range position and absent key are both `badarg`.
        assert!(matches!(
            t.lookup_element(k, 3, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(k, usize::MAX, &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.lookup_element(make_small(99), 0, &atoms),
            Err(BeamError::BadArg)
        ));

        // A non-tuple object has no elements.
        let k2 = make_small(2);
        t.insert(k2, make_small(5), &atoms).unwrap();
        assert!(matches!(
            t.lookup_element(k2, 0, &atoms),
            Err(BeamError::BadArg)
        ));
    }

    /// Read element `i` (0-based) of a stored `object` (a live tuple).
    fn elem(object: Eterm, i: usize) -> Eterm {
        // SAFETY: `object` is a live tuple; the callers pass in-range indices.
        unsafe { tuple_element(object, i) }
    }

    #[test]
    fn take_returns_and_removes_the_keys_objects() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(1);

        let mut set = HashInterior::new(TableKind::Set);
        let o = object(&mut heap, 1, 10);
        set.insert(k, o, &atoms).unwrap();
        assert_eq!(set.take(k, &atoms), vec![o]);
        assert!(!set.member(k, &atoms));
        assert_eq!(set.size(), 0);
        // A second `take` (and any absent key) yields nothing.
        assert!(set.take(k, &atoms).is_empty());
        assert!(set.take(make_small(42), &atoms).is_empty());

        // A `duplicate_bag` hands back every object, in insertion order.
        let mut dup = HashInterior::new(TableKind::DuplicateBag);
        let a = object(&mut heap, 7, 1);
        let b = object(&mut heap, 7, 2);
        dup.insert(make_small(7), a, &atoms).unwrap();
        dup.insert(make_small(7), b, &atoms).unwrap();
        assert_eq!(dup.take(make_small(7), &atoms), vec![a, b]);
        assert_eq!(dup.size(), 0);
        assert!(!dup.member(make_small(7), &atoms));
    }

    /// A bignum term of `digits` (least significant first) built on the caller's
    /// `beam_alloc::Heap` — the heap an `update_counter` bignum result lands on.
    fn big(heap: &mut beam_alloc::Heap, negative: bool, digits: &[u64]) -> Eterm {
        // SAFETY: `alloc_words` hands out exactly `big_size_words(n)` writable
        // words, which `write_big` fills completely.
        let hp = unsafe { heap.alloc_words(big_size_words(digits.len())).unwrap() };
        // SAFETY: `hp` addresses `big_size_words(n)` words and `digits` is non-empty.
        unsafe { write_big(hp, negative, digits) }
    }

    /// The `(negative, magnitude)` view of an integer term, in the same
    /// little-endian limb representation (leading zero limbs stripped) the
    /// production arithmetic uses — so a test can assert the exact value
    /// independent of its small/bignum encoding.
    fn magnitude_of(term: Eterm) -> (bool, Vec<u64>) {
        let (negative, mut digits) = if is_small(term) {
            let value = signed_value(term);
            (value < 0, vec![value.unsigned_abs()])
        } else {
            assert!(unsafe { is_big(term) }, "the test only asks for integers");
            // SAFETY: `term` is a live bignum, so `big_parts` reads its header and
            // digit words.
            let (negative, digits) = unsafe { big_parts(term) };
            (negative, digits.to_vec())
        };
        while matches!(digits.last(), Some(0)) {
            digits.pop();
        }
        (negative, digits)
    }

    #[test]
    fn update_counter_increments_decrements_and_writes_back() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        // Increment the element at pos 2, and check it was stored back.
        assert_eq!(
            t.update_counter(k, 2, make_small(5), &atoms, &mut out)
                .unwrap(),
            make_small(15)
        );
        assert_eq!(elem(t.lookup(k, &atoms)[0], 1), make_small(15));

        // A negative increment at a different position.
        assert_eq!(
            t.update_counter(k, 3, make_small(-25), &atoms, &mut out)
                .unwrap(),
            make_small(-5)
        );
        assert_eq!(
            t.update_counter(k, 3, make_small(7), &atoms, &mut out)
                .unwrap(),
            make_small(2)
        );
        // Counter updates never change the object count (set: one object per key).
        assert_eq!(t.size(), 1);
        assert_eq!(t.lookup(k, &atoms).len(), 1);
    }

    #[test]
    fn update_counter_absent_key_and_wrong_kind_are_badarg() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);

        // Absent key: real ETS raises `badarg` ("no object ... and no default").
        let mut set = HashInterior::new(TableKind::Set);
        assert!(matches!(
            set.update_counter(k, 2, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert_eq!(set.size(), 0, "a failed update must not create an object");

        // `bag` / `duplicate_bag` have no counters.
        for kind in [TableKind::Bag, TableKind::DuplicateBag] {
            let mut t = HashInterior::new(kind);
            t.insert(k, object(&mut heap, 1, 10), &atoms).unwrap();
            assert!(matches!(
                t.update_counter(k, 2, make_small(1), &atoms, &mut out),
                Err(BeamError::BadArg)
            ));
        }
    }

    #[test]
    fn update_counter_position_and_type_errors_are_badarg() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        let an_atom = make_atom(Atom::from_raw(0));
        // pos below 1 and above the arity are both `badarg`.
        assert!(matches!(
            t.update_counter(k, 0, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.update_counter(k, 4, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // A non-integer increment is `badarg`.
        assert!(matches!(
            t.update_counter(k, 2, an_atom, &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        // Nothing changed after the failures.
        assert_eq!(t.lookup(k, &atoms), vec![object]);

        // A non-integer *element* is `badarg` too.
        let k2 = make_small(2);
        let bad = heap.tuple(&[k2, an_atom]).unwrap();
        t.insert(k2, bad, &atoms).unwrap();
        assert!(matches!(
            t.update_counter(k2, 2, make_small(1), &atoms, &mut out),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.lookup(k2, &atoms), vec![bad]);
    }

    #[test]
    fn update_counter_promotes_out_of_range_sums_to_bignums() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(256).expect("a test heap is allocatable");
        let mut t = HashInterior::new(TableKind::Set);

        // `MAX_SMALL + 1` leaves the 60-bit range: real ETS promotes to a bignum.
        let hi = make_small(1);
        let object = heap.tuple(&[hi, make_small(MAX_SMALL)]).unwrap();
        t.insert(hi, object, &atoms).unwrap();
        let sum = t
            .update_counter(hi, 2, make_small(1), &atoms, &mut out)
            .unwrap();
        assert!(unsafe { is_big(sum) });
        // `2^59` is one limb and does not fit a small.
        assert_eq!(magnitude_of(sum), (false, vec![1u64 << 59]));
        // The stored object now holds the bignum (the interior rebuilt it).
        assert_eq!(elem(t.lookup(hi, &atoms)[0], 1), sum);

        // Subtracting back lands exactly on `MAX_SMALL` — normalised to a small.
        let back = t
            .update_counter(hi, 2, make_small(-1), &atoms, &mut out)
            .unwrap();
        assert_eq!(back, make_small(MAX_SMALL));

        // `MIN_SMALL - 1` underflows the small range the same way.
        let lo = make_small(2);
        let object = heap.tuple(&[lo, make_small(MIN_SMALL)]).unwrap();
        t.insert(lo, object, &atoms).unwrap();
        let sum = t
            .update_counter(lo, 2, make_small(-1), &atoms, &mut out)
            .unwrap();
        assert!(unsafe { is_big(sum) });
        assert_eq!(magnitude_of(sum), (true, vec![(1u64 << 59) + 1]));

        // A sum landing exactly on the boundary is still a small.
        assert_eq!(
            t.update_counter(lo, 2, make_small(1), &atoms, &mut out)
                .unwrap(),
            make_small(MIN_SMALL)
        );
    }

    #[test]
    fn update_counter_accepts_bignum_elements_and_operands() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(256).expect("a test heap is allocatable");
        let mut t = HashInterior::new(TableKind::Set);
        let k = make_small(1);

        // Element `2^64` (two limbs `[0, 1]`), a bignum.
        let element = big(&mut out, false, &[0, 1]);
        let object = heap.tuple(&[k, element]).unwrap();
        t.insert(k, object, &atoms).unwrap();

        // bignum element + small increment.
        let sum = t
            .update_counter(k, 2, make_small(10), &atoms, &mut out)
            .unwrap();
        assert_eq!(magnitude_of(sum), (false, vec![10, 1]));

        // bignum element + bignum increment (`2^64 + 10` plus `2^64 + 5`).
        let operand = big(&mut out, false, &[5, 1]);
        let sum = t.update_counter(k, 2, operand, &atoms, &mut out).unwrap();
        assert_eq!(magnitude_of(sum), (false, vec![15, 2]));

        // A bignum that exactly cancels the element leaves the small zero.
        let operand = big(&mut out, true, &[15, 2]);
        let sum = t.update_counter(k, 2, operand, &atoms, &mut out).unwrap();
        assert_eq!(sum, make_small(0));
    }
    #[test]
    fn update_element_replaces_elements_and_reports_existence() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20, 30}
        let object = heap
            .tuple(&[k, make_small(10), make_small(20), make_small(30)])
            .unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        // A single element update returns `true` and rewrites only that element.
        assert!(t
            .update_element(k, &[(2, make_small(99))], &mut out, &atoms)
            .unwrap());
        let stored = t.lookup(k, &atoms)[0];
        assert_eq!(elem(stored, 0), k);
        assert_eq!(elem(stored, 1), make_small(99));
        assert_eq!(elem(stored, 2), make_small(20));
        assert_eq!(elem(stored, 3), make_small(30));

        // Later entries win for a repeated position.
        assert!(t
            .update_element(k, &[(2, make_small(1)), (2, make_small(5))], &mut out, &atoms)
            .unwrap());
        assert_eq!(elem(t.lookup(k, &atoms)[0], 1), make_small(5));

        // An empty update list is a no-op that still reports the key exists.
        assert!(t.update_element(k, &[], &mut out, &atoms).unwrap());

        // An absent key reports `false` — not an error.
        assert!(!t
            .update_element(make_small(42), &[(2, make_small(9))], &mut out, &atoms)
            .unwrap());
        assert_eq!(t.size(), 1);
    }

    #[test]
    fn update_element_out_of_range_or_wrong_kind_is_badarg() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut out = beam_alloc::Heap::new(64).expect("a test heap is allocatable");
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        // pos 0 and pos > arity are `badarg`, and nothing is written.
        for bad in [0usize, 4] {
            assert!(matches!(
                t.update_element(k, &[(bad, NIL)], &mut out, &atoms),
                Err(BeamError::BadArg)
            ));
        }
        // Validation is all-or-nothing across the list.
        assert!(matches!(
            t.update_element(k, &[(2, NIL), (9, NIL)], &mut out, &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.lookup(k, &atoms), vec![object]);

        // `bag` is rejected with `badarg`.
        let mut bag = HashInterior::new(TableKind::Bag);
        bag.insert(k, object, &atoms).unwrap();
        assert!(matches!(
            bag.update_element(k, &[(2, NIL)], &mut out, &atoms),
            Err(BeamError::BadArg)
        ));
    }

    #[test]
    fn memory_is_a_non_negative_honest_word_count() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(4096);
        let mut t = HashInterior::new(TableKind::Set);

        // The count is a `usize` (non-negative by construction); even an empty
        // table owns its bucket array, so it is strictly positive.
        let empty = t.memory(&atoms);
        assert!(empty > 0);

        // Inserting only ever adds storage, so the count never decreases.
        let mut prev = empty;
        for i in 0..200i64 {
            t.insert(make_small(i), object(&mut heap, i, i), &atoms).unwrap();
            let now = t.memory(&atoms);
            assert!(now >= prev, "count shrank on insert {i} ({prev} -> {now})");
            prev = now;
        }
        let populated = t.memory(&atoms);
        assert!(populated >= empty);
        // Never under-reports: at least one word per stored object (keys, buckets
        // and vector overhead only add to it).
        assert!(populated >= t.size());
        assert_eq!(t.size(), 200);
    }

    #[test]
    fn insert_reports_whether_the_object_was_retained() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(7);

        // `set`: a fresh insert and a replace both retain the object.
        let mut set = HashInterior::new(TableKind::Set);
        assert!(set.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert!(set.insert(k, object(&mut heap, 7, 2), &atoms).unwrap(), "a replace retains");

        // `bag`: an identical object is rejected (`false`), a distinct one is kept.
        let mut bag = HashInterior::new(TableKind::Bag);
        assert!(bag.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert!(
            !bag.insert(k, object(&mut heap, 7, 1), &atoms).unwrap(),
            "an identical `bag` object is not retained"
        );
        assert!(bag.insert(k, object(&mut heap, 7, 2), &atoms).unwrap());
        assert_eq!(bag.size(), 2);

        // `duplicate_bag`: every object is retained, even an identical one.
        let mut dup = HashInterior::new(TableKind::DuplicateBag);
        assert!(dup.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert!(dup.insert(k, object(&mut heap, 7, 1), &atoms).unwrap());
        assert_eq!(dup.size(), 2);
    }
    /// The bucket array gives capacity back as objects leave: a table that grew to
    /// thousands of objects and then shrank to one rebuilds its buckets down to the
    /// starting count (never below it), so `ets:info(T, memory)` reports — and the
    /// interior holds — storage proportional to what it still **stores**.
    #[test]
    fn the_bucket_array_shrinks_back_to_the_live_object_count() {
        let atoms = AtomTable::new();
        const N: i64 = 3_000;
        let mut heap = Heap::new(8 * N as usize);
        let mut t = HashInterior::new(TableKind::Set);
        let empty = t.memory(&atoms);
        let objs: Vec<Eterm> = (0..N).map(|i| object(&mut heap, i, i * 2)).collect();
        for i in 0..N as usize {
            t.insert(make_small(i as i64), objs[i], &atoms).unwrap();
        }
        let populated = t.memory(&atoms);
        assert!(
            t.bucket_count() > INITIAL_BUCKETS,
            "the bucket array grew past its starting count ({})",
            t.bucket_count()
        );
        // Remove all but the last key: the load falls far below 1/8, so the array is
        // rebuilt at a proportional size.
        for i in 0..N - 1 {
            t.delete(make_small(i), &atoms).unwrap();
        }
        assert_eq!(t.size(), 1);
        let one_left = t.memory(&atoms);
        assert_eq!(
            t.bucket_count(),
            INITIAL_BUCKETS,
            "the bucket array stops at its starting count, never below it"
        );
        assert!(
            one_left < 10_000,
            "one object left must not keep the {N}-object index ({one_left} words, \
             populated {populated})"
        );
        assert!(
            one_left * 100 < populated,
            "the one-object figure is far below the loaded one ({one_left} vs {populated})"
        );
        assert!(one_left > empty, "the surviving entry still owns storage");
        // The surviving key is still reachable — a rebuild must not lose an entry.
        assert_eq!(t.lookup(make_small(N - 1), &atoms), vec![objs[N as usize - 1]]);
        // Emptying the interior releases the rest: it is back at (or below) its
        // starting footprint, with the starting bucket count.
        t.delete(make_small(N - 1), &atoms).unwrap();
        assert_eq!(t.bucket_count(), INITIAL_BUCKETS);
        // The emptied interior is back at its starting allocation (the bucket array
        // plus whatever the now-empty bucket vectors reserved); the **exact** empty
        // floor is `Table::release_empty_interior`'s fresh rebuild, pinned by
        // `tests/ets_capacity.rs`.
        assert!(
            t.memory(&atoms) <= empty + 32,
            "an emptied interior owns no surplus ({} words, fresh floor {empty})",
            t.memory(&atoms)
        );
    }
    /// A stable-size insert/delete churn must neither grow the interior nor make its
    /// bucket array shrink and re-grow: the grow (3/4 load) and shrink (1/8 load)
    /// thresholds leave a 6× hysteresis band the churn never crosses.
    #[test]
    fn a_stable_size_churn_neither_grows_nor_thrashes_the_bucket_array() {
        let atoms = AtomTable::new();
        const KEYS: i64 = 500;
        const ROUNDS: usize = 200;
        let mut heap = Heap::new(4 * KEYS as usize);
        let mut t = HashInterior::new(TableKind::Set);
        let objs: Vec<Eterm> = (0..KEYS).map(|i| object(&mut heap, i, i)).collect();
        let keys: Vec<Eterm> = (0..KEYS).map(make_small).collect();
        for i in 0..KEYS as usize {
            t.insert(keys[i], objs[i], &atoms).unwrap();
        }
        let base = t.memory(&atoms);
        let base_buckets = t.bucket_count();
        let (mut min, mut max) = (base, base);
        for _ in 0..ROUNDS {
            // Remove and immediately re-insert every other key: the occupancy never
            // leaves its steady state, so neither threshold may fire.
            for (i, key) in keys.iter().enumerate().step_by(2) {
                t.delete(*key, &atoms).unwrap();
                t.insert(*key, objs[i], &atoms).unwrap();
            }
            let now = t.memory(&atoms);
            min = min.min(now);
            max = max.max(now);
            assert_eq!(t.size(), KEYS as usize, "the round ends at a stable size");
        }
        assert!(
            max <= base,
            "the churn grew the interior above its warm-up figure (base {base}, max {max})"
        );
        assert!(
            min * 8 >= base * 7,
            "the churn shrank the interior by more than an eighth (base {base}, min {min})"
        );
        assert_eq!(
            t.bucket_count(),
            base_buckets,
            "the bucket array must not shrink/re-grow during a stable-size churn"
        );
    }
}
