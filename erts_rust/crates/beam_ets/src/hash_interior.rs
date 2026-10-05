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

use crate::kind::TableKind;
use crate::table::Interior;

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
        unimplemented!("M1: implementation pending")
    }
}

impl Interior for HashInterior {
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        unimplemented!("M1: implementation pending")
    }

    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        unimplemented!("M1: implementation pending")
    }

    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        unimplemented!("M1: implementation pending")
    }

    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        unimplemented!("M1: implementation pending")
    }

    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        unimplemented!("M1: implementation pending")
    }

    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        unimplemented!("M1: implementation pending")
    }

    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        unimplemented!("M1: implementation pending")
    }

    fn size(&self) -> usize {
        unimplemented!("M1: implementation pending")
    }

    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        unimplemented!("M1: implementation pending")
    }
}
