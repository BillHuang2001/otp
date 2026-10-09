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

//! Ordered interior for `ordered_set`: a K-ary B+-tree.
//!
//! `ordered_set` is the one ETS kind whose observable behaviour depends on *term
//! order*: `lookup`/`member`/`delete` must find a key by **arithmetic** key order
//! and `tab2list`/iteration must run in ascending arithmetic key order. Both are
//! exactly `beam_term::compare::cmp_arith` (the single authority on the order ERTS
//! `ordered_set` uses — never re-implemented here), so `1` and `1.0` are the *same*
//! key. The one exception is `delete_object/2`, which compares the whole *object*
//! exactly with `beam_term::compare::eq`.
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
/// refuses an existing key, and every read/iteration is in arithmetic key order
/// (`cmp_arith`, so `1` and `1.0` are the same key).
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
        // Ascending arithmetic key order: the leaf chain of the B+-tree *is* the
        // sorted key run, so this is the interior's own iteration order.
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
mod tests;
