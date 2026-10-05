//! Ordered interior for `ordered_set`.
//!
//! A **key-sorted `Vec`** of entries, binary-searched with `beam_term::compare::cmp`
//! (the one authority on Erlang term order). Insert is `O(n)` (a `Vec` shift),
//! which is correct and sufficient for M1; a B-tree / CA-tree is a documented
//! future optimization and would slot in behind the same `Interior` trait.
//!
//! Public for testing and reuse, but **not** the cross-crate seam: dependents go
//! through [`crate::Table`].

use beam_common::Result;
use beam_term::prelude::*;

use crate::error::badarg;
use crate::key::key_of;
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
        OrderedSetInterior {
            entries: Vec::new(),
        }
    }

    /// The position of `key` in `entries`: `Ok(idx)` on an exact (`cmp`-equal)
    /// hit, `Err(idx)` with the ascending insertion point on a miss. Ordering is
    /// delegated to `beam_term::compare::cmp` (so `1` and `1.0` are distinct).
    fn search(&self, key: Eterm, atoms: &AtomTable) -> std::result::Result<usize, usize> {
        self.entries
            .binary_search_by(|entry| cmp(entry.key, key, atoms))
    }

    /// Insert a fresh entry at the ascending position `idx`.
    fn insert_at(&mut self, idx: usize, key: Eterm, object: Eterm) {
        self.entries.insert(idx, Entry { key, obj: object });
    }
}

impl Interior for OrderedSetInterior {
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        match self.search(key, atoms) {
            Ok(idx) => self.entries[idx].obj = object,
            Err(idx) => self.insert_at(idx, key, object),
        }
        Ok(())
    }

    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        match self.search(key, atoms) {
            Ok(_) => Ok(false),
            Err(idx) => {
                self.insert_at(idx, key, object);
                Ok(true)
            }
        }
    }

    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        if let Ok(idx) = self.search(key, atoms) {
            self.entries.remove(idx);
        }
        Ok(())
    }

    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        if let Ok(idx) = self.search(key, atoms) {
            if eq(self.entries[idx].obj, object, atoms) {
                self.entries.remove(idx);
            }
        }
        Ok(())
    }

    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        match self.search(key, atoms) {
            Ok(idx) => vec![self.entries[idx].obj],
            Err(_) => Vec::new(),
        }
    }

    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        self.search(key, atoms).is_ok()
    }

    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        let idx = self.search(key, atoms).map_err(|_| badarg())?;
        let object = self.entries[idx].obj;
        // `pos` is a 0-based element index; `key_of` takes a 1-based position and
        // rejects a non-tuple object or an out-of-range position with `badarg`.
        let keypos = pos.checked_add(1).ok_or_else(badarg)?;
        key_of(object, keypos)
    }

    fn size(&self) -> usize {
        self.entries.len()
    }

    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        Box::new(self.entries.iter().map(|entry| entry.obj))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;
    use beam_term::heap::Heap;
    use core::cmp::Ordering;

    /// A heap-allocated flonum term (`[HEADER_FLONUM, bits]`); `beam_term` has no
    /// `Heap::float` constructor, so the two words are leaked for the test's life.
    fn float_term(value: f64) -> Eterm {
        let words: Box<[Eterm; 2]> =
            Box::new([HEADER_FLONUM, Eterm::from_raw(value.to_bits() as usize)]);
        let leaked: &'static [Eterm; 2] = Box::leak(words);
        make_boxed(leaked.as_ptr())
    }

    /// A `{Key, Value}` object tuple on `heap`.
    fn object(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)]).unwrap()
    }

    /// Structural (`beam_term::compare::eq`) equality of two object slices: two
    /// separately built but structurally equal tuples are equal terms even though
    /// their `Eterm` words (pointers) differ.
    fn same_objects(a: &[Eterm], b: &[Eterm], atoms: &AtomTable) -> bool {
        a.len() == b.len() && a.iter().zip(b).all(|(x, y)| eq(*x, *y, atoms))
    }

    #[test]
    fn insert_new_replace_and_delete() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(256);
        let mut t = OrderedSetInterior::new();

        // Insert 0..20 in reverse; the interior sorts them.
        for v in (0..20).rev() {
            let o = object(&mut heap, v, v * 10);
            assert!(t.insert_new(make_small(v), o, &atoms).unwrap());
        }
        assert_eq!(t.size(), 20);

        // `insert_new` on an existing key returns `false` and changes nothing.
        let o7 = object(&mut heap, 7, 70);
        assert!(!t.insert_new(make_small(7), make_small(-1), &atoms).unwrap());
        assert!(same_objects(
            &t.lookup(make_small(7), &atoms),
            &[o7],
            &atoms
        ));

        // `insert` replaces the object on an equal key.
        let o7b = object(&mut heap, 7, 77);
        t.insert(make_small(7), o7b, &atoms).unwrap();
        assert_eq!(t.lookup(make_small(7), &atoms), vec![o7b]);
        assert_eq!(t.size(), 20);

        // Misses.
        assert!(t.lookup(make_small(100), &atoms).is_empty());
        assert!(!t.member(make_small(100), &atoms));

        // `delete` removes the entry; a second delete is a no-op.
        t.delete(make_small(7), &atoms).unwrap();
        assert!(!t.member(make_small(7), &atoms));
        assert_eq!(t.size(), 19);
        t.delete(make_small(7), &atoms).unwrap();
        assert_eq!(t.size(), 19);

        // `delete_object` only removes when the stored object equals it.
        let o3 = object(&mut heap, 3, 30);
        t.delete_object(make_small(3), make_small(999), &atoms)
            .unwrap();
        assert!(t.member(make_small(3), &atoms));
        t.delete_object(make_small(3), o3, &atoms).unwrap();
        assert!(!t.member(make_small(3), &atoms));
        assert_eq!(t.size(), 18);

        // The remaining objects stay in ascending `cmp` order.
        let objs: Vec<Eterm> = t.iter_objects().collect();
        assert_eq!(objs.len(), 18);
        for w in objs.windows(2) {
            assert_eq!(cmp(w[0], w[1], &atoms), Ordering::Less);
        }
    }

    #[test]
    fn iterates_in_ascending_term_order_across_classes() {
        let mut atoms = AtomTable::new();
        let atom = make_atom(atoms.intern("a"));
        let mut heap = Heap::new(64);
        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        let list = heap.cons(make_small(1), NIL).unwrap();
        let float = float_term(1.0);
        let small = make_small(5);

        let mut t = OrderedSetInterior::new();
        // Insert out of term order: list > nil > tuple > atom > float > small.
        for k in [list, tuple, atom, float, small] {
            t.insert(k, k, &atoms).unwrap();
        }
        assert_eq!(t.size(), 5);

        let ascending: Vec<Eterm> = t.iter_objects().collect();
        assert_eq!(ascending, vec![small, float, atom, tuple, list]);
        for w in ascending.windows(2) {
            assert_eq!(cmp(w[0], w[1], &atoms), Ordering::Less);
        }
    }

    #[test]
    fn one_and_one_point_zero_are_distinct_keys() {
        let atoms = AtomTable::new();
        let mut t = OrderedSetInterior::new();
        let one = make_small(1);
        let one_float = float_term(1.0);

        t.insert(one, one, &atoms).unwrap();
        t.insert(one_float, one_float, &atoms).unwrap();
        assert_eq!(t.size(), 2);
        assert!(t.member(one, &atoms));
        assert!(t.member(one_float, &atoms));
        assert_eq!(t.lookup(one, &atoms), vec![one]);
        assert_eq!(t.lookup(one_float, &atoms), vec![one_float]);

        // The integer sorts before the float (exact mode orders by class).
        assert_eq!(cmp(one, one_float, &atoms), Ordering::Less);
    }

    #[test]
    fn lookup_element_reads_the_object_and_rejects_out_of_range() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = OrderedSetInterior::new();

        let k = make_small(1);
        let o = heap.tuple(&[k, make_small(10), make_small(20)]).unwrap();
        t.insert(k, o, &atoms).unwrap();

        assert_eq!(t.lookup_element(k, 0, &atoms).unwrap(), k);
        assert_eq!(t.lookup_element(k, 1, &atoms).unwrap(), make_small(10));
        assert_eq!(t.lookup_element(k, 2, &atoms).unwrap(), make_small(20));

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
    }
}
