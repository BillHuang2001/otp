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

//! The external-thing **node entry**: a refcounted `(node, creation)` identity.
//!
//! Ground truth: the legacy C node registry (`erts_ref_node_entry` /
//! `erts_deref_node_entry`, `erts/emulator/beam/erl_node_tables.h:280-334`). Copying an
//! `ExternalThing` bumps its node's refcount (`copy.c:882-892`, `erts_ref_node_entry`) and
//! releasing the copy dereferences it again (`erl_gc.c:3105`, `erl_message.c:181`).
//!
//! # M1 substitution
//!
//! There is **no node registry anywhere in the workspace**, so there is no `ErlNode *` to point
//! at. The same substitution `beam_term::thing` documents for an `ExternalThing`'s `node`/`next`
//! slots applies here: the node is the sysname **atom term** plus its `creation` number. A
//! [`NodeEntry`] therefore models exactly the refcounted identity `(node, creation)` a real
//! registry would key on — the count is structural bookkeeping that future-proofs a real registry
//! (whose `ErlNode *` would replace the atom), not an observable Erlang-level value.
//!
//! # The count
//!
//! The count is the number of **references registered in the owning table** — the destination
//! table registers one reference per copied external thing, so a term holding two references to
//! the same node leaves one entry with count `2` (the C's per-node count, minus the global
//! cross-process sharing a real registry would add). A table keeps **one entry per distinct
//! node** and releases every reference it registered when it is dropped; `Copy` of the *payload*
//! does not, so [`OffHeapEntry`](crate::off_heap::OffHeapTable) is the only owner of a count.

use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_term::prelude::Eterm;

/// The shared interior of a [`NodeEntry`]: the node identity and its reference count.
struct NodeInner {
    node: Eterm,
    creation: u32,
    refs: AtomicUsize,
}

/// A refcounted reference to the node of an external pid, port or reference.
///
/// Two entries are the *same* node when their node atom and `creation` match; a table holds
/// **one entry per distinct node** while its [`refcount`](NodeEntry::refcount) counts the
/// references registered against it. See the module docs for the M1 substitution.
pub struct NodeEntry {
    inner: Arc<NodeInner>,
}

impl NodeEntry {
    /// A new entry for the node `(node, creation)`, holding **one** reference.
    pub fn new(node: Eterm, creation: u32) -> NodeEntry {
        NodeEntry {
            inner: Arc::new(NodeInner {
                node,
                creation,
                refs: AtomicUsize::new(1),
            }),
        }
    }

    /// The node identity term (the sysname atom, per the M1 substitution).
    pub fn node(&self) -> Eterm {
        self.inner.node
    }

    /// The node's `creation` number.
    pub fn creation(&self) -> u32 {
        self.inner.creation
    }

    /// The number of references registered against this node (`erts_ref_node_entry`'s count).
    pub fn refcount(&self) -> usize {
        self.inner.refs.load(Ordering::Relaxed)
    }

    /// Whether this entry and `other` denote the same node (`node` + `creation`).
    pub fn same_node(&self, other: &NodeEntry) -> bool {
        self.identity_eq(other.inner.node, other.inner.creation)
    }

    /// Whether this entry denotes the node `(node, creation)`.
    pub fn identity_eq(&self, node: Eterm, creation: u32) -> bool {
        self.inner.node.raw() == node.raw() && self.inner.creation == creation
    }

    /// `erts_ref_node_entry`: registers one more reference to this node.
    pub(crate) fn add_reference(&self) {
        self.inner.refs.fetch_add(1, Ordering::Relaxed);
    }

    /// `erts_deref_node_entry`: releases every reference this entry's owner registered.
    pub(crate) fn release_all(&self) {
        self.inner.refs.store(0, Ordering::Relaxed);
    }

    /// A second *handle* on this entry, for observation — it does **not** register a reference
    /// (the count tracks references, not handles), so a handle outliving the owning table sees
    /// the count the table released.
    pub fn handle(&self) -> NodeEntry {
        NodeEntry {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl core::fmt::Debug for NodeEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NodeEntry")
            .field("node", &self.inner.node)
            .field("creation", &self.inner.creation)
            .field("refcount", &self.refcount())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::prelude::*;

    #[test]
    fn a_new_entry_holds_one_reference_and_add_reference_counts() {
        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));

        let entry = NodeEntry::new(node, 7);
        assert_eq!(entry.refcount(), 1);
        assert_eq!(entry.node().raw(), node.raw());
        assert_eq!(entry.creation(), 7);

        entry.add_reference();
        assert_eq!(entry.refcount(), 2);

        // A bare handle shares the count without registering a reference.
        let held = entry.handle();
        assert_eq!(held.refcount(), 2);
        entry.release_all();
        assert_eq!(held.refcount(), 0);
    }

    #[test]
    fn identity_is_node_plus_creation() {
        let mut atoms = AtomTable::new();
        let a = make_atom(atoms.intern("a"));
        let b = make_atom(atoms.intern("b"));

        let entry = NodeEntry::new(a, 1);
        assert!(entry.identity_eq(a, 1));
        assert!(!entry.identity_eq(a, 2), "a different creation is a different node");
        assert!(!entry.identity_eq(b, 1), "a different node atom is a different node");

        let same = NodeEntry::new(a, 1);
        let other = NodeEntry::new(a, 2);
        assert!(entry.same_node(&same));
        assert!(!entry.same_node(&other));
    }
}
