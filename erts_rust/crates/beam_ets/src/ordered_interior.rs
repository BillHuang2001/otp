//! Ordered interior for `ordered_set`.
//!
//! A **key-sorted `Vec`** of entries, binary-searched with `beam_term::compare::cmp`
//! (the one authority on Erlang term order). Insert is `O(n)` (a `Vec` shift),
//! which is correct and sufficient for M1; a B-tree / CA-tree is a documented
//! future optimization and would slot in behind the same [`Interior`] trait.
//!
//! Public for testing and reuse, but **not** the cross-crate seam: dependents go
//! through [`crate::Table`].

use beam_common::Result;
use beam_term::prelude::*;

use crate::table::Interior;

/// One ordered entry: a key and its single object (`ordered_set` keeps one per
/// key).
struct Entry {
    /// The key (the sort key of the entry).
    key: Eterm,
    /// The object stored under `key`.
    obj: Eterm,
}

/// The key-sorted interior used by `ordered_set`.
pub struct OrderedSetInterior {
    /// Entries kept sorted by `beam_term::compare::cmp` over `key`.
    entries: Vec<Entry>,
}

impl OrderedSetInterior {
    /// A new empty ordered interior.
    pub fn new() -> OrderedSetInterior {
        unimplemented!("M1: implementation pending")
    }
}

impl Interior for OrderedSetInterior {
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
