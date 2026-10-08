//! Ordered interior for `ordered_set`: a K-ary B+-tree.
//!
//! `ordered_set` is the one ETS kind whose observable behaviour depends on *term
//! order*: `lookup`/`member`/`delete` must find a key by `=:=` order and
//! `tab2list`/iteration must run in ascending term order. Both are exactly
//! `beam_term::compare::cmp`/`eq` (the single authority on Erlang term order —
//! never re-implemented here), so `1` and `1.0` are distinct keys.
//!
//! The index is a real B+-tree of order 32 rather than a sorted `Vec`, so exact
//! lookup, insertion and deletion are `O(log n)` instead of `O(n)`:
//!
//! * [`node`] — the nodes (a sorted entry run in a leaf, children plus their
//!   subtree maxima in a branch), the arena that owns them, and the two node-local
//!   search helpers.
//! * [`tree`] — the tree: insert with node splits, delete with borrow-or-merge
//!   rebalancing, and the ordered navigation (`first`/`last`/`next`/`prev` and the
//!   range bounds). Its module docs state the invariants the test suite asserts.
//! * [`iter`] — the left-to-right leaf-chain walk that makes ordered iteration a
//!   pointer chase.
//! * [`sorted`] — the alternative [`SortedOrderedInterior`]: the *same* observable
//!   semantics over a plain sorted `Vec` (`O(log n)` binary-search lookup, `O(n)`
//!   insert/delete). It is selectable with
//!   [`OrderedInterior::Sorted`](crate::OrderedInterior::Sorted) so the two ordered
//!   interiors can be cross-checked and measured; the B+-tree above is the default.
//!
//! [`OrderedSetInterior`] is the public face: it owns one [`Tree`] and adapts it
//! to the crate-private [`Interior`] seam. The inherent navigation methods
//! (`first`/`last`/`next`/`prev`/`next_from`/`prev_from`) are the range API a
//! later `ets:next/2`, `ets:prev/2` or `ets:select` range scan wires up; none of
//! them require the bound key to be present.
//!
//! Public for testing and reuse, but **not** the cross-crate seam: dependents go
//! through [`crate::Table`].

mod iter;
mod node;
mod sorted;
mod tree;

use beam_common::Result;
use beam_term::prelude::*;

use self::tree::Tree;
use crate::error::badarg;
use crate::key::key_of;
use crate::table::Interior;

pub use self::sorted::SortedOrderedInterior;

/// The B+-tree-backed interior used by `ordered_set`.
///
/// One object per key: an insert on an equal key replaces the object, `insert_new`
/// refuses an existing key, and every read/iteration is in `cmp` order.
pub struct OrderedSetInterior {
    /// The index itself; membership, ordering and iteration all live there.
    tree: Tree,
}

impl OrderedSetInterior {
    /// A new, empty ordered interior.
    pub fn new() -> OrderedSetInterior {
        OrderedSetInterior { tree: Tree::new() }
    }

    /// The smallest key, or `None` when the table is empty.
    ///
    /// `atoms` is accepted for call-site symmetry with the other navigation
    /// methods; a leftmost descent needs no term comparison.
    pub fn first(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.tree.first()
    }

    /// The largest key, or `None` when the table is empty (see [`Self::first`] for
    /// the unused atom table).
    pub fn last(&self, _atoms: &AtomTable) -> Option<Eterm> {
        self.tree.last()
    }

    /// [`Self::first`] under the name the range API uses.
    pub fn first_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.first(atoms)
    }

    /// [`Self::last`] under the name the range API uses.
    pub fn last_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        self.last(atoms)
    }

    /// The smallest key **strictly greater** than `key`, whether or not `key` is
    /// present in the table (a partial-bound search).
    pub fn next(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.tree.next(key, atoms)
    }

    /// The largest key **strictly less** than `key`, whether or not `key` is
    /// present in the table (a partial-bound search).
    pub fn prev(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.tree.prev(key, atoms)
    }

    /// The smallest stored key `>= bound`, or `> bound` when `inclusive` is false.
    /// `bound` need not be a stored key.
    pub fn next_from(&self, bound: Eterm, inclusive: bool, atoms: &AtomTable) -> Option<Eterm> {
        self.tree.next_from(bound, inclusive, atoms)
    }

    /// The largest stored key `<= bound`, or `< bound` when `inclusive` is false.
    /// `bound` need not be a stored key.
    pub fn prev_from(&self, bound: Eterm, inclusive: bool, atoms: &AtomTable) -> Option<Eterm> {
        self.tree.prev_from(bound, inclusive, atoms)
    }

    /// An honest, non-negative word count of the B+-tree node arena — the
    /// `ordered_set` interior contribution to `ets:info(Tab, memory)`.
    ///
    /// Measured at capacity (the node arena's honest allocation), so it is strictly
    /// positive even when empty (the root leaf exists), never decreases as keys are
    /// inserted, and is at least one word per stored object.
    pub fn memory(&self) -> usize {
        self.tree.memory()
    }
}

impl Interior for OrderedSetInterior {
    fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        self.tree.insert(key, object, atoms);
        // A `set`-family insert always retains the object (an equal key replaces).
        Ok(true)
    }

    fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<bool> {
        Ok(self.tree.insert_new(key, object, atoms))
    }

    fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> Result<()> {
        self.tree.delete(key, atoms);
        Ok(())
    }

    fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> Result<()> {
        self.tree.delete_object(key, object, atoms);
        Ok(())
    }

    fn lookup(&self, key: Eterm, atoms: &AtomTable) -> Vec<Eterm> {
        self.tree.find(key, atoms).into_iter().collect()
    }

    fn member(&self, key: Eterm, atoms: &AtomTable) -> bool {
        self.tree.find(key, atoms).is_some()
    }

    fn lookup_element(&self, key: Eterm, pos: usize, atoms: &AtomTable) -> Result<Eterm> {
        let object = self.tree.find(key, atoms).ok_or_else(badarg)?;
        // `pos` is a 0-based element index; `key_of` takes a 1-based position and
        // rejects a non-tuple object or an out-of-range position with `badarg`.
        let keypos = pos.checked_add(1).ok_or_else(badarg)?;
        key_of(object, keypos)
    }

    fn size(&self) -> usize {
        self.tree.len()
    }

    fn memory(&self) -> usize {
        // The honest node-arena count, not the `Interior` default of 0.
        OrderedSetInterior::memory(self)
    }

    fn iter_objects(&self) -> Box<dyn Iterator<Item = Eterm> + '_> {
        Box::new(self.tree.iter().map(|entry| entry.obj))
    }

    fn keys(&self) -> Vec<Eterm> {
        // Ascending `cmp` order: the leaf chain of the B+-tree *is* the sorted key
        // run, so this is the interior's own iteration order.
        self.tree.iter().map(|entry| entry.key).collect()
    }

    fn first_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        // O(log n) leftmost descent rather than the default's `keys()` scan.
        OrderedSetInterior::first_key(self, atoms)
    }

    fn last_key(&self, atoms: &AtomTable) -> Option<Eterm> {
        // O(log n) rightmost descent rather than the default's `keys()` scan.
        OrderedSetInterior::last_key(self, atoms)
    }

    fn next_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        // The smallest stored key strictly greater than `key`, present or not — the
        // B+-tree's exact successor search, which is also `ets:next/2`'s
        // range-bound behaviour for `ordered_set`.
        OrderedSetInterior::next(self, key, atoms)
    }

    fn prev_key(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        // The largest stored key strictly less than `key` (the mirror of `next_key`).
        OrderedSetInterior::prev(self, key, atoms)
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

    /// A traversal order over `0..count`: multiplying by a constant coprime with
    /// `count` is a bijection, so every index is visited exactly once, scattered
    /// (a deterministic "shuffle" with no `rand` dependency).
    fn shuffled(count: i64, step: i64) -> impl Iterator<Item = i64> {
        (0..count).map(move |i| (i * step) % count)
    }

    /// Every stored key, asserting on the way that the iteration order really is
    /// ascending `cmp` order.
    fn ordered_keys(t: &OrderedSetInterior, atoms: &AtomTable) -> Vec<Eterm> {
        let keys: Vec<Eterm> = t.tree.iter().map(|entry| entry.key).collect();
        for pair in keys.windows(2) {
            assert_eq!(
                cmp(pair[0], pair[1], atoms),
                Ordering::Less,
                "iteration ascends in cmp order"
            );
        }
        keys
    }

    #[test]
    fn an_empty_table_has_no_extremes_or_neighbours() {
        let atoms = AtomTable::new();
        let mut t = OrderedSetInterior::new();

        assert_eq!(t.size(), 0);
        assert!(t.iter_objects().next().is_none());
        assert_eq!(t.first(&atoms), None);
        assert_eq!(t.last(&atoms), None);
        assert_eq!(t.next(make_small(1), &atoms), None);
        assert_eq!(t.prev(make_small(1), &atoms), None);
        assert_eq!(t.next_from(make_small(1), true, &atoms), None);
        assert_eq!(t.prev_from(make_small(1), true, &atoms), None);
        assert!(t.lookup(make_small(1), &atoms).is_empty());
        assert!(!t.member(make_small(1), &atoms));

        // Deleting from an empty table is a no-op, not an error.
        t.delete(make_small(1), &atoms).unwrap();
        t.delete_object(make_small(1), make_small(1), &atoms)
            .unwrap();
        assert_eq!(t.size(), 0);
        t.tree.check_invariants(&atoms);
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
        t.tree.check_invariants(&atoms);
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

        // Ascending term order: small int < float < atom < tuple < nil < list.
        let ascending = [small, float, atom, tuple, NIL, list];

        let mut t = OrderedSetInterior::new();
        // Insert in reverse term order; the tree sorts them.
        for k in ascending.iter().rev() {
            t.insert(*k, *k, &atoms).unwrap();
        }
        assert_eq!(t.size(), 6);

        let iterated: Vec<Eterm> = t.iter_objects().collect();
        assert_eq!(iterated, ascending.to_vec());
        for w in iterated.windows(2) {
            assert_eq!(cmp(w[0], w[1], &atoms), Ordering::Less);
        }

        // The navigation agrees with the order.
        assert_eq!(t.first(&atoms), Some(small));
        assert_eq!(t.last(&atoms), Some(list));
        assert_eq!(t.next(small, &atoms), Some(float));
        assert_eq!(t.prev(list, &atoms), Some(NIL));
        t.tree.check_invariants(&atoms);
    }

    /// Drive one interior through a **mixed-class** key set and assert that both
    /// its ordered iteration and its navigation (`first`/`last`/`next`/`prev`) run
    /// in exactly [`cmp`] order. The key classes here are numbers (integers,
    /// including a negative and zero, plus a float), an atom, a tuple, the empty
    /// list, a non-empty list and a binary — every class whose relative rank is
    /// pinned by Erlang term order. This does **not** consult the live oracle
    /// (the differential harness owns that); it pins the interior against
    /// `beam_term::compare`, the single authority on term order.
    fn check_mixed_keys(interior: &mut dyn Interior, keys: &[Eterm], atoms: &AtomTable) {
        // Insert in reverse, so a correct interior must restore `cmp` order itself.
        for key in keys.iter().rev() {
            interior.insert(*key, *key, atoms).unwrap();
        }
        assert_eq!(interior.size(), keys.len());

        // The expected order is *defined* as the `cmp` sort of the key set.
        let mut expected = keys.to_vec();
        expected.sort_by(|a, b| cmp(*a, *b, atoms));
        assert_eq!(expected.len(), keys.len(), "the mixed keys are distinct");

        // Ordered iteration (both the object walk and the key run) is ascending.
        let iterated: Vec<Eterm> = interior.iter_objects().collect();
        assert_eq!(iterated, expected, "iter_objects is in cmp order");
        assert_eq!(interior.keys(), expected, "keys is in cmp order");
        for pair in expected.windows(2) {
            assert_eq!(
                cmp(pair[0], pair[1], atoms),
                Ordering::Less,
                "consecutive keys are strictly ascending"
            );
        }

        // `first`/`last` are the extremes; each present key's neighbours are the
        // adjacent keys of the expected run.
        assert_eq!(interior.first_key(atoms), Some(expected[0]));
        assert_eq!(interior.last_key(atoms), expected.last().copied());
        for (i, key) in expected.iter().enumerate() {
            assert_eq!(
                interior.next_key(*key, atoms),
                expected.get(i + 1).copied(),
                "next_key at index {i}"
            );
            assert_eq!(
                interior.prev_key(*key, atoms),
                i.checked_sub(1).map(|before| expected[before]),
                "prev_key at index {i}"
            );
        }

        // Walking forward from `first` with `next_key` visits every key in order …
        let mut forward = Vec::new();
        let mut cursor = interior.first_key(atoms);
        while let Some(key) = cursor {
            forward.push(key);
            cursor = interior.next_key(key, atoms);
        }
        assert_eq!(forward, expected, "the next-chain is the ascending run");

        // … and walking backward from `last` with `prev_key` is its mirror.
        let mut backward = Vec::new();
        let mut cursor = interior.last_key(atoms);
        while let Some(key) = cursor {
            backward.push(key);
            cursor = interior.prev_key(key, atoms);
        }
        backward.reverse();
        assert_eq!(backward, expected, "the prev-chain is the ascending run");
    }

    #[test]
    fn mixed_key_classes_iterate_and_navigate_in_cmp_order() {
        use beam_term::bitstring::{heap_bits_size_words, write_binary};

        let mut atoms = AtomTable::new();
        let atom = make_atom(atoms.intern("alpha"));
        let mut heap = Heap::new(1 << 12);

        // A 24-bit binary `<<0,1,2>>` in the heap arena.
        let binary = {
            let per = heap_bits_size_words(24);
            // SAFETY: `per` words are reserved for a 24-bit (3-byte) binary and the
            // payload is exactly 3 bytes.
            unsafe {
                let hp = heap.alloc_words(per).expect("the arena is sized");
                write_binary(hp, &[0u8, 1, 2]).expect("3 bytes is a 24-bit binary")
            }
        };

        let tuple = heap.tuple(&[make_small(1)]).unwrap();
        let list = heap.cons(make_small(1), NIL).unwrap();

        // Every class whose relative rank Erlang term order pins: numbers (a
        // negative int, zero, a positive int, and a float), an atom, a tuple, the
        // empty list, a non-empty list and a binary.
        let keys = vec![
            make_small(-5),
            make_small(0),
            make_small(7),
            float_term(1.0),
            atom,
            tuple,
            NIL,
            list,
            binary,
        ];

        // The B+-tree interior, plus its structural invariants after the inserts.
        let mut tree = OrderedSetInterior::new();
        check_mixed_keys(&mut tree, &keys, &atoms);
        tree.tree.check_invariants(&atoms);

        // The sorted-`Vec` interior must agree on order and navigation exactly.
        let mut sorted = SortedOrderedInterior::new();
        check_mixed_keys(&mut sorted, &keys, &atoms);
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

        // The integer sorts before the float (exact mode orders by class), and the
        // float is the integer's successor.
        assert_eq!(cmp(one, one_float, &atoms), Ordering::Less);
        assert_eq!(t.next(one, &atoms), Some(one_float));
        assert_eq!(t.prev(one_float, &atoms), Some(one));
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

    #[test]
    fn delete_object_only_removes_an_equal_object() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = OrderedSetInterior::new();

        let stored = object(&mut heap, 3, 30);
        t.insert(make_small(3), stored, &atoms).unwrap();

        // A structurally equal object (a *different* term) still matches …
        let equal = object(&mut heap, 3, 30);
        assert_ne!(stored.raw(), equal.raw(), "the two objects are distinct");
        t.delete_object(make_small(3), equal, &atoms).unwrap();
        assert!(!t.member(make_small(3), &atoms));
        assert_eq!(t.size(), 0);

        // … while a different object under the same key does not.
        t.insert(make_small(3), stored, &atoms).unwrap();
        let other = object(&mut heap, 3, 31);
        t.delete_object(make_small(3), other, &atoms).unwrap();
        assert!(t.member(make_small(3), &atoms));
        assert_eq!(t.size(), 1);

        // An absent key is a no-op whatever the object is.
        t.delete_object(make_small(9), other, &atoms).unwrap();
        assert_eq!(t.size(), 1);
    }

    #[test]
    fn navigation_reports_neighbours_of_present_and_absent_keys() {
        let atoms = AtomTable::new();
        let mut t = OrderedSetInterior::new();
        for k in [10, 20, 30] {
            t.insert(make_small(k), make_small(k), &atoms).unwrap();
        }

        assert_eq!(t.first(&atoms), Some(make_small(10)));
        assert_eq!(t.last(&atoms), Some(make_small(30)));
        assert_eq!(t.first_key(&atoms), Some(make_small(10)));
        assert_eq!(t.last_key(&atoms), Some(make_small(30)));

        // Present keys: strictly-greater / strictly-less neighbours.
        assert_eq!(t.next(make_small(10), &atoms), Some(make_small(20)));
        assert_eq!(t.next(make_small(20), &atoms), Some(make_small(30)));
        assert_eq!(t.next(make_small(30), &atoms), None);
        assert_eq!(t.prev(make_small(30), &atoms), Some(make_small(20)));
        assert_eq!(t.prev(make_small(20), &atoms), Some(make_small(10)));
        assert_eq!(t.prev(make_small(10), &atoms), None);

        // Keys that are *not* in the table (a partial-bound search).
        assert_eq!(t.next(make_small(15), &atoms), Some(make_small(20)));
        assert_eq!(t.prev(make_small(15), &atoms), Some(make_small(10)));
        assert_eq!(t.next(make_small(5), &atoms), Some(make_small(10)));
        assert_eq!(t.prev(make_small(5), &atoms), None);
        assert_eq!(t.next(make_small(35), &atoms), None);
        assert_eq!(t.prev(make_small(35), &atoms), Some(make_small(30)));

        // Inclusive/exclusive range bounds.
        assert_eq!(
            t.next_from(make_small(20), true, &atoms),
            Some(make_small(20))
        );
        assert_eq!(
            t.next_from(make_small(20), false, &atoms),
            Some(make_small(30))
        );
        assert_eq!(
            t.next_from(make_small(21), true, &atoms),
            Some(make_small(30))
        );
        assert_eq!(
            t.prev_from(make_small(20), true, &atoms),
            Some(make_small(20))
        );
        assert_eq!(
            t.prev_from(make_small(20), false, &atoms),
            Some(make_small(10))
        );
        assert_eq!(
            t.prev_from(make_small(19), true, &atoms),
            Some(make_small(10))
        );
        assert_eq!(
            t.next_from(make_small(30), true, &atoms),
            Some(make_small(30))
        );
        assert_eq!(t.next_from(make_small(30), false, &atoms), None);
        assert_eq!(
            t.prev_from(make_small(10), true, &atoms),
            Some(make_small(10))
        );
        assert_eq!(t.prev_from(make_small(10), false, &atoms), None);
    }

    #[test]
    fn navigation_crosses_leaf_and_branch_boundaries() {
        const N: i64 = 1_000;
        let atoms = AtomTable::new();
        let mut t = OrderedSetInterior::new();
        for k in shuffled(N, 7919) {
            t.insert(make_small(k), make_small(k), &atoms).unwrap();
        }
        t.tree.check_invariants(&atoms);
        assert!(t.tree.height() >= 2, "a 1k-key tree is deeper than a leaf");

        assert_eq!(t.first(&atoms), Some(make_small(0)));
        assert_eq!(t.last(&atoms), Some(make_small(N - 1)));

        // Walk the -3..N+3 neighbourhood of *every* key (present and absent) and
        // compare the neighbours against the known dense key set.
        for k in -3..N + 3 {
            let next = if k + 1 < 0 {
                Some(make_small(0))
            } else if k + 1 < N {
                Some(make_small(k + 1))
            } else {
                None
            };
            let prev = if k - 1 > N - 1 {
                Some(make_small(N - 1))
            } else if k - 1 >= 0 {
                Some(make_small(k - 1))
            } else {
                None
            };
            assert_eq!(t.next(make_small(k), &atoms), next, "next({k})");
            assert_eq!(t.prev(make_small(k), &atoms), prev, "prev({k})");

            let next_from_inclusive = if k >= 0 && k < N {
                Some(make_small(k))
            } else {
                next
            };
            assert_eq!(
                t.next_from(make_small(k), true, &atoms),
                next_from_inclusive,
                "next_from({k}, inclusive)"
            );
            let prev_from_inclusive = if k >= 0 && k < N {
                Some(make_small(k))
            } else {
                prev
            };
            assert_eq!(
                t.prev_from(make_small(k), true, &atoms),
                prev_from_inclusive,
                "prev_from({k}, inclusive)"
            );
        }
    }

    #[test]
    fn borrowing_from_the_left_sibling_keeps_the_separator_exact() {
        let atoms = AtomTable::new();
        let mut t = OrderedSetInterior::new();
        // 65 ascending keys split into four leaves of 16/16/16/17 entries.
        for k in 0..=64 {
            t.insert(make_small(k), make_small(k), &atoms).unwrap();
        }
        assert_eq!(t.tree.height(), 2, "the 65 keys need two levels");
        t.tree.check_invariants(&atoms);

        // Emptying the first leaf down to its minimum makes the *next* delete merge
        // it with its right sibling: three leaves are left, the first holding 30
        // entries (a spare) — so the leaf after it can borrow from the left.
        t.delete(make_small(15), &atoms).unwrap();
        t.tree.check_invariants(&atoms);

        // Deleting that leaf's maximum under-fills it and forces a borrow from the
        // left: it inherits the key 31 while its own maximum moves *down* (47 →
        // 46). Its separator must therefore be re-derived from the surviving leaf —
        // the borrow itself cannot know the maximum shrank.
        t.delete(make_small(47), &atoms).unwrap();
        t.tree.check_invariants(&atoms);

        assert_eq!(t.size(), 63);
        assert!(!t.member(make_small(15), &atoms));
        assert!(!t.member(make_small(47), &atoms));
        // The borrowed key survived its move and still orders correctly.
        assert!(t.member(make_small(31), &atoms));
        assert_eq!(t.next(make_small(30), &atoms), Some(make_small(31)));
        assert_eq!(t.next(make_small(31), &atoms), Some(make_small(32)));
        assert_eq!(t.prev(make_small(32), &atoms), Some(make_small(31)));
        assert_eq!(
            ordered_keys(&t, &atoms),
            (0..=64)
                .filter(|k| *k != 15 && *k != 47)
                .map(make_small)
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn invariants_hold_after_every_operation() {
        const N: i64 = 200;
        let atoms = AtomTable::new();
        let mut heap = Heap::new(2_048);
        let objects: Vec<Eterm> = (0..N).map(|k| object(&mut heap, k, k)).collect();
        let mut t = OrderedSetInterior::new();

        // Insert in a scattered order, checking the whole tree (structure *and*
        // the full ascending key run) against a sorted oracle after every step.
        let mut oracle: Vec<i64> = Vec::new();
        for k in shuffled(N, 7919) {
            assert!(t
                .insert_new(make_small(k), objects[k as usize], &atoms)
                .unwrap());
            oracle.push(k);
            oracle.sort_unstable();
            t.tree.check_invariants(&atoms);
            assert_eq!(t.size(), oracle.len());
            assert_eq!(
                ordered_keys(&t, &atoms),
                oracle.iter().copied().map(make_small).collect::<Vec<_>>()
            );
        }

        // …and delete 200 keys in another scattered order, again after every step:
        // this is what exercises borrowing, merging and the root collapse.
        for k in shuffled(N, 1237) {
            t.delete(make_small(k), &atoms).unwrap();
            oracle.retain(|&v| v != k);
            t.tree.check_invariants(&atoms);
            assert_eq!(t.size(), oracle.len());
            assert_eq!(
                ordered_keys(&t, &atoms),
                oracle.iter().copied().map(make_small).collect::<Vec<_>>()
            );
            assert_eq!(
                t.first(&atoms),
                oracle.first().copied().map(make_small),
                "first after deleting {k}"
            );
            assert_eq!(
                t.last(&atoms),
                oracle.last().copied().map(make_small),
                "last after deleting {k}"
            );
        }
        assert_eq!(t.size(), 0);
    }

    #[test]
    fn ten_thousand_keys_survive_shuffled_insert_delete_and_churn() {
        const N: i64 = 10_000;
        let atoms = AtomTable::new();
        // One `{Key, Key*3}` tuple per key, built once and re-inserted as needed
        // (the interior stores `Eterm` words; nothing is copied at this layer).
        let mut heap = Heap::new(1 << 18);
        let objects: Vec<Eterm> = (0..N).map(|k| object(&mut heap, k, k * 3)).collect();
        let mut t = OrderedSetInterior::new();

        // Scattered insertion: the tree splits its way down.
        for (step, k) in shuffled(N, 7919).enumerate() {
            assert!(t
                .insert_new(make_small(k), objects[k as usize], &atoms)
                .unwrap());
            if step % 500 == 0 {
                t.tree.check_invariants(&atoms);
            }
        }
        assert_eq!(t.size(), N as usize);
        t.tree.check_invariants(&atoms);
        assert!(
            t.tree.height() >= 3,
            "10k keys need more than two levels (height {})",
            t.tree.height()
        );

        // Every key reads back its own object, and iteration is the full run.
        for k in 0..N {
            assert!(t.member(make_small(k), &atoms), "key {k} is a member");
            assert_eq!(t.lookup(make_small(k), &atoms), vec![objects[k as usize]]);
        }
        assert_eq!(
            ordered_keys(&t, &atoms),
            (0..N).map(make_small).collect::<Vec<_>>()
        );

        // Remove every third key, leaving holes that force merges …
        let removed: Vec<i64> = (0..N).step_by(3).collect();
        let mut present = vec![true; N as usize];
        for &k in &removed {
            present[k as usize] = false;
            t.delete(make_small(k), &atoms).unwrap();
        }
        assert_eq!(t.size(), N as usize - removed.len());
        t.tree.check_invariants(&atoms);
        for k in 0..N {
            assert_eq!(
                t.member(make_small(k), &atoms),
                present[k as usize],
                "membership of {k} after the deletes"
            );
        }
        assert_eq!(
            ordered_keys(&t, &atoms),
            (0..N)
                .filter(|k| present[*k as usize])
                .map(make_small)
                .collect::<Vec<_>>()
        );

        // … re-insert them (re-splitting around the holes) …
        for &k in &removed {
            assert!(t
                .insert_new(make_small(k), objects[k as usize], &atoms)
                .unwrap());
        }
        assert_eq!(t.size(), N as usize);
        t.tree.check_invariants(&atoms);

        // … churn the high half away and back …
        for k in (N / 2..N).rev() {
            t.delete(make_small(k), &atoms).unwrap();
        }
        assert_eq!(t.size(), (N / 2) as usize);
        t.tree.check_invariants(&atoms);
        for k in N / 2..N {
            assert!(t
                .insert_new(make_small(k), objects[k as usize], &atoms)
                .unwrap());
        }
        assert_eq!(t.size(), N as usize);

        // … then drain the whole tree; the last delete collapses the root back to
        // a single empty leaf.
        for (step, k) in shuffled(N, 7919).enumerate() {
            t.delete(make_small(k), &atoms).unwrap();
            if step % 500 == 0 {
                t.tree.check_invariants(&atoms);
            }
        }
        assert_eq!(t.size(), 0);
        assert!(t.iter_objects().next().is_none());
        assert_eq!(t.first(&atoms), None);
        assert_eq!(t.last(&atoms), None);
        t.tree.check_invariants(&atoms);
    }

    /// The `Interior` navigation seam: `keys` is the ascending key run and the four
    /// overrides answer straight from the B+-tree (including for an absent bound).
    #[test]
    fn the_navigation_seam_reports_keys_in_ascending_order() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64);
        let mut t = OrderedSetInterior::new();
        for v in [3i64, 1, 2] {
            t.insert(make_small(v), object(&mut heap, v, v * 10), &atoms)
                .unwrap();
        }

        assert_eq!(
            Interior::keys(&t),
            vec![make_small(1), make_small(2), make_small(3)],
            "`keys` is the ascending key run"
        );
        assert_eq!(Interior::first_key(&t, &atoms), Some(make_small(1)));
        assert_eq!(Interior::last_key(&t, &atoms), Some(make_small(3)));
        assert_eq!(
            Interior::next_key(&t, make_small(1), &atoms),
            Some(make_small(2))
        );
        assert_eq!(Interior::next_key(&t, make_small(3), &atoms), None);
        assert_eq!(
            Interior::prev_key(&t, make_small(3), &atoms),
            Some(make_small(2))
        );
        assert_eq!(Interior::prev_key(&t, make_small(1), &atoms), None);

        // An absent key is a bound, so its neighbours are still found.
        assert_eq!(
            Interior::next_key(&t, make_small(0), &atoms),
            Some(make_small(1))
        );
        assert_eq!(
            Interior::prev_key(&t, make_small(9), &atoms),
            Some(make_small(3))
        );

        // The B+-tree reports its node-arena storage through the seam.
        assert!(Interior::memory(&t) > 0);
        assert_eq!(Interior::memory(&t), OrderedSetInterior::memory(&t));

        // Draining the tree empties both `keys` and the extremes.
        for v in [1i64, 2, 3] {
            t.delete(make_small(v), &atoms).unwrap();
        }
        assert!(Interior::keys(&t).is_empty());
        assert_eq!(Interior::first_key(&t, &atoms), None);
        assert_eq!(Interior::last_key(&t, &atoms), None);
    }

    /// The `ordered_set` interior answers `ets:info(Tab, memory)` honestly: the
    /// node-arena word count is strictly positive even when empty (the root leaf
    /// exists), never decreases as keys are inserted, and is at least one word per
    /// stored object.
    #[test]
    fn memory_is_a_non_negative_honest_word_count() {
        let atoms = AtomTable::new();
        let mut heap = Heap::new(64 * 1024);
        let mut t = OrderedSetInterior::new();

        // Even an empty tree owns its root leaf and the slot table, so the count is
        // strictly positive.
        let empty = t.memory();
        assert!(empty > 0, "an empty B+-tree owns its node arena");

        // Insert well past `ORDER` (32) so the tree splits into branch + multiple
        // leaves, and assert the count never decreases at any step.
        let mut prev = empty;
        for i in 0..2000i64 {
            let o = object(&mut heap, i, i * 2);
            t.insert_new(make_small(i), o, &atoms).unwrap();
            let now = t.memory();
            assert!(now >= prev, "count shrank on insert {i} ({prev} -> {now})");
            prev = now;
        }
        let populated = t.memory();
        assert!(populated > empty, "inserting grows the node arena");

        // Never under-reports: at least one word per stored object (node vectors and
        // the slot table only add to it).
        assert!(populated >= t.size());
        assert_eq!(t.size(), 2000);

        // The seam and the inherent accessor agree.
        assert_eq!(Interior::memory(&t), populated);
    }
}
