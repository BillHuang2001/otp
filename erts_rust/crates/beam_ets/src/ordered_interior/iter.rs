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

//! Ordered iteration over the B+-tree's leaf chain.
//!
//! Every leaf keeps a link to the next leaf in ascending key order (maintained by
//! splits, merges and borrows in [`super::tree`]), so walking the leaves is a
//! pointer chase with no comparisons and no recursion. The iterator borrows the
//! node arena, so it stays valid only while the tree is not mutated — which is
//! exactly the lifetime the [`crate::table::Interior`] iterator returns.

use super::node::{Entry, NodeId, NodeStore};

/// A left-to-right walk over the coalesced leaves, yielding `(key, object)`
/// entries in ascending arithmetic key order (`beam_term::compare::cmp_arith`).
pub(crate) struct LeafIter<'a> {
    /// The arena the leaves live in.
    store: &'a NodeStore,
    /// The leaf being drained; `None` once the chain ends.
    leaf: Option<NodeId>,
    /// The position within [`LeafIter::leaf`].
    pos: usize,
}

impl<'a> LeafIter<'a> {
    /// An iterator over the whole tree rooted at `root` (starting at its leftmost
    /// leaf).
    pub(crate) fn new(store: &'a NodeStore, root: NodeId) -> LeafIter<'a> {
        let mut id = root;
        while !store.node(id).is_leaf() {
            id = store.branch(id).first_child();
        }
        LeafIter {
            store,
            leaf: Some(id),
            pos: 0,
        }
    }
}

impl Iterator for LeafIter<'_> {
    type Item = Entry;

    fn next(&mut self) -> Option<Entry> {
        let store = self.store;
        loop {
            let leaf = store.leaf(self.leaf?);
            match leaf.entries.get(self.pos) {
                Some(entry) => {
                    self.pos += 1;
                    return Some(*entry);
                }
                None => {
                    // This leaf is drained; continue in its successor. The root
                    // leaf may be empty and unlinked, which ends the walk.
                    self.leaf = leaf.next;
                    self.pos = 0;
                }
            }
        }
    }
}
