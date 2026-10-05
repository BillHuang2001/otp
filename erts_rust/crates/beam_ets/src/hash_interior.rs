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

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::key_of;
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
}
