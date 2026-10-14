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

//! The B+-tree: mutation (insert with node splits, delete with borrow-or-merge
//! rebalancing) and ordered navigation (leftmost/rightmost descent,
//! successor/predecessor, range bounds).
//!
//! # Shape
//!
//! A K-ary B+-tree of order [`ORDER`]: every object lives in a **leaf** entry
//! `(key, obj)`; every leaf is linked to the next leaf in ascending key order, so
//! ordered iteration is a pointer walk. A **branch** holds `children[i]` followed
//! by `separator[i]`, where `separator[i]` is *exactly* the greatest key in
//! `children[i]`'s subtree (not a lower bound for the right child — the maximum is
//! itself a stored leaf key, which is what makes the maximum of any subtree
//! readable in `O(1)` as `keys.last()`).
//!
//! # Invariants (asserted by [`Tree::check_invariants`] in the test suite)
//!
//! 1. `keys.len() == children.len()` on a branch, and both arrays are strictly
//!    ascending in arithmetic key order (`beam_term::compare::cmp_arith`).
//! 2. `separator[i] == max(children[i])` for every child.
//! 3. Every node holds at most [`ORDER`] entries/children (the fan-out that
//!    bounds node size and makes a split amortized `O(1)`), and every node except
//!    the root holds at least [`MIN_OCCUPANCY`]; the root leaf may be empty (that
//!    is the empty tree) and a root branch has at least two children.
//! 4. All leaves are at the same depth.
//! 5. The leaf chain visits every leaf exactly once, left to right.
//!
//! # Complexity
//!
//! Every operation is `O(log n)` node visits with `O(K)` work per node
//! (`Vec::insert`/`remove` inside a `K`-sized node), and `O(log n)` term
//! comparisons overall (`O(log ORDER)` of them per node, from `child_index`'s and
//! `search_entries`' binary searches), except for two documented exceptions:
//!
//! * `find`/`first`/`last`/`next`/`prev` — `O(log n)` node visits: one descent,
//!   plus for `next`/`prev` at most one extra descent to the neighbouring subtree's
//!   leftmost/rightmost leaf (`next_subtree_key`).
//! * `insert` — `O(log n)` node visits **amortized**. The unwind carries the new
//!   subtree maximum up in `InsertStep`, so an insert that does not split a node
//!   never descends again. A split does re-derive one separator with a
//!   [`NodeStore::subtree_max_key`] descent (`O(log n)` node visits, but *no* term
//!   comparisons — it is a pointer chase to a rightmost leaf), and splits are
//!   amortized `O(1)` per insert (one per `ORDER / 2` inserts), so an insert is
//!   worst-case `O(log^2 n)` only when a split cascades up every level.
//! * `delete` — `O(log^2 n)` node visits (unconditional): the unwind re-derives the
//!   settled child's maximum with a [`NodeStore::subtree_max_key`] descent at *every*
//!   level rather than carrying it up, so the tree depth is paid once per level.
//!   Those re-derivations are pointer chases, so the *comparison* count is still
//!   `O(log n)`.
//!
//! Ordered iteration is `O(n)` overall (a walk of the leaf chain). Splits always
//! split a node in half *after* it exceeds [`ORDER`], and a delete that under-fills
//! a node borrows from a sibling with a spare entry or merges (the two occupancies
//! then sum to at most `ORDER`), so every node stays within
//! `[MIN_OCCUPANCY, ORDER]` and the height only changes at the root.
//!
//! Term order is never re-implemented: every **key** comparison goes through the
//! arithmetic key order `beam_term::compare::cmp_arith`, so `1` and `1.0` are the
//! *same* key; the whole-object identity check [`Tree::delete_object`] is the sole
//! exception and keeps the exact `beam_term::compare::eq`.

use beam_term::prelude::*;

use super::iter::LeafIter;
use super::node::{
    child_index, search_entries, Entry, Node, NodeId, NodeStore, MIN_OCCUPANCY, ORDER,
};

/// The ordered index over `beam_term` terms.
pub(crate) struct Tree {
    /// The root: a leaf while the tree is empty/shallow, a branch otherwise.
    root: NodeId,
    /// The number of stored keys (one object per key).
    len: usize,
    /// The node arena. Nodes never move, so a split/merge never invalidates an id.
    store: NodeStore,
}

/// What one recursive insert step did to its subtree — exactly the information the
/// caller (whose node is the parent of that subtree) needs to keep the separator
/// invariant (see the module docs) satisfied *without* descending back into it.
///
/// The separator a branch stores for a child is the child subtree's maximum, and it
/// has to alias a **live** table-owned term (see [`NodeStore::subtree_max_key`]). An
/// insert may therefore have to rewrite that separator — but only when the subtree's
/// maximum actually moved. This type carries the new maximum up when it did, so the
/// unwind no longer has to re-derive every separator from scratch.
enum InsertStep {
    /// The key was already present and the caller asked for `insert_new`: the
    /// subtree is exactly as it was, so nothing above it changed either.
    Refused,
    /// The subtree was mutated but did not split. `added` reports whether a new key
    /// appeared. `new_max` is `Some(term)` exactly when the subtree's maximum no
    /// longer is the live term the parent's stored separator holds — `term` is the
    /// new maximum, aliasing the live stored entry that carries it. `None` means the
    /// stored separator still *is* the subtree's (live) maximum.
    Grown {
        /// Whether a new key was added (as opposed to an existing one replaced).
        added: bool,
        /// The subtree's new maximum, when the parent must rewrite its separator.
        new_max: Option<Eterm>,
    },
    /// The subtree was mutated and split in two: the parent adopts `right` — whose
    /// maximum is `right_max` — directly behind its child and rewrites that child's
    /// separator (the left half's maximum).
    Split {
        /// Whether a new key was added (as opposed to an existing one replaced).
        added: bool,
        /// The split-off right half.
        right: NodeId,
        /// The right half's maximum key.
        right_max: Eterm,
    },
}

impl Tree {
    /// A new, empty tree: a single empty root leaf (invariant 3's empty case).
    pub(crate) fn new() -> Tree {
        let mut store = NodeStore::new();
        let root = store.alloc(Node::leaf(Vec::new(), None));
        Tree {
            root,
            len: 0,
            store,
        }
    }

    /// The number of stored keys.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// An honest word count of the tree's node arena (the `ordered_set` interior
    /// contribution to `ets:info(Tab, memory)`); delegated to the store.
    pub(crate) fn memory(&self) -> usize {
        self.store.memory()
    }

    /// Walk the leaf chain left to right (ascending key order).
    pub(crate) fn iter(&self) -> LeafIter<'_> {
        LeafIter::new(&self.store, self.root)
    }

    // --------------------------------------------------------------- lookups

    /// The object stored under `key`, if the key is present.
    pub(crate) fn find(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        let mut id = self.root;
        loop {
            match self.store.node(id) {
                Node::Leaf(leaf) => {
                    let (pos, found) = search_entries(&leaf.entries, key, atoms);
                    return if found {
                        Some(leaf.entries[pos].obj)
                    } else {
                        None
                    };
                }
                Node::Branch(branch) => id = branch.child_at(child_index(branch, key, atoms)),
            }
        }
    }

    /// The smallest key, or `None` in an empty tree.
    pub(crate) fn first(&self) -> Option<Eterm> {
        self.store
            .leaf(self.leftmost_leaf())
            .entries
            .first()
            .map(|entry| entry.key)
    }

    /// The largest key, or `None` in an empty tree.
    pub(crate) fn last(&self) -> Option<Eterm> {
        self.store
            .leaf(self.rightmost_leaf())
            .entries
            .last()
            .map(|entry| entry.key)
    }

    /// The smallest key strictly greater than `key`. Works whether or not `key`
    /// is present ([`Tree::next_from`] with `inclusive == false`).
    pub(crate) fn next(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.next_from(key, false, atoms)
    }

    /// The largest key strictly less than `key`. Works whether or not `key` is
    /// present ([`Tree::prev_from`] with `inclusive == false`).
    pub(crate) fn prev(&self, key: Eterm, atoms: &AtomTable) -> Option<Eterm> {
        self.prev_from(key, false, atoms)
    }

    /// The smallest key `>= bound`, or `> bound` when `inclusive` is false.
    ///
    /// `bound` need not be a stored key: the descent lands in the leaf that would
    /// hold it and the search position ("insertion point") names the bound's
    /// neighbours directly.
    pub(crate) fn next_from(
        &self,
        bound: Eterm,
        inclusive: bool,
        atoms: &AtomTable,
    ) -> Option<Eterm> {
        if self.len == 0 {
            return None;
        }
        let (leaf, steps) = self.seek(bound, atoms);
        let entries = &self.store.leaf(leaf).entries;
        let (pos, found) = search_entries(entries, bound, atoms);
        // On a hit, an exclusive bound skips the equal key; on a miss `pos` is the
        // insertion point, so `entries[pos]` is already the first greater key.
        let start = if found && !inclusive { pos + 1 } else { pos };
        entries
            .get(start)
            .map(|entry| entry.key)
            .or_else(|| self.next_subtree_key(&steps))
    }

    /// The largest key `<= bound`, or `< bound` when `inclusive` is false. The
    /// mirror of [`Tree::next_from`]; `bound` need not be a stored key.
    pub(crate) fn prev_from(
        &self,
        bound: Eterm,
        inclusive: bool,
        atoms: &AtomTable,
    ) -> Option<Eterm> {
        if self.len == 0 {
            return None;
        }
        let (leaf, steps) = self.seek(bound, atoms);
        let entries = &self.store.leaf(leaf).entries;
        let (pos, found) = search_entries(entries, bound, atoms);
        // On a hit, an inclusive bound keeps the equal key; otherwise the key just
        // below the insertion point is the answer (absent when none is smaller).
        let candidate = if found && inclusive {
            Some(pos)
        } else {
            pos.checked_sub(1)
        };
        candidate
            .and_then(|idx| entries.get(idx))
            .map(|entry| entry.key)
            .or_else(|| self.prev_subtree_key(&steps))
    }

    // -------------------------------------------------------------- mutation

    /// Insert `object` under `key`, replacing the object when `key` already
    /// exists. Returns whether a *new* key was added.
    pub(crate) fn insert(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> bool {
        match self.insert_rec(self.root, key, object, atoms, false) {
            InsertStep::Grown { added, .. } => {
                if added {
                    self.len += 1;
                }
                added
            }
            InsertStep::Split {
                added,
                right,
                right_max,
            } => {
                self.split_root(right, right_max);
                if added {
                    self.len += 1;
                }
                added
            }
            // Only `insert_new` sets the refuse flag, and `insert` never does.
            InsertStep::Refused => {
                debug_assert!(false, "an upsert never refuses an existing key");
                false
            }
        }
    }

    /// Insert `object` under `key` only if `key` is absent; returns whether the
    /// key was added.
    ///
    /// One descent: the leaf that would hold `key` reports a hit as
    /// [`InsertStep::Refused`], so the presence test costs the caller nothing beyond
    /// the insertion descent it was going to make anyway.
    pub(crate) fn insert_new(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> bool {
        match self.insert_rec(self.root, key, object, atoms, true) {
            InsertStep::Refused => false,
            InsertStep::Grown { added, .. } => {
                if added {
                    self.len += 1;
                }
                added
            }
            InsertStep::Split {
                added,
                right,
                right_max,
            } => {
                self.split_root(right, right_max);
                if added {
                    self.len += 1;
                }
                added
            }
        }
    }

    /// Remove `key` (and its object); returns whether the key was present.
    pub(crate) fn delete(&mut self, key: Eterm, atoms: &AtomTable) -> bool {
        if self.len == 0 || !self.delete_rec(self.root, key, atoms) {
            return false;
        }
        self.len -= 1;
        self.collapse_root();
        true
    }

    /// Remove `key`'s object only when it is term-equal to `object`; returns
    /// whether something was removed.
    pub(crate) fn delete_object(&mut self, key: Eterm, object: Eterm, atoms: &AtomTable) -> bool {
        match self.find(key, atoms) {
            Some(stored) if eq(stored, object, atoms) => self.delete(key, atoms),
            _ => false,
        }
    }

    /// Descend to the leaf `key` would live in, inserting/replacing along the way,
    /// and report what changed on the way back up ([`InsertStep`]).
    ///
    /// `refuse_existing` is `insert_new`'s mode: a leaf that already holds `key`
    /// stops the descent with [`InsertStep::Refused`] and mutates nothing.
    fn insert_rec(
        &mut self,
        id: NodeId,
        key: Eterm,
        object: Eterm,
        atoms: &AtomTable,
        refuse_existing: bool,
    ) -> InsertStep {
        if self.store.node(id).is_leaf() {
            let (pos, found) = search_entries(&self.store.leaf(id).entries, key, atoms);
            if found {
                if refuse_existing {
                    return InsertStep::Refused;
                }
                // Replace *both* the key term and the object. The stored key may be
                // arithmetically equal to `key` without being the same term (`1` vs
                // `1.0`); the oracle's `ordered_set` makes the key follow the stored
                // object, so `first`/`last`/`next`/`prev`/iteration must observe the
                // new term. Replacing it is invariant-safe: `cmp_arith`-equality with
                // the old key keeps every ordering/search/separator comparison —
                // hence the tree's routing — unchanged.
                let leaf = self.store.leaf_mut(id);
                // A leaf's maximum is its *last* entry, so only replacing that one
                // moves the subtree maximum the parent's separator holds — and it
                // moves to the new key term, which aliases the stored entry (a live,
                // table-owned term) rather than the reclaimed storage of the object
                // that established the old separator.
                let is_max = pos + 1 == leaf.entries.len();
                let entry = &mut leaf.entries[pos];
                entry.key = key;
                entry.obj = object;
                return InsertStep::Grown {
                    added: false,
                    new_max: is_max.then_some(key),
                };
            }
            let leaf = self.store.leaf_mut(id);
            leaf.entries.insert(pos, Entry { key, obj: object });
            let is_max = pos + 1 == leaf.entries.len();
            if self.store.node(id).is_overfull() {
                let (right_max, right) = self.split_overfull(id);
                return InsertStep::Split {
                    added: true,
                    right,
                    right_max,
                };
            }
            return InsertStep::Grown {
                added: true,
                new_max: is_max.then_some(key),
            };
        }

        // A branch: descend into the child whose subtree can hold `key`.
        let idx = child_index(self.store.branch(id), key, atoms);
        let child = self.store.branch(id).child_at(idx);
        // A branch's own maximum is its *last* child's, so this is what decides
        // whether the change below has to be reported further up.
        let is_last_child = idx + 1 == self.store.branch(id).children.len();
        match self.insert_rec(child, key, object, atoms, refuse_existing) {
            InsertStep::Refused => InsertStep::Refused,
            InsertStep::Split {
                added,
                right,
                right_max,
            } => {
                // The child split; its right half is adopted behind it. The left
                // half's maximum — the separator just rewritten — is re-derived from
                // the child itself: a split is amortized-`O(1)` per insert (one per
                // `ORDER / 2` inserts), so this descent is not on the hot path.
                let left_max = self.store.subtree_max_key(child);
                self.store
                    .branch_mut(id)
                    .adopt_split(idx, left_max, right, right_max);
                if self.store.node(id).is_overfull() {
                    let (right_max, right) = self.split_overfull(id);
                    return InsertStep::Split {
                        added,
                        right,
                        right_max,
                    };
                }
                // The adopted right half is this subtree's last child exactly when
                // the split child was, so only then did this subtree's maximum move
                // (and it moved to the right half's maximum — a live term).
                InsertStep::Grown {
                    added,
                    new_max: is_last_child.then_some(right_max),
                }
            }
            InsertStep::Grown { added, new_max } => {
                if let Some(new_max) = new_max {
                    // The descended-into subtree's maximum was rewritten, so the
                    // separator that points at it must follow — with the live term
                    // the child handed up. Leaving it alone would keep a term that
                    // aliases an object the caller may already have reclaimed.
                    self.store.branch_mut(id).set_separator(idx, new_max);
                }
                InsertStep::Grown {
                    added,
                    new_max: if is_last_child { new_max } else { None },
                }
            }
        }
    }

    /// Split an over-full node in half, leaving the left half in place and
    /// returning the right half with its maximum key.
    ///
    /// A node overflows at `ORDER + 1` entries/children and is cut at `ORDER / 2`,
    /// so both halves keep at least [`MIN_OCCUPANCY`]; a leaf's chain link now runs
    /// through the new right leaf. Splitting moves entries only — it needs no term
    /// comparison, so it takes no atom table.
    fn split_overfull(&mut self, id: NodeId) -> (Eterm, NodeId) {
        match self.store.take(id) {
            Node::Leaf(mut leaf) => {
                let tail = leaf.entries.split_off(ORDER / 2);
                // The right half of an over-full leaf holds at least one entry.
                let right_max = tail.last().expect("a split leaf keeps entries").key;
                let next = leaf.next;
                let right = self.store.alloc(Node::leaf(tail, next));
                leaf.next = Some(right);
                self.store.put(id, Node::Leaf(leaf));
                (right_max, right)
            }
            Node::Branch(mut branch) => {
                let keys = branch.keys.split_off(ORDER / 2);
                let children = branch.children.split_off(ORDER / 2);
                // The right half of an over-full branch holds at least one child.
                let right = self.store.alloc(Node::branch(keys, children));
                // Re-derive the right half's maximum from its own rightmost leaf
                // rather than from the last *stored* separator it inherited, which may
                // alias an object the caller has already replaced and reclaimed.
                let right_max = self.store.subtree_max_key(right);
                self.store.put(id, Node::Branch(branch));
                (right_max, right)
            }
        }
    }

    /// Grow the tree by one level: the old root and its split-off right half
    /// become the two children of a fresh root.
    fn split_root(&mut self, right: NodeId, right_max: Eterm) {
        let left = self.root;
        let left_max = self.store.subtree_max_key(left);
        self.root = self
            .store
            .alloc(Node::branch(vec![left_max, right_max], vec![left, right]));
    }

    /// Descend to the leaf `key` would live in, deleting there.
    ///
    /// Returns whether the key was removed; on the way back up it rebalances every
    /// node the delete under-filled (invariant 3).
    fn delete_rec(&mut self, id: NodeId, key: Eterm, atoms: &AtomTable) -> bool {
        if self.store.node(id).is_leaf() {
            let (pos, found) = search_entries(&self.store.leaf(id).entries, key, atoms);
            if !found {
                return false;
            }
            self.store.leaf_mut(id).entries.remove(pos);
            return true;
        }

        let idx = child_index(self.store.branch(id), key, atoms);
        let child = self.store.branch(id).child_at(idx);
        if !self.delete_rec(child, key, atoms) {
            return false;
        }
        // The delete may have removed the child's maximum, so the separator at
        // `idx` is stale. Which node now holds the child's keys, and therefore
        // which separator has to be re-derived, is what `rebalance` reports.
        let settled = if self.store.node(child).is_underfull() {
            // Below the minimum: borrow from or merge with a sibling.
            self.rebalance(id, idx)
        } else {
            idx
        };
        let settled_child = self.store.branch(id).child_at(settled);
        let settled_max = self.store.subtree_max_key(settled_child);
        self.store
            .branch_mut(id)
            .set_separator(settled, settled_max);
        true
    }

    /// Restore invariant 3 for `parent`'s under-full child `idx`: borrow one
    /// entry/child from a sibling with a spare, or merge with a sibling.
    ///
    /// Returns the index (in `parent`) of the node that now holds `child`'s keys:
    /// `idx` itself for a borrow (the child stays put) or `idx - 1` when the child
    /// is merged into its left sibling. The caller re-derives *that* node's
    /// separator, because the delete that triggered the rebalance may have removed
    /// the child's maximum; the sibling separator a borrow invalidates (the one of
    /// a left sibling that gives up its maximum) is repaired here.
    ///
    /// A borrow leaves the parent's child count alone; a merge drops it by one, so
    /// the caller's own parent then sees *it* under-full in turn.
    fn rebalance(&mut self, parent: NodeId, idx: usize) -> usize {
        let child = self.store.branch(parent).child_at(idx);
        let leaf = self.store.node(child).is_leaf();

        if idx > 0 {
            let left = self.store.branch(parent).child_at(idx - 1);
            if self.store.node(left).occupancy() > MIN_OCCUPANCY {
                if leaf {
                    self.borrow_leaf_from_left(parent, idx, left, child);
                } else {
                    self.borrow_branch_from_left(parent, idx, left, child);
                }
                return idx;
            }
        }
        if idx + 1 < self.store.branch(parent).children.len() {
            let right = self.store.branch(parent).child_at(idx + 1);
            if self.store.node(right).occupancy() > MIN_OCCUPANCY {
                if leaf {
                    self.borrow_leaf_from_right(child, right);
                } else {
                    self.borrow_branch_from_right(child, right);
                }
                return idx;
            }
        }

        // No sibling has a spare entry: merge, which fits because both siblings
        // hold at most `MIN_OCCUPANCY` (so at most `ORDER` together).
        if idx > 0 {
            let left = self.store.branch(parent).child_at(idx - 1);
            self.merge(parent, idx - 1, left, child);
            idx - 1
        } else {
            let right = self.store.branch(parent).child_at(idx + 1);
            self.merge(parent, idx, child, right);
            idx
        }
    }

    /// Move the last entry of `left` to the front of `child`. Neither maximum
    /// changes, so only `left`'s separator moves.
    fn borrow_leaf_from_left(&mut self, parent: NodeId, idx: usize, left: NodeId, child: NodeId) {
        let entry = self
            .store
            .leaf_mut(left)
            .entries
            .pop()
            .expect("a borrowing sibling holds an entry to give");
        self.store.leaf_mut(child).entries.insert(0, entry);
        let left_max = self.store.subtree_max_key(left);
        self.store
            .branch_mut(parent)
            .set_separator(idx - 1, left_max);
    }

    /// Move `left`'s last child to the front of `child` (branch case).
    fn borrow_branch_from_left(&mut self, parent: NodeId, idx: usize, left: NodeId, child: NodeId) {
        let key = self
            .store
            .branch_mut(left)
            .keys
            .pop()
            .expect("a borrowing sibling holds a child to give");
        let moved = self
            .store
            .branch_mut(left)
            .children
            .pop()
            .expect("a branching sibling has a child per separator");
        self.store.branch_mut(child).keys.insert(0, key);
        self.store.branch_mut(child).children.insert(0, moved);
        let left_max = self.store.subtree_max_key(left);
        self.store
            .branch_mut(parent)
            .set_separator(idx - 1, left_max);
    }

    /// Move the first entry of `right` to the end of `child`: `child`'s maximum
    /// becomes the moved key (which the caller re-derives), `right`'s is unchanged.
    fn borrow_leaf_from_right(&mut self, child: NodeId, right: NodeId) {
        let entry = self.store.leaf_mut(right).entries.remove(0);
        self.store.leaf_mut(child).entries.push(entry);
    }

    /// Move `right`'s first child to the end of `child` (branch case).
    fn borrow_branch_from_right(&mut self, child: NodeId, right: NodeId) {
        let key = self.store.branch_mut(right).keys.remove(0);
        let moved = self.store.branch_mut(right).children.remove(0);
        self.store.branch_mut(child).keys.push(key);
        self.store.branch_mut(child).children.push(moved);
    }

    /// Merge `right` into `left` and unlink `right` from `parent`.
    ///
    /// `left_idx` is `left`'s position among `parent`'s children; `right` sits
    /// right behind it, so the parent loses its separator and child at
    /// `left_idx + 1`. The merged node inherits `right`'s maximum, which the caller
    /// re-derives from it. Only `left`'s successor link is carried over, which is
    /// what keeps every still-live leaf reachable from the chain.
    fn merge(&mut self, parent: NodeId, left_idx: usize, left: NodeId, right: NodeId) {
        match (self.store.take(left), self.store.remove(right)) {
            (Node::Leaf(mut merged), Node::Leaf(tail)) => {
                merged.entries.extend(tail.entries);
                merged.next = tail.next;
                self.store.put(left, Node::Leaf(merged));
            }
            (Node::Branch(mut merged), Node::Branch(tail)) => {
                merged.keys.extend(tail.keys);
                merged.children.extend(tail.children);
                self.store.put(left, Node::Branch(merged));
            }
            // Siblings share a depth, so they are always the same kind.
            _ => unreachable!("the tree merges nodes of the same kind"),
        }
        let parent = self.store.branch_mut(parent);
        parent.keys.remove(left_idx + 1);
        parent.children.remove(left_idx + 1);
    }

    /// Drop a root branch that is left with a single child: that child is the
    /// tree's only content, so it becomes the root (keeping the root exempt from
    /// the occupancy minimum).
    fn collapse_root(&mut self) {
        loop {
            let sole_child = match self.store.node(self.root) {
                Node::Branch(branch) if branch.children.len() == 1 => Some(branch.first_child()),
                _ => None,
            };
            match sole_child {
                Some(child) => {
                    self.store.free(self.root);
                    self.root = child;
                }
                None => return,
            }
        }
    }

    // ------------------------------------------------------------ navigation

    /// Descend from the root to the leaf `key` would live in, returning that leaf
    /// and the `(branch, child index)` steps taken to reach it.
    fn seek(&self, key: Eterm, atoms: &AtomTable) -> (NodeId, Vec<(NodeId, usize)>) {
        let mut steps = Vec::new();
        let mut id = self.root;
        while !self.store.node(id).is_leaf() {
            let idx = child_index(self.store.branch(id), key, atoms);
            steps.push((id, idx));
            id = self.store.branch(id).child_at(idx);
        }
        (id, steps)
    }

    /// The first key of the first subtree to the right of a walked path, i.e. the
    /// successor once the path's own leaf is exhausted.
    fn next_subtree_key(&self, steps: &[(NodeId, usize)]) -> Option<Eterm> {
        for &(branch, idx) in steps.iter().rev() {
            let node = self.store.branch(branch);
            if idx + 1 < node.children.len() {
                let leaf = self.leftmost_leaf_from(node.child_at(idx + 1));
                return self.store.leaf(leaf).entries.first().map(|entry| entry.key);
            }
        }
        None
    }

    /// The last key of the first subtree to the left of a walked path, i.e. the
    /// predecessor once the path's own leaf is exhausted.
    fn prev_subtree_key(&self, steps: &[(NodeId, usize)]) -> Option<Eterm> {
        for &(branch, idx) in steps.iter().rev() {
            let node = self.store.branch(branch);
            if idx > 0 {
                let leaf = self.rightmost_leaf_from(node.child_at(idx - 1));
                return self.store.leaf(leaf).entries.last().map(|entry| entry.key);
            }
        }
        None
    }

    /// The leftmost leaf of the whole tree.
    fn leftmost_leaf(&self) -> NodeId {
        self.leftmost_leaf_from(self.root)
    }

    /// The rightmost leaf of the whole tree.
    fn rightmost_leaf(&self) -> NodeId {
        self.rightmost_leaf_from(self.root)
    }

    /// The leftmost leaf of the subtree at `id`.
    fn leftmost_leaf_from(&self, id: NodeId) -> NodeId {
        let mut id = id;
        while !self.store.node(id).is_leaf() {
            id = self.store.branch(id).first_child();
        }
        id
    }

    /// The rightmost leaf of the subtree at `id`.
    fn rightmost_leaf_from(&self, id: NodeId) -> NodeId {
        let mut id = id;
        while !self.store.node(id).is_leaf() {
            id = self.store.branch(id).last_child();
        }
        id
    }

    // ---------------------------------------------------- invariant checking

    /// The number of levels: `1` for a bare leaf. The test suite uses it to assert
    /// that a large table really did grow a multi-level tree (rather than a flat
    /// run of leaves the lookups never had to search).
    #[cfg(test)]
    pub(crate) fn height(&self) -> usize {
        let mut height = 1;
        let mut id = self.root;
        while !self.store.node(id).is_leaf() {
            id = self.store.branch(id).first_child();
            height += 1;
        }
        height
    }

    /// Walk the whole tree and panic on the first broken invariant (1–5 above).
    ///
    /// Test-only: it is `O(n)` and exists so the suite can assert the structural
    /// invariants after arbitrary mutation sequences (a wrong-but-fast structure
    /// would otherwise still pass a lookup/iteration test).
    #[cfg(test)]
    pub(crate) fn check_invariants(&self, atoms: &AtomTable) {
        if self.len == 0 {
            assert!(
                self.store.node(self.root).is_leaf(),
                "an empty tree is a bare leaf"
            );
            assert!(
                self.store.leaf(self.root).entries.is_empty(),
                "an empty tree stores no entries"
            );
            assert_eq!(
                self.store.live_nodes(),
                1,
                "an empty tree owns exactly its root leaf"
            );
            return;
        }

        let mut seen = Vec::new();
        let mut leaves = Vec::new();
        let (root_max, keys, _height) =
            self.check_subtree(self.root, true, &mut seen, &mut leaves, atoms);
        assert!(root_max.is_some(), "a non-empty tree has a maximum");
        assert_eq!(keys, self.len, "the subtree holds exactly `len` keys");
        assert_eq!(
            seen.len(),
            self.store.live_nodes(),
            "every live node is reachable from the root"
        );

        // The chain runs through exactly the leaves, once each, in order.
        let mut chained = Vec::new();
        let mut walk = Some(leaves[0]);
        while let Some(id) = walk {
            chained.push(id);
            walk = self.store.leaf(id).next;
        }
        assert_eq!(
            chained, leaves,
            "the leaf chain visits every leaf exactly once, in order"
        );

        // …and their entries ascend strictly and cover the tree.
        let mut previous: Option<Eterm> = None;
        let mut chained_keys = 0usize;
        for &id in &leaves {
            for entry in &self.store.leaf(id).entries {
                if let Some(prev) = previous {
                    assert_eq!(
                        cmp_arith(prev, entry.key, atoms),
                        core::cmp::Ordering::Less,
                        "the leaf chain ascends"
                    );
                }
                previous = Some(entry.key);
                chained_keys += 1;
            }
        }
        assert_eq!(chained_keys, self.len, "the leaf chain covers every key");
    }

    /// Check the subtree at `id` and return
    /// `(maximum key, number of keys, height)`.
    #[cfg(test)]
    fn check_subtree(
        &self,
        id: NodeId,
        is_root: bool,
        seen: &mut Vec<NodeId>,
        leaves: &mut Vec<NodeId>,
        atoms: &AtomTable,
    ) -> (Option<Eterm>, usize, usize) {
        seen.push(id);
        match self.store.node(id) {
            Node::Leaf(leaf) => {
                assert!(!leaf.entries.is_empty(), "a reachable leaf holds entries");
                // Invariant 3's upper bound: the fan-out `ORDER` bounds every
                // node, so a split is amortized `O(1)` per insert (one per
                // `ORDER / 2` inserts) and no node can grow without bound.
                assert!(
                    leaf.entries.len() <= ORDER,
                    "a leaf holds at most {ORDER} entries (has {})",
                    leaf.entries.len()
                );
                if !is_root {
                    assert!(
                        leaf.entries.len() >= MIN_OCCUPANCY,
                        "a leaf below the root keeps at least {MIN_OCCUPANCY} entries"
                    );
                }
                for pair in leaf.entries.windows(2) {
                    assert_eq!(
                        cmp_arith(pair[0].key, pair[1].key, atoms),
                        core::cmp::Ordering::Less,
                        "leaf entries ascend strictly"
                    );
                }
                leaves.push(id);
                (
                    leaf.entries.last().map(|entry| entry.key),
                    leaf.entries.len(),
                    1,
                )
            }
            Node::Branch(branch) => {
                assert_eq!(
                    branch.keys.len(),
                    branch.children.len(),
                    "a branch has one separator per child"
                );
                // Invariant 3's upper bound (see the leaf arm): `ORDER` bounds a
                // branch's child count too, so a split is amortized `O(1)`.
                assert!(
                    branch.children.len() <= ORDER,
                    "a branch keeps at most {ORDER} children (has {})",
                    branch.children.len()
                );
                if is_root {
                    assert!(
                        branch.children.len() >= 2,
                        "the root branch has at least two children"
                    );
                } else {
                    assert!(
                        branch.children.len() >= MIN_OCCUPANCY,
                        "a branch below the root keeps at least {MIN_OCCUPANCY} children"
                    );
                }
                let mut keys = 0usize;
                let mut height = None;
                let mut subtree_max: Option<Eterm> = None;
                for (idx, &child) in branch.children.iter().enumerate() {
                    let (max, count, child_height) =
                        self.check_subtree(child, false, seen, leaves, atoms);
                    let max = max.expect("a reachable subtree has a maximum");
                    assert_eq!(
                        cmp_arith(branch.separator(idx), max, atoms),
                        core::cmp::Ordering::Equal,
                        "separator {idx} is exactly its child's (subtree) maximum"
                    );
                    // …and it is the *live term* that carries that maximum, not a
                    // term that merely compares equal: the recursion handed up the
                    // rightmost leaf's last entry key, which is the term a separator
                    // must alias (see `NodeStore::subtree_max_key`). A separator left
                    // holding the term of an object that has since been replaced
                    // would dangle once the caller reclaims it — the failure mode
                    // this pins.
                    assert_eq!(
                        branch.separator(idx),
                        max,
                        "separator {idx} is the child's live maximum term"
                    );
                    if let Some(previous) = subtree_max {
                        assert_eq!(
                            cmp_arith(previous, max, atoms),
                            core::cmp::Ordering::Less,
                            "child subtrees ascend strictly"
                        );
                    }
                    subtree_max = Some(max);
                    match height {
                        None => height = Some(child_height),
                        Some(known) => {
                            assert_eq!(known, child_height, "every leaf sits at the same depth")
                        }
                    }
                    keys += count;
                }
                let height = height.expect("a branch has children") + 1;
                (subtree_max, keys, height)
            }
        }
    }
}
