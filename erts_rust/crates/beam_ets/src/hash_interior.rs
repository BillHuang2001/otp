//! Hash-indexed interior for `set`, `bag` and `duplicate_bag`.
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
//! (OTP 29 / erts-17.1); the residual gaps (bignum counters, the `pos == keypos`
//! check) are called out on each method.

use core::mem::size_of;

use beam_common::{BeamError, Result};
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::{key_of, object_elements};
use crate::kind::TableKind;
use crate::table::Interior;

/// The starting bucket count of a fresh table (a power of two, so the modulo
/// index distributes evenly).
const INITIAL_BUCKETS: usize = 8;

/// Load-factor numerator: grow past 3/4 of a bucket per stored object.
const LOAD_FACTOR_NUM: usize = 3;
/// Load-factor denominator.
const LOAD_FACTOR_DEN: usize = 4;

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
    /// The bucket array.
    buckets: Vec<Vec<Entry>>,
    /// The total number of stored objects.
    len: usize,
}

impl HashInterior {
    /// A new empty interior for the given (hash-indexed) `kind`.
    pub fn new(kind: TableKind) -> HashInterior {
        HashInterior {
            kind,
            buckets: (0..INITIAL_BUCKETS).map(|_| Vec::new()).collect(),
            len: 0,
        }
    }

    /// The bucket a key hashes to. `buckets` is never empty (it starts at
    /// [`INITIAL_BUCKETS`] and only ever doubles), so the modulo is well defined.
    fn bucket_index(&self, key: Eterm, atoms: &AtomTable) -> usize {
        (term_hash(key, atoms) as usize) % self.buckets.len()
    }

    /// The position of `key`'s entry in `bucket`, if any. Term equality is
    /// delegated to `beam_term::compare::eq`.
    fn entry_pos(bucket: &[Entry], key: Eterm, atoms: &AtomTable) -> Option<usize> {
        bucket.iter().position(|e| eq(e.key, key, atoms))
    }

    /// Double the bucket array and reinsert every entry at its new index.
    fn grow(&mut self, atoms: &AtomTable) {
        let new_len = self.buckets.len() * 2;
        let mut grown: Vec<Vec<Entry>> = (0..new_len).map(|_| Vec::new()).collect();
        for bucket in self.buckets.drain(..) {
            for entry in bucket {
                let idx = (term_hash(entry.key, atoms) as usize) % new_len;
                grown[idx].push(entry);
            }
        }
        self.buckets = grown;
    }

    /// Grow once the object load factor passes ~3/4. `len` is the object count
    /// (what `size` reports); for the multi kinds it grows a little earlier than
    /// the entry count alone would, which is harmless.
    fn maybe_grow(&mut self, atoms: &AtomTable) {
        if self.len * LOAD_FACTOR_DEN > self.buckets.len() * LOAD_FACTOR_NUM {
            self.grow(atoms);
        }
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
                entry.objs
            }
            None => Vec::new(),
        }
    }

    /// Add small-integer `incr` (which may be negative) to element `pos` (1-based)
    /// of the object stored under `key`, writing the result back, and return the
    /// new counter value. Mirrors `ets:update_counter/3,4` for `set`.
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
    /// Gaps (documented, never a silently-wrong value):
    /// * `pos == keypos` should be `badarg`; the interior carries no `keypos`, so
    ///   the caller (the `Table`/BIF layer, which owns `keypos`) must reject it;
    /// * a bignum counter/increment, or a sum that does not fit a 60-bit small
    ///   integer (a real-ETS *success*, since it promotes to a bignum), reports
    ///   [`BeamError::SystemLimit`] — this crate has no bignum arithmetic;
    /// * the `{Pos, Incr, Threshold, SetValue}` threshold form has no parameter
    ///   here and is not applied.
    pub fn update_counter(
        &mut self,
        key: Eterm,
        pos: usize,
        incr: Eterm,
        atoms: &AtomTable,
    ) -> Result<Eterm> {
        if self.kind != TableKind::Set {
            return Err(badarg());
        }
        // The key must exist first: real ETS raises `badarg` here before it even
        // inspects the update operand.
        let bi = self.bucket_index(key, atoms);
        let object = match Self::entry_pos(&self.buckets[bi], key, atoms) {
            Some(pos) => *self.buckets[bi][pos].objs.first().ok_or_else(badarg)?,
            None => return Err(badarg()),
        };
        // Validate the position against the object's arity and locate the element.
        let slot = counter_slot(object, pos)?;
        // SAFETY: `slot` is element `pos` of a live tuple owned by the table (an
        // entry always stores a deep copy), so the word is an initialised term.
        let oldcnt = unsafe { *slot };
        let newcnt = add_counters(oldcnt, incr)?;
        // Destructive in-place update, exactly as `ets:update_counter` mutates the
        // stored object (the table owns this copy). The new value is a small
        // integer — an immediate — so it needs no allocation and cannot dangle.
        // SAFETY: as above — `slot` is a writable element word of a table-owned
        // tuple, and `newcnt` is an immediate that owns no external storage.
        unsafe { *slot = newcnt };
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
    /// under-reports what is actually allocated, and it never decreases as objects
    /// are inserted (the bucket array only doubles and each `Vec` only grows).
    /// `_atoms` is accepted for signature symmetry with the other `&AtomTable`
    /// operations: atom storage is global (the runtime's `AtomTable`), not owned by
    /// the interior, so it contributes no words here.
    pub fn memory(&self, _atoms: &AtomTable) -> usize {
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
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        let bi = self.bucket_index(key, atoms);
        match Self::entry_pos(&self.buckets[bi], key, atoms) {
            Some(pos) => {
                let entry = &mut self.buckets[bi][pos];
                match self.kind {
                    // `set`/`ordered_set` keep at most one object per key.
                    TableKind::Set | TableKind::OrderedSet => match entry.objs.first_mut() {
                        Some(slot) => *slot = object,
                        None => {
                            entry.objs.push(object);
                            self.len += 1;
                        }
                    },
                    // `bag` keeps *distinct* objects: an identical object is a no-op.
                    TableKind::Bag => {
                        if !entry.objs.iter().any(|o| eq(*o, object, atoms)) {
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
        Ok(())
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
        }
        Ok(())
    }

    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        let bi = self.bucket_index(key, atoms);
        if let Some(pos) = Self::entry_pos(&self.buckets[bi], key, atoms) {
            let empty = {
                let entry = &mut self.buckets[bi][pos];
                let before = entry.objs.len();
                entry.objs.retain(|o| !eq(*o, object, atoms));
                self.len -= before - entry.objs.len();
                entry.objs.is_empty()
            };
            if empty {
                self.buckets[bi].remove(pos);
            }
        }
        Ok(())
    }

    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        let bi = self.bucket_index(key, atoms);
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
}

/// Validate a 1-based element position against `object`'s arity and return a raw
/// pointer to that element word (for a destructive update).
///
/// `badarg` when `object` is not a tuple, or `pos` is outside `1..=arity`.
fn counter_slot(object: Eterm, pos: usize) -> Result<*mut Eterm> {
    if pos == 0 {
        return Err(badarg());
    }
    // SAFETY: `is_tuple` only reads the header word of a boxed term.
    if !unsafe { is_tuple(object) } {
        return Err(badarg());
    }
    // SAFETY: `object` is a live tuple (checked above).
    let arity = unsafe { tuple_size(object) };
    if pos > arity {
        return Err(badarg());
    }
    // SAFETY: `tuple_val` is the header pointer of the live tuple, and element
    // `pos` lives at `header + pos` (the header word occupies slot 0); `1 <= pos <=
    // arity` keeps the displacement within the tuple's `arity` element words.
    Ok(unsafe { tuple_val(object).add(pos) })
}

/// `old + incr` for two small-integer counter operands, or the closest
/// representable failure.
///
/// `badarg` for a non-integer operand (matching `ets:update_counter`). A bignum
/// operand is valid in real ETS but is not addable here, and a sum outside the
/// 60-bit small range (which real ETS promotes to a bignum) is likewise
/// unavailable: both report [`BeamError::SystemLimit`] rather than a wrong value.
fn add_counters(old: Eterm, incr: Eterm) -> Result<Eterm> {
    if !is_small(incr) {
        return Err(counter_operand_error(incr));
    }
    if !is_small(old) {
        return Err(counter_operand_error(old));
    }
    // Widen to `i128` so the 60-bit sum cannot wrap before it is range-checked.
    let sum = signed_value(old) as i128 + signed_value(incr) as i128;
    if sum < MIN_SMALL as i128 || sum > MAX_SMALL as i128 {
        // Real ETS promotes the sum to a bignum; this crate has no bignum support.
        return Err(BeamError::SystemLimit);
    }
    Ok(make_small(sum as i64))
}

/// The error for a counter operand that is not a small integer: `badarg` unless it
/// is a bignum (a valid ETS operand this crate cannot process), which is a
/// representation-limit gap.
fn counter_operand_error(x: Eterm) -> BeamError {
    // SAFETY: `x` is a live term (a stored element or a caller-supplied operand),
    // so classifying it through its header word is well defined.
    if unsafe { is_integer(x) } {
        BeamError::SystemLimit
    } else {
        badarg()
    }
}

/// Build a copy of the `arity`-tuple `object` on `heap`, overwriting position
/// `pos` (`1 <= pos <= arity`) with its `updates` value; later entries win.
fn replace_elements(
    object: Eterm,
    arity: usize,
    updates: &[(usize, Eterm)],
    heap: &mut beam_alloc::Heap,
) -> Result<Eterm> {
    // SAFETY: `object` is a live tuple with `arity` element words (established by
    // the caller) and stays valid for the duration of the copy below.
    let elements = unsafe { object_elements(object) };
    // SAFETY: `alloc_tuple` writes the arity header and hands back `arity`
    // uninitialised element words; the `copy_nonoverlapping` plus the update loop
    // write every one of them before `make_tuple` exposes the pointer as a term.
    unsafe {
        let hdr = heap.alloc_tuple(arity)?;
        core::ptr::copy_nonoverlapping(elements.as_ptr(), hdr.add(1), arity);
        for &(pos, value) in updates {
            // `pos` is 1-based; the header occupies slot 0, so element `pos` lives
            // at `hdr + pos` (in range, since the caller validated `pos <= arity`).
            *hdr.add(pos) = value;
        }
        Ok(make_tuple(hdr))
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

    #[test]
    fn update_counter_increments_decrements_and_writes_back() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        // Increment the element at pos 2, and check it was stored back.
        assert_eq!(
            t.update_counter(k, 2, make_small(5), &atoms).unwrap(),
            make_small(15)
        );
        assert_eq!(elem(t.lookup(k, &atoms)[0], 1), make_small(15));

        // A negative increment at a different position.
        assert_eq!(
            t.update_counter(k, 3, make_small(-25), &atoms).unwrap(),
            make_small(-5)
        );
        assert_eq!(
            t.update_counter(k, 3, make_small(7), &atoms).unwrap(),
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
        let k = make_small(1);

        // Absent key: real ETS raises `badarg` ("no object ... and no default").
        let mut set = HashInterior::new(TableKind::Set);
        assert!(matches!(
            set.update_counter(k, 2, make_small(1), &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(set.size(), 0, "a failed update must not create an object");

        // `bag` / `duplicate_bag` have no counters.
        for kind in [TableKind::Bag, TableKind::DuplicateBag] {
            let mut t = HashInterior::new(kind);
            t.insert(k, object(&mut heap, 1, 10), &atoms).unwrap();
            assert!(matches!(
                t.update_counter(k, 2, make_small(1), &atoms),
                Err(BeamError::BadArg)
            ));
        }
    }

    #[test]
    fn update_counter_position_and_type_errors_are_badarg() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let k = make_small(1);
        // {1, 10, 20}
        let object = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        let mut t = HashInterior::new(TableKind::Set);
        t.insert(k, object, &atoms).unwrap();

        let an_atom = make_atom(Atom::from_raw(0));
        // pos below 1 and above the arity are both `badarg`.
        assert!(matches!(
            t.update_counter(k, 0, make_small(1), &atoms),
            Err(BeamError::BadArg)
        ));
        assert!(matches!(
            t.update_counter(k, 4, make_small(1), &atoms),
            Err(BeamError::BadArg)
        ));
        // A non-integer increment is `badarg`.
        assert!(matches!(
            t.update_counter(k, 2, an_atom, &atoms),
            Err(BeamError::BadArg)
        ));
        // Nothing changed after the failures.
        assert_eq!(t.lookup(k, &atoms), vec![object]);

        // A non-integer *element* is `badarg` too.
        let k2 = make_small(2);
        let bad = heap.tuple(&[k2, an_atom]).unwrap();
        t.insert(k2, bad, &atoms).unwrap();
        assert!(matches!(
            t.update_counter(k2, 2, make_small(1), &atoms),
            Err(BeamError::BadArg)
        ));
        assert_eq!(t.lookup(k2, &atoms), vec![bad]);
    }

    #[test]
    fn update_counter_overflow_reports_the_representation_gap() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = HashInterior::new(TableKind::Set);

        // `MAX_SMALL + 1` is a bignum in real ETS; here it is an honest error, and
        // the stored value is left untouched (no partially-applied update).
        let hi = make_small(1);
        let object = heap.tuple(&[hi, make_small(MAX_SMALL)]).unwrap();
        t.insert(hi, object, &atoms).unwrap();
        assert!(matches!(
            t.update_counter(hi, 2, make_small(1), &atoms),
            Err(BeamError::SystemLimit)
        ));
        assert_eq!(elem(t.lookup(hi, &atoms)[0], 1), make_small(MAX_SMALL));

        // `MIN_SMALL - 1` likewise underflows the small range.
        let lo = make_small(2);
        let object = heap.tuple(&[lo, make_small(MIN_SMALL)]).unwrap();
        t.insert(lo, object, &atoms).unwrap();
        assert!(matches!(
            t.update_counter(lo, 2, make_small(-1), &atoms),
            Err(BeamError::SystemLimit)
        ));
        assert_eq!(elem(t.lookup(lo, &atoms)[0], 1), make_small(MIN_SMALL));

        // A sum landing exactly on the boundary still succeeds.
        assert_eq!(
            t.update_counter(hi, 2, make_small(MIN_SMALL), &atoms).unwrap(),
            make_small(-1)
        );
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
}
