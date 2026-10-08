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

//! B+-tree nodes, the node arena, and the small node-surgery helpers the tree
//! operations in [`super`] are built from.
//!
//! The representation is deliberately flat and arena-backed: nodes live in a slot
//! table addressed by [`NodeId`], so a rebalance never relocates a node and the
//! left-to-right leaf chain can be plain [`NodeId`] links. Term order is never
//! re-implemented here — node search delegates to `beam_term::compare::cmp_arith`
//! (the **arithmetic key order** ERTS `ordered_set` uses, where `1` and `1.0` are
//! the same key).
//!
//! Every accessor that could fail only fails on an *internal* invariant (a
//! `NodeId` or an index the tree itself derived): those carry a documented
//! `expect`/`unreachable!` instead of silently returning a wrong answer. They are
//! unreachable from the public API.

use core::cmp::Ordering;
use core::mem::size_of;

use beam_term::prelude::*;

/// The B+-tree order `K`: a leaf holds at most `ORDER` entries, a branch at most
/// `ORDER` children.
pub(crate) const ORDER: usize = 32;

/// The smallest occupancy a node *below the root* may keep (`ORDER / 2`). A
/// delete that drops a node below it borrows from — or merges with — a sibling.
pub(crate) const MIN_OCCUPANCY: usize = ORDER / 2;

/// A slot in the node arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct NodeId(usize);

impl NodeId {
    /// The arena slot this id addresses.
    pub(crate) fn index(self) -> usize {
        self.0
    }
}

/// One leaf entry: a key and the single object stored under it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Entry {
    /// The sort key.
    pub(crate) key: Eterm,
    /// The object stored under `key`.
    pub(crate) obj: Eterm,
}

/// A leaf: entries sorted by `beam_term::compare::cmp_arith` (arithmetic key
/// order) plus the left-to-right leaf link that makes ordered iteration a pointer
/// walk.
pub(crate) struct LeafNode {
    /// The sorted entries (`1..=ORDER` of them).
    pub(crate) entries: Vec<Entry>,
    /// The next leaf in ascending key order, if any.
    pub(crate) next: Option<NodeId>,
}

/// A branch: `keys[i]` is the exact greatest key of `children[i]`'s subtree.
pub(crate) struct BranchNode {
    /// The separators, strictly ascending in arithmetic key order
    /// (`beam_term::compare::cmp_arith`).
    pub(crate) keys: Vec<Eterm>,
    /// The children (`keys.len() == children.len()`, `1..=ORDER` of them).
    pub(crate) children: Vec<NodeId>,
}

impl BranchNode {
    /// `children[idx]`.
    ///
    /// Infallible: every index the tree derives from a descent or from `keys` is
    /// within `children` (`keys.len() == children.len()` is maintained by every
    /// operation), so the `expect` guards an internal invariant — never runtime
    /// input.
    pub(crate) fn child_at(&self, idx: usize) -> NodeId {
        self.children
            .get(idx)
            .copied()
            .expect("a branch child index from the tree names a child")
    }

    /// The first child (a branch is never childless).
    pub(crate) fn first_child(&self) -> NodeId {
        self.child_at(0)
    }

    /// The last child (a branch is never childless).
    pub(crate) fn last_child(&self) -> NodeId {
        self.child_at(self.children.len().saturating_sub(1))
    }

    /// `keys[idx]`, the exact greatest key of `children[idx]`'s subtree.
    pub(crate) fn separator(&self, idx: usize) -> Eterm {
        self.keys
            .get(idx)
            .copied()
            .expect("a branch separator index from the tree names a key")
    }

    /// Replace `keys[idx]`.
    pub(crate) fn set_separator(&mut self, idx: usize, key: Eterm) {
        match self.keys.get_mut(idx) {
            Some(slot) => *slot = key,
            None => unreachable!("a branch separator index from the tree names a key"),
        }
    }

    /// Replace `children[idx]`'s separator with `node_max` and adopt `right` right
    /// behind it, with separator `right_max` — i.e. what a split of that child
    /// produces. The caller guarantees `idx < children.len()`.
    pub(crate) fn adopt_split(
        &mut self,
        idx: usize,
        node_max: Eterm,
        right: NodeId,
        right_max: Eterm,
    ) {
        self.set_separator(idx, node_max);
        let at = idx + 1;
        self.keys.insert(at, right_max);
        self.children.insert(at, right);
    }
}

/// A tree node.
pub(crate) enum Node {
    /// A leaf (a sorted entry run plus its chain link).
    Leaf(LeafNode),
    /// A branch (children plus their subtree maxima).
    Branch(BranchNode),
}

impl Node {
    /// A leaf node.
    pub(crate) fn leaf(entries: Vec<Entry>, next: Option<NodeId>) -> Node {
        Node::Leaf(LeafNode { entries, next })
    }

    /// A branch node.
    pub(crate) fn branch(keys: Vec<Eterm>, children: Vec<NodeId>) -> Node {
        Node::Branch(BranchNode { keys, children })
    }

    /// Whether this node is a leaf.
    pub(crate) fn is_leaf(&self) -> bool {
        matches!(self, Node::Leaf(_))
    }

    /// The number of entries (leaf) or children (branch).
    pub(crate) fn occupancy(&self) -> usize {
        match self {
            Node::Leaf(leaf) => leaf.entries.len(),
            Node::Branch(branch) => branch.children.len(),
        }
    }

    /// The greatest key in this node's own arrays (`entries.last()` / `keys.last()`).
    ///
    /// Infallible: the tree never stores an empty node — a node is created either
    /// as a one-entry root leaf or as half of an over-full node — so the `expect`s
    /// guard an internal invariant, never runtime input.
    pub(crate) fn max_key(&self) -> Eterm {
        match self {
            Node::Leaf(leaf) => leaf
                .entries
                .last()
                .map(|entry| entry.key)
                .expect("a leaf is never empty"),
            Node::Branch(branch) => branch
                .keys
                .last()
                .copied()
                .expect("a branch is never childless"),
        }
    }

    /// Whether the node holds more than [`ORDER`] entries/children (so it must
    /// split).
    pub(crate) fn is_overfull(&self) -> bool {
        self.occupancy() > ORDER
    }

    /// Whether the node holds fewer than [`MIN_OCCUPANCY`] entries/children (so it
    /// must borrow from or merge with a sibling). The root is exempt.
    pub(crate) fn is_underfull(&self) -> bool {
        self.occupancy() < MIN_OCCUPANCY
    }
}

/// The node arena: a slot table addressed by [`NodeId`] with a free list, so a
/// rebalance never relocates a node (and leaves can be chained with plain ids).
pub(crate) struct NodeStore {
    /// The live slots; `None` marks a slot that [`NodeStore::remove`] retired.
    slots: Vec<Option<Node>>,
    /// The retired slot ids, reused by [`NodeStore::alloc`].
    retired: Vec<NodeId>,
}

impl NodeStore {
    /// A new, empty arena.
    pub(crate) fn new() -> NodeStore {
        NodeStore {
            slots: Vec::new(),
            retired: Vec::new(),
        }
    }

    /// Store `node` in a reused or fresh slot and return its id.
    pub(crate) fn alloc(&mut self, node: Node) -> NodeId {
        match self.retired.pop() {
            Some(id) => {
                self.slots[id.index()] = Some(node);
                id
            }
            None => {
                let id = NodeId(self.slots.len());
                self.slots.push(Some(node));
                id
            }
        }
    }

    /// The node at `id`.
    ///
    /// Infallible: [`NodeId`]s are handed out only by [`NodeStore::alloc`] and
    /// retired only by [`NodeStore::free`], which the tree calls exclusively after
    /// unlinking the node (and rewiring the leaf chain) — so every id the tree
    /// still holds addresses a live slot. The `expect` guards that internal
    /// invariant, never runtime input.
    pub(crate) fn node(&self, id: NodeId) -> &Node {
        self.slots
            .get(id.index())
            .and_then(Option::as_ref)
            .expect("live B+-tree node")
    }

    /// The leaf at `id` (depth uniformity makes the variant infallible).
    pub(crate) fn leaf(&self, id: NodeId) -> &LeafNode {
        match self.node(id) {
            Node::Leaf(leaf) => leaf,
            Node::Branch(_) => unreachable!("a leaf id from the tree names a leaf"),
        }
    }

    /// Mutable access to the leaf at `id`.
    pub(crate) fn leaf_mut(&mut self, id: NodeId) -> &mut LeafNode {
        match self.slot_mut(id) {
            Node::Leaf(leaf) => leaf,
            Node::Branch(_) => unreachable!("a leaf id from the tree names a leaf"),
        }
    }

    /// The branch at `id`.
    pub(crate) fn branch(&self, id: NodeId) -> &BranchNode {
        match self.node(id) {
            Node::Branch(branch) => branch,
            Node::Leaf(_) => unreachable!("a branch id from the tree names a branch"),
        }
    }

    /// Mutable access to the branch at `id`.
    pub(crate) fn branch_mut(&mut self, id: NodeId) -> &mut BranchNode {
        match self.slot_mut(id) {
            Node::Branch(branch) => branch,
            Node::Leaf(_) => unreachable!("a branch id from the tree names a branch"),
        }
    }

    /// Retire `id` and drop whatever it holds.
    ///
    /// The caller must have unlinked the node first (nothing may still point at
    /// it).
    pub(crate) fn free(&mut self, id: NodeId) {
        drop(self.remove(id));
    }

    /// Take the node at `id` out of the arena and retire its slot; the node is
    /// handed to the caller (who unlinks it as part of the same step).
    pub(crate) fn remove(&mut self, id: NodeId) -> Node {
        let node = self.take(id);
        self.retired.push(id);
        node
    }

    /// Take the node at `id` out of the arena, leaving its slot empty.
    ///
    /// The caller must put a node back ([`NodeStore::put`]) or retire the slot
    /// ([`NodeStore::free`]); the tree takes a node only to split, merge or free
    /// it.
    pub(crate) fn take(&mut self, id: NodeId) -> Node {
        self.slots
            .get_mut(id.index())
            .and_then(Option::take)
            .expect("live B+-tree node")
    }

    /// Put `node` back into the empty slot at `id`.
    pub(crate) fn put(&mut self, id: NodeId, node: Node) {
        debug_assert!(
            self.slots
                .get(id.index())
                .map(Option::is_none)
                .unwrap_or(false),
            "put back into a taken slot"
        );
        self.slots[id.index()] = Some(node);
    }

    /// An honest, non-negative word count of the arena's backing storage —
    /// `ets:info(Tab, memory)`'s `ordered_set` interior contribution.
    ///
    /// Every array is measured at **capacity**, so the figure never under-reports
    /// what is actually allocated and never decreases as nodes are inserted (the
    /// slot table only grows, and a fresh slot reuses a retired id rather than
    /// shrinking anything). Words are `Eterm`-sized; `slots`/`retired` hold
    /// `Option<Node>`/`NodeId` elements, counted by their rounded-up word size.
    pub(crate) fn memory(&self) -> usize {
        let mut words = self.slots.capacity() * words_of::<Option<Node>>();
        words += self.retired.capacity() * words_of::<NodeId>();
        for slot in &self.slots {
            match slot {
                Some(Node::Leaf(leaf)) => {
                    words += leaf.entries.capacity() * words_of::<Entry>();
                }
                Some(Node::Branch(branch)) => {
                    words += branch.keys.capacity() * words_of::<Eterm>();
                    words += branch.children.capacity() * words_of::<NodeId>();
                }
                // A slot taken for a split/merge is transiently empty; it owns no
                // storage beyond the slot word already counted above.
                None => {}
            }
        }
        words
    }

    /// The number of live nodes — reachable from the root or not. The test suite
    /// uses it to prove that a rebalance leaves no slot behind.
    #[cfg(test)]
    pub(crate) fn live_nodes(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// The node at `id`, mutably.
    fn slot_mut(&mut self, id: NodeId) -> &mut Node {
        self.slots
            .get_mut(id.index())
            .and_then(Option::as_mut)
            .expect("live B+-tree node")
    }
}

/// The position of `key` among the sorted `entries`: `(pos, true)` on a hit,
/// `(pos, false)` with the ascending insertion point on a miss. Ordering is the
/// **arithmetic** key order (`beam_term::compare::cmp_arith`, the one authority on
/// the order ERTS `ordered_set` uses), so `1` and `1.0` are the *same* key.
pub(crate) fn search_entries(entries: &[Entry], key: Eterm, atoms: &AtomTable) -> (usize, bool) {
    match entries.binary_search_by(|entry| cmp_arith(entry.key, key, atoms)) {
        Ok(pos) => (pos, true),
        Err(pos) => (pos, false),
    }
}

/// The child of `branch` to descend into for `key`: its first child whose subtree
/// maximum is `>= key` in **arithmetic** key order (every later child holds
/// strictly greater keys), or its last child when `key` exceeds all separators.
///
/// Infallible: a branch always has at least one child.
pub(crate) fn child_index(branch: &BranchNode, key: Eterm, atoms: &AtomTable) -> usize {
    for idx in 0..branch.keys.len() {
        if cmp_arith(branch.separator(idx), key, atoms) != Ordering::Less {
            return idx;
        }
    }
    branch
        .children
        .len()
        .checked_sub(1)
        .expect("a branch is never childless")
}

/// The number of `Eterm`-sized words a value of type `T` occupies (rounded up).
pub(crate) fn words_of<T>() -> usize {
    size_of::<T>().div_ceil(size_of::<Eterm>())
}
