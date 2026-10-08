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

//! Lazy, **view-borrowed** traversal over a [`TableRead`]: the key and object
//! iterators plus the `foldl`/`foldr` and copy-out list helpers.
//!
//! Everything here follows the lifetime discipline the [parent module](super)
//! describes: a traversal borrows the *view* (which owns the table's read guard),
//! and every key/object it yields is a [`TableTerm`] tagged to **that borrow** — so
//! neither the iterator nor a term it produced can outlive the guard. Each iterator
//! therefore stores the view by reference rather than a raw `Eterm`, which also
//! makes it `!Send` by construction (the view carries a scratch
//! [`Heap`](beam_alloc::Heap), which is never `Send`).
//!
//! # Which walk, and why it cannot fail
//!
//! A forward key walk is exactly `ets:first/1` followed by `ets:next/2`; the
//! reverse one is `ets:last/1` followed by `ets:prev/2`. The crate's public
//! [`TableRead::next`]/[`TableRead::prev`] add a guard on top: the hash kinds
//! (`set`/`bag`/`duplicate_bag`) require the anchor key to be *present* and raise
//! `badarg` otherwise. Every anchor these iterators use is a key the walk itself
//! just read, so it is present by construction; that guard would therefore never
//! fire. The iterators drive the interior's `next_key`/`prev_key` primitives
//! directly — the very calls that guard wraps — so there is no error path at all:
//! nothing is swallowed and nothing can be reported wrongly.
//!
//! # Objects, including the multi kinds
//!
//! [`TableRead::iter_objects`] yields every stored **object**, not every key: it
//! walks the keys and expands each key's object list, so a `bag`/`duplicate_bag`
//! key holding several objects yields each of them. [`TableRead::iter_keys`] yields
//! each key once. The reverse forms are the exact mirror of the forward ones.
//!
//! # Cost
//!
//! Each step of an `ordered_set` walk is the B+-tree's O(log n) successor/­
//! predecessor search; the hash kinds' successor search scans the interior's key
//! run, so a *full* walk of a hash kind is O(n²) — fine for the small/medium tables
//! a single walk touches, and the reason the crate's own `tab2list` (a single
//! interior scan) stays the bulk-enumeration path. [`TableRead::iter_objects`]
//! additionally allocates one `Vec` per key while expanding a multi-kind entry.

use beam_common::Result;
use beam_term::prelude::*;

use super::{TableRead, TableTerm};
use crate::store::TermStore;

/// The direction a key walk runs in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    /// `ets:first/1` then `ets:next/2`.
    Forward,
    /// `ets:last/1` then `ets:prev/2`.
    Reverse,
}

/// Where a key walk currently is.
#[derive(Clone, Copy)]
enum Cursor {
    /// Nothing yielded yet: the next step asks for the first (or last) key.
    Start,
    /// `anchor` was just yielded; the next step asks for its successor (or
    /// predecessor). Every anchor is a key the walk itself read, hence present.
    At(Eterm),
}

/// A raw key walk over a table, in one direction.
///
/// Yields bare `Eterm` keys (which alias table storage) for the two wrappers below
/// and for the object iterators to expand. It borrows the view, so its keys cannot
/// outlive the guard either.
struct KeyWalk<'a> {
    /// The borrowing view: it owns the guard that keeps these keys valid.
    read: &'a TableRead<'a>,
    /// The atom table the interior's key comparisons use.
    atoms: &'a AtomTable,
    /// Which way the walk runs.
    direction: Direction,
    /// Where the walk currently is.
    cursor: Cursor,
}

impl<'a> KeyWalk<'a> {
    /// Start a walk over `read` in `direction`.
    fn new(read: &'a TableRead<'a>, atoms: &'a AtomTable, direction: Direction) -> KeyWalk<'a> {
        KeyWalk {
            read,
            atoms,
            direction,
            cursor: Cursor::Start,
        }
    }

    /// The next key, or `None` at the end of the walk.
    fn next_raw(&mut self) -> Option<Eterm> {
        let step = match self.cursor {
            Cursor::Start => match self.direction {
                Direction::Forward => self.read.table().first(self.atoms),
                Direction::Reverse => self.read.table().last(self.atoms),
            },
            Cursor::At(anchor) => {
                let interior = &self.read.table().interior;
                match self.direction {
                    Direction::Forward => interior.next_key(anchor, self.atoms),
                    Direction::Reverse => interior.prev_key(anchor, self.atoms),
                }
            }
        };
        if let Some(key) = step {
            self.cursor = Cursor::At(key);
        }
        step
    }

    /// The objects stored under `key`, in the interior's stored order.
    fn objects(&self, key: Eterm) -> Vec<Eterm> {
        self.read.table().lookup(key, self.atoms)
    }
}

/// A forward key iterator over a read view: `ets:first/1` + `ets:next/2`, yielding
/// [`TableTerm`]s tagged to the view's borrow.
///
/// Obtained from [`TableRead::iter_keys`]. It **must not** outlive the guard the
/// view holds, and it cannot: the returned value borrows the view.
pub struct KeyIter<'a> {
    walk: KeyWalk<'a>,
}

impl<'a> Iterator for KeyIter<'a> {
    type Item = TableTerm<'a>;

    fn next(&mut self) -> Option<TableTerm<'a>> {
        self.walk.next_raw().map(TableTerm::new)
    }
}

/// A reverse key iterator over a read view: `ets:last/1` + `ets:prev/2`.
///
/// The mirror of [`KeyIter`]; obtained from [`TableRead::iter_keys_rev`].
pub struct KeyIterRev<'a> {
    walk: KeyWalk<'a>,
}

impl<'a> Iterator for KeyIterRev<'a> {
    type Item = TableTerm<'a>;

    fn next(&mut self) -> Option<TableTerm<'a>> {
        self.walk.next_raw().map(TableTerm::new)
    }
}

/// A forward **object** iterator over a read view.
///
/// Yields every stored object — each key's whole object list is expanded, so a
/// `bag`/`duplicate_bag` key holding several objects yields each of them. Obtained
/// from [`TableRead::iter_objects`].
pub struct ObjectIter<'a> {
    walk: KeyWalk<'a>,
    /// The current key's remaining objects; refilled from the next key when empty.
    pending: std::vec::IntoIter<Eterm>,
}

impl<'a> Iterator for ObjectIter<'a> {
    type Item = TableTerm<'a>;

    fn next(&mut self) -> Option<TableTerm<'a>> {
        loop {
            if let Some(object) = self.pending.next() {
                return Some(TableTerm::new(object));
            }
            let key = self.walk.next_raw()?;
            self.pending = self.walk.objects(key).into_iter();
        }
    }
}

/// A reverse **object** iterator over a read view: the exact mirror of
/// [`ObjectIter`] (keys last-to-first, each key's objects last-to-first).
///
/// Obtained from [`TableRead::iter_objects_rev`].
pub struct ObjectIterRev<'a> {
    walk: KeyWalk<'a>,
    /// The current key's remaining objects, in reverse stored order.
    pending: std::vec::IntoIter<Eterm>,
}

impl<'a> Iterator for ObjectIterRev<'a> {
    type Item = TableTerm<'a>;

    fn next(&mut self) -> Option<TableTerm<'a>> {
        loop {
            if let Some(object) = self.pending.next() {
                return Some(TableTerm::new(object));
            }
            let key = self.walk.next_raw()?;
            let mut objects = self.walk.objects(key);
            objects.reverse();
            self.pending = objects.into_iter();
        }
    }
}

impl<'g> TableRead<'g> {
    /// The keys of the table, in the table's own iteration order (`ets:first/1` +
    /// `ets:next/2`), each tagged to this view's borrow.
    ///
    /// ```
    /// use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions};
    /// use beam_term::prelude::*;
    ///
    /// let mut opts = TableOptions::new();
    /// opts.set_kind(TableKind::OrderedSet).set_access(Access::Public);
    /// let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
    /// let atoms = AtomTable::new();
    /// let mut source = Heap::new(64);
    /// for key in 1..=3 {
    ///     let object = source.tuple(&[make_small(key), make_small(key * 10)]).unwrap();
    ///     shared.with_write(|view| view.insert(object, &atoms)).unwrap().unwrap();
    /// }
    ///
    /// // An `ordered_set` walks ascending; each key is a `TableTerm` that may only
    /// // be used while the guard is held — extract the owned integer, not the term.
    /// let keys: Vec<i64> = shared
    ///     .with_read(|view| {
    ///         view.iter_keys(&atoms)
    ///             .filter_map(|key| key.as_small_int())
    ///             .collect()
    ///     })
    ///     .unwrap();
    /// assert_eq!(keys, vec![1, 2, 3]);
    /// ```
    ///
    /// The iterator borrows the closure-local view (which owns the read guard), so
    /// it cannot escape the closure:
    ///
    /// ```compile_fail
    /// use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions};
    /// use beam_term::prelude::*;
    ///
    /// let mut opts = TableOptions::new();
    /// opts.set_kind(TableKind::Set).set_access(Access::Public);
    /// let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
    /// let atoms = AtomTable::new();
    ///
    /// // Does not compile: the iterator borrows the closure-local view.
    /// let leaked = shared.with_read(|view| view.iter_keys(&atoms)).unwrap();
    /// let _ = leaked.count();
    /// ```
    pub fn iter_keys<'a>(&'a self, atoms: &'a AtomTable) -> KeyIter<'a> {
        KeyIter {
            walk: KeyWalk::new(self, atoms, Direction::Forward),
        }
    }

    /// The keys of the table in reverse iteration order (`ets:last/1` +
    /// `ets:prev/2`), each tagged to this view's borrow.
    ///
    /// For an `ordered_set` this is exactly [`iter_keys`](Self::iter_keys) reversed;
    /// for the hash kinds it is the reverse of the interior's own (unspecified)
    /// order.
    ///
    /// A term the iterator yields may not outlive the view:
    ///
    /// ```compile_fail
    /// use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions};
    /// use beam_term::prelude::*;
    ///
    /// let mut opts = TableOptions::new();
    /// opts.set_kind(TableKind::Set).set_access(Access::Public);
    /// let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
    /// let atoms = AtomTable::new();
    ///
    /// // Does not compile: the yielded term is tagged to the closure-local view.
    /// let leaked = shared
    ///     .with_read(|view| view.iter_keys_rev(&atoms).next())
    ///     .unwrap();
    /// let _ = leaked.is_some();
    /// ```
    pub fn iter_keys_rev<'a>(&'a self, atoms: &'a AtomTable) -> KeyIterRev<'a> {
        KeyIterRev {
            walk: KeyWalk::new(self, atoms, Direction::Reverse),
        }
    }

    /// Every stored **object** of the table, in the table's own iteration order —
    /// each key's whole object list is expanded, so the multi kinds
    /// (`bag`/`duplicate_bag`) yield one term per stored object.
    ///
    /// Each yielded object is tagged to this view's borrow; use
    /// [`TableTerm::copy_into`] to own one beyond the guard, or
    /// [`tab2list_into`](Self::tab2list_into) for the whole list at once.
    ///
    /// ```
    /// use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions};
    /// use beam_term::prelude::*;
    ///
    /// let mut opts = TableOptions::new();
    /// opts.set_kind(TableKind::OrderedSet).set_access(Access::Public);
    /// let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
    /// let atoms = AtomTable::new();
    /// let mut source = Heap::new(64);
    /// for key in 1..=3 {
    ///     let object = source.tuple(&[make_small(key), make_small(key * 10)]).unwrap();
    ///     shared.with_write(|view| view.insert(object, &atoms)).unwrap().unwrap();
    /// }
    ///
    /// // The value element of every object, summed while the guard is held.
    /// let total = shared
    ///     .with_read(|view| {
    ///         view.iter_objects(&atoms).fold(0i64, |acc, object| {
    ///             acc + object.tuple_element(1).and_then(|v| v.as_small_int()).unwrap()
    ///         })
    ///     })
    ///     .unwrap();
    /// assert_eq!(total, 60);
    /// ```
    pub fn iter_objects<'a>(&'a self, atoms: &'a AtomTable) -> ObjectIter<'a> {
        ObjectIter {
            walk: KeyWalk::new(self, atoms, Direction::Forward),
            pending: Vec::new().into_iter(),
        }
    }

    /// Every stored **object** in reverse iteration order — the exact mirror of
    /// [`iter_objects`](Self::iter_objects): keys last-to-first, and each key's
    /// objects last-to-first.
    pub fn iter_objects_rev<'a>(&'a self, atoms: &'a AtomTable) -> ObjectIterRev<'a> {
        ObjectIterRev {
            walk: KeyWalk::new(self, atoms, Direction::Reverse),
            pending: Vec::new().into_iter(),
        }
    }

    /// `lists:foldl/3` over the table's **objects**, in the table's iteration
    /// order: `f(f(f(init, object1), object2), …)`.
    ///
    /// The accumulator is caller-owned, so it escapes the guard freely; the objects
    /// handed to `f` are [`TableTerm`]s that may only be used inside the call.
    ///
    /// Folding is *not* a way to smuggle one out: `f` must accept the object for
    /// **every** borrow of the view, so a term can never be stored in the
    /// accumulator.
    ///
    /// ```compile_fail
    /// use beam_ets::{Access, SharedTable, Table, TableId, TableKind, TableOptions, TableTerm};
    /// use beam_term::prelude::*;
    ///
    /// let mut opts = TableOptions::new();
    /// opts.set_kind(TableKind::Set).set_access(Access::Public);
    /// let shared = SharedTable::new(Table::new(TableId::next(), opts).unwrap());
    /// let atoms = AtomTable::new();
    ///
    /// // Does not compile: the object is tagged to the closure-local view, so the
    /// // accumulator cannot hold it and the fold result cannot escape the guard.
    /// let smuggled: Option<TableTerm<'_>> = shared
    ///     .with_read(|view| view.foldl(None, |_acc, object| Some(object), &atoms))
    ///     .unwrap();
    /// let _ = smuggled;
    /// ```
    pub fn foldl<T>(
        &self,
        init: T,
        mut f: impl FnMut(T, TableTerm<'_>) -> T,
        atoms: &AtomTable,
    ) -> T {
        let mut acc = init;
        for object in self.iter_objects(atoms) {
            acc = f(acc, object);
        }
        acc
    }

    /// `lists:foldr/3` over the table's **objects**, in reverse iteration order:
    /// `f(object1, f(object2, … f(objectN, init)))` — the mirror of
    /// [`foldl`](Self::foldl), so folding a list into itself reverses it.
    ///
    /// As with `foldl`, only the accumulator escapes the guard.
    pub fn foldr<T>(
        &self,
        init: T,
        mut f: impl FnMut(TableTerm<'_>, T) -> T,
        atoms: &AtomTable,
    ) -> T {
        let mut acc = init;
        for object in self.iter_objects_rev(atoms) {
            acc = f(object, acc);
        }
        acc
    }

    /// Every object of [`tab2list`](TableRead::tab2list), **deep-copied** into the
    /// caller-owned `dest` so the result may outlive the guard.
    ///
    /// This is the bulk counterpart of [`TableTerm::copy_into`]: it reuses the very
    /// same deep-copier an `insert` uses ([`TermStore::copy`]), so its supported
    /// subset is exactly what a table can store, and an unsupported object is the
    /// same typed `badarg` an insert would give. A failure leaves `dest` holding the
    /// copies taken so far (the caller owns `dest` and decides what to do with them).
    pub fn tab2list_into(&self, dest: &mut TermStore) -> Result<Vec<Eterm>> {
        let raw = self.table().tab2list();
        let mut owned = Vec::with_capacity(raw.len());
        for object in raw {
            owned.push(dest.copy(object)?);
        }
        Ok(owned)
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the view-borrowed traversal API ([`TableRead::iter_keys`],
    //! [`TableRead::iter_objects`], the folds and [`TableRead::tab2list_into`]).
    //!
    //! Every assertion is an **exact** sequence or multiset — never an
    //! ordinal/probabilistic claim:
    //!
    //! * an `ordered_set` walk (forward and reverse) has a *deterministic* order, so
    //!   it is asserted literally;
    //! * a hash kind's forward walk order is unspecified, so it is asserted as a
    //!   *permutation* of the stored keys, while the **reverse** walk is asserted to
    //!   be the exact reverse of the forward one (both are deterministic runs of the
    //!   same interior);
    //! * the object walks are asserted to yield every stored object exactly once
    //!   (a multiset), and the reverse object walk to be the exact mirror of the
    //!   forward one.

    use super::*;
    use crate::kind::{Access, TableKind, TableOptions};
    use crate::table::{SharedTable, Table, TableId};

    /// A fresh, publicly-accessible table of `kind`, wrapped for sharing.
    fn shared_table(kind: TableKind) -> SharedTable {
        let mut opts = TableOptions::new();
        opts.set_kind(kind).set_access(Access::Public);
        SharedTable::new(Table::new(TableId::next(), opts).expect("a fresh table is constructible"))
    }

    /// `{Key, Value}` on `heap`.
    fn pair(heap: &mut Heap, key: i64, value: i64) -> Eterm {
        heap.tuple(&[make_small(key), make_small(value)])
            .expect("the test arena has room")
    }

    /// Insert `object` into `shared` as the owner would.
    fn put(shared: &SharedTable, object: Eterm, atoms: &AtomTable) {
        shared
            .with_write(|write| write.insert(object, atoms))
            .expect("no poisoned lock")
            .expect("a supported object inserts");
    }

    /// The `(key, value)` of a yielded object, as owned integers (extracted under
    /// the guard: nothing tagged escapes).
    fn pair_of(object: &TableTerm<'_>) -> (i64, i64) {
        let key = object
            .tuple_element(0)
            .and_then(|element| element.as_small_int())
            .expect("every stored key element is a small integer");
        let value = object
            .tuple_element(1)
            .and_then(|element| element.as_small_int())
            .expect("every stored value element is a small integer");
        (key, value)
    }

    /// The keys [`TableRead::iter_keys`] yields, as owned integers.
    fn keys_forward(shared: &SharedTable, atoms: &AtomTable) -> Vec<i64> {
        shared
            .with_read(|view| {
                view.iter_keys(atoms)
                    .filter_map(|key| key.as_small_int())
                    .collect()
            })
            .expect("no poisoned lock")
    }

    /// The keys [`TableRead::iter_keys_rev`] yields, as owned integers.
    fn keys_reverse(shared: &SharedTable, atoms: &AtomTable) -> Vec<i64> {
        shared
            .with_read(|view| {
                view.iter_keys_rev(atoms)
                    .filter_map(|key| key.as_small_int())
                    .collect()
            })
            .expect("no poisoned lock")
    }

    /// `(key, value)` of every object [`TableRead::iter_objects`] yields.
    fn objects_forward(shared: &SharedTable, atoms: &AtomTable) -> Vec<(i64, i64)> {
        shared
            .with_read(|view| view.iter_objects(atoms).map(|object| pair_of(&object)).collect())
            .expect("no poisoned lock")
    }

    /// `(key, value)` of every object [`TableRead::iter_objects_rev`] yields.
    fn objects_reverse(shared: &SharedTable, atoms: &AtomTable) -> Vec<(i64, i64)> {
        shared
            .with_read(|view| {
                view.iter_objects_rev(atoms)
                    .map(|object| pair_of(&object))
                    .collect()
            })
            .expect("no poisoned lock")
    }

    /// `items`, sorted (the order-insensitive comparison for a hash kind).
    fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
        items.sort();
        items
    }

    /// An `ordered_set` walk is exact in both directions: ascending forward,
    /// descending in reverse, over objects keyed in the same order.
    #[test]
    fn ordered_set_walks_are_exact_and_mirrored() {
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::OrderedSet);
        let mut heap = Heap::new(64);
        // Inserted out of order: the tree's iteration order is *key* order.
        put(&shared, pair(&mut heap, 3, 30), &atoms);
        put(&shared, pair(&mut heap, 1, 10), &atoms);
        put(&shared, pair(&mut heap, 2, 20), &atoms);

        assert_eq!(keys_forward(&shared, &atoms), vec![1, 2, 3]);
        assert_eq!(keys_reverse(&shared, &atoms), vec![3, 2, 1]);
        assert_eq!(
            objects_forward(&shared, &atoms),
            vec![(1, 10), (2, 20), (3, 30)]
        );
        assert_eq!(
            objects_reverse(&shared, &atoms),
            vec![(3, 30), (2, 20), (1, 10)]
        );
    }

    /// A hash kind's forward walk is a permutation of the stored keys (each exactly
    /// once); its reverse walk is the exact mirror of the forward one, and the same
    /// holds for the object walks.
    #[test]
    fn hash_kind_walks_visit_every_key_once_and_mirror() {
        const N: i64 = 64;
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::Set);
        let mut heap = Heap::new(4 * (N as usize + 1));
        for key in 1..=N {
            put(&shared, pair(&mut heap, key, key * 7), &atoms);
        }

        let forward = keys_forward(&shared, &atoms);
        assert_eq!(
            sorted(forward.clone()),
            (1..=N).collect::<Vec<_>>(),
            "every key exactly once"
        );
        let reverse = keys_reverse(&shared, &atoms);
        assert_eq!(
            reverse,
            forward.iter().rev().copied().collect::<Vec<_>>(),
            "the reverse walk is the exact mirror of the forward one"
        );

        let forward_objects = objects_forward(&shared, &atoms);
        assert_eq!(
            sorted(forward_objects.clone()),
            (1..=N).map(|key| (key, key * 7)).collect::<Vec<_>>()
        );
        let reverse_objects = objects_reverse(&shared, &atoms);
        assert_eq!(
            reverse_objects,
            forward_objects.iter().rev().copied().collect::<Vec<_>>()
        );
    }

    /// A `bag` key holds several objects: the key walk yields the key once, the
    /// object walk expands it into one term per stored object.
    #[test]
    fn object_walk_expands_every_object_of_a_multi_kind() {
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::Bag);
        let mut heap = Heap::new(64);
        put(&shared, pair(&mut heap, 1, 10), &atoms);
        put(&shared, pair(&mut heap, 1, 20), &atoms);
        put(&shared, pair(&mut heap, 1, 30), &atoms);
        put(&shared, pair(&mut heap, 2, 200), &atoms);

        // The key walk yields each key once...
        assert_eq!(sorted(keys_forward(&shared, &atoms)), vec![1, 2]);
        // ...while the object walk expands the bag: all four objects, exactly once.
        let forward = objects_forward(&shared, &atoms);
        assert_eq!(forward.len(), 4, "one term per stored object");
        assert_eq!(
            sorted(forward.clone()),
            vec![(1, 10), (1, 20), (1, 30), (2, 200)]
        );
        // The reverse object walk is the exact mirror of the forward one.
        assert_eq!(
            objects_reverse(&shared, &atoms),
            forward.iter().rev().copied().collect::<Vec<_>>()
        );
    }

    /// An empty table walks to nothing, and a fold over nothing returns its seed.
    #[test]
    fn an_empty_table_walks_to_nothing() {
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::OrderedSet);
        assert!(keys_forward(&shared, &atoms).is_empty());
        assert!(keys_reverse(&shared, &atoms).is_empty());
        assert!(objects_forward(&shared, &atoms).is_empty());
        assert!(objects_reverse(&shared, &atoms).is_empty());
        assert_eq!(
            shared
                .with_read(|view| view.foldl(7i64, |acc, _object| acc + 1, &atoms))
                .expect("no poisoned lock"),
            7
        );
        assert_eq!(
            shared
                .with_read(|view| view.foldr(7i64, |_object, acc| acc + 1, &atoms))
                .expect("no poisoned lock"),
            7
        );
    }

    /// A one-key table walks to exactly that key, in both directions.
    #[test]
    fn a_single_key_table_walks_to_exactly_that_key() {
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::OrderedSet);
        let mut heap = Heap::new(8);
        put(&shared, pair(&mut heap, 42, 420), &atoms);
        assert_eq!(keys_forward(&shared, &atoms), vec![42]);
        assert_eq!(keys_reverse(&shared, &atoms), vec![42]);
        assert_eq!(objects_forward(&shared, &atoms), vec![(42, 420)]);
        assert_eq!(objects_reverse(&shared, &atoms), vec![(42, 420)]);
    }

    /// `foldl` and `foldr` traverse opposite orders over the same objects.
    #[test]
    fn foldl_and_foldr_traverse_opposite_orders() {
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::OrderedSet);
        let mut heap = Heap::new(64);
        for key in 1..=4 {
            put(&shared, pair(&mut heap, key, key * 10), &atoms);
        }

        let forward: Vec<i64> = shared
            .with_read(|view| {
                view.foldl(
                    Vec::new(),
                    |mut acc, object| {
                        acc.push(pair_of(&object).1);
                        acc
                    },
                    &atoms,
                )
            })
            .expect("no poisoned lock");
        assert_eq!(forward, vec![10, 20, 30, 40]);

        let reverse: Vec<i64> = shared
            .with_read(|view| {
                view.foldr(
                    Vec::new(),
                    |object, mut acc| {
                        acc.push(pair_of(&object).1);
                        acc
                    },
                    &atoms,
                )
            })
            .expect("no poisoned lock");
        assert_eq!(reverse, vec![40, 30, 20, 10]);
        assert_eq!(
            reverse,
            forward.iter().rev().copied().collect::<Vec<_>>(),
            "foldr is the mirror of foldl"
        );
    }

    /// `tab2list_into` copies the whole table out under one guard, and the copies
    /// survive the guard being dropped *and* churn that recycles the originals'
    /// arena words.
    #[test]
    fn tab2list_into_copies_every_object_out_and_survives_churn() {
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::OrderedSet);
        let mut heap = Heap::new(64);
        let originals: Vec<Eterm> = (1..=3).map(|key| pair(&mut heap, key, key * 10)).collect();
        for object in &originals {
            put(&shared, *object, &atoms);
        }

        // Copy the whole table out under ONE guard, into caller-owned storage.
        let mut dest = TermStore::with_default_allocator();
        let owned = shared
            .with_read(|view| view.tab2list_into(&mut dest))
            .expect("no poisoned lock")
            .expect("every stored object is copyable");
        assert_eq!(owned.len(), 3);
        // (The guard is released here.)

        // Churn: delete each key and re-insert a *same-word-size* object, which
        // recycles the freed arena words a raw read would now alias.
        for key in 1..=3 {
            shared
                .with_write(|view| view.delete(make_small(key), &atoms))
                .expect("no poisoned lock")
                .expect("deleting a present key succeeds");
            put(&shared, pair(&mut heap, key, key * 10 + 1), &atoms);
        }

        // The table really holds the replacements...
        assert_eq!(
            objects_forward(&shared, &atoms),
            vec![(1, 11), (2, 21), (3, 31)]
        );
        // ...while the copies still hold the originals (caller-owned storage).
        for (copy, original) in owned.iter().zip(originals.iter()) {
            assert!(
                eq(*copy, *original, &atoms),
                "an owned copy must survive the churn that recycled its original"
            );
        }
    }

    /// `TableTerm::tuple_elements` lists a whole row; a non-tuple has none.
    #[test]
    fn tuple_elements_lists_the_whole_row() {
        let atoms = AtomTable::new();
        let shared = shared_table(TableKind::OrderedSet);
        let mut heap = Heap::new(64);
        let row = heap
            .tuple(&[make_small(1), make_small(2), make_small(3)])
            .expect("the test arena has room");
        put(&shared, row, &atoms);

        let (elements, scalar_elements) = shared
            .with_read(|view| {
                let object = view.lookup(make_small(1), &atoms).remove(0);
                let elements: Vec<i64> = object
                    .tuple_elements()
                    .iter()
                    .map(|element| element.as_small_int().expect("a small integer element"))
                    .collect();
                // An element of a tuple is itself a scalar: no elements of its own.
                let scalar = object.tuple_element(0).expect("the row has a first element");
                (elements, scalar.tuple_elements().len())
            })
            .expect("no poisoned lock");
        assert_eq!(elements, vec![1, 2, 3]);
        assert_eq!(scalar_elements, 0, "a non-tuple has no elements");
    }
}