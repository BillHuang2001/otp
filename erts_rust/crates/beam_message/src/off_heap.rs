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

//! The off-heap (MSO) reference table: a **slot-indexed** view of [`beam_gc::OffHeap`].
//!
//! The off-heap object model, its atomic refcount, its owning handle and the per-process
//! list that owns them are entirely `beam_gc::mso`'s — this crate never re-implements (nor
//! re-stores) MSO. What the copy boundary additionally needs is a *readable index*: the
//! on-heap off-heap reference stores a slot number (`val`) into the owning process's table,
//! so the copier can resolve a source slot, clone the handle (refcount++) and register it
//! under a fresh destination slot. [`OffHeapTable`] is therefore a thin, **MSO-typed
//! façade** over the shared [`beam_gc::OffHeap`] storage — the slot↔object association has
//! exactly one home.
//!
//! # Why this table never sweeps
//!
//! Slots must be **stable for the table's lifetime**: the copier writes slot indices into
//! on-heap containers, so a shifted slot would silently corrupt every reference to a later
//! object. [`OffHeapTable`] therefore uses **only** the append-only insert/read path
//! ([`beam_gc::OffHeap::insert`]/[`get`](OffHeapTable::get)/[`iter`](OffHeapTable::iter)),
//! which never reuses or reorders a slot. It **never** calls [`beam_gc::OffHeap::sweep`] —
//! that operation releases entries *in place* (a released slot becomes empty; it is neither
//! reused nor moved, so surviving slots keep their indices), and reclamation is the
//! collector's concern (`beam_gc`), not this read-only view's.
//!
//! # Binaries and nodes
//!
//! Two further off-heap categories ride along with a copied message and are tracked
//! **beside** the MSO list, so the MSO-typed accessors above keep their exact meaning:
//!
//! * **Refc binaries** ([`RefcBinary`], the payload of an on-heap `BinRef`/`ErlSubBits`
//!   pair) — [`push_binary`](OffHeapTable::push_binary)/[`binary_at`](OffHeapTable::binary_at)/
//!   [`binary_by_data_ptr`](OffHeapTable::binary_by_data_ptr)/[`drain_binaries`](OffHeapTable::drain_binaries).
//!   The copy engine **shares** the payload (refcount++) rather than deep-copying it
//!   (`copy.c:836` `erts_refc_inc(&from_br->val->intern.refc, 2)`).
//! * **Nodes** ([`NodeEntry`], the refcounted `(node, creation)` identity of an external
//!   pid/port/reference) — [`push_node`](OffHeapTable::push_node)/[`nodes`](OffHeapTable::nodes)/
//!   [`drain_nodes`](OffHeapTable::drain_nodes), mirroring `erts_ref_node_entry`.

use beam_alloc::RefcBinary;

use crate::node::NodeEntry;

pub use beam_gc::MsoHandle;

/// A slot-indexed table of off-heap objects: MSO handles, refc binaries and node
/// references.
///
/// The slot of an **MSO** handle is the value the on-heap off-heap-reference container
/// stores in its `val` word; `push` returns it. Slots are stable for the table's lifetime
/// (they are never reused or reordered) — see the module docs: this table deliberately
/// never sweeps, because off-heap reclamation is the collector's job, not a read view's.
///
/// MSO storage is delegated to [`beam_gc::OffHeap`] (the workspace's single owner of the
/// off-heap list); this type adds the MSO-typed access the copy boundary needs, plus the
/// refc-binary and node lists that travel with a message.
pub struct OffHeapTable {
    inner: beam_gc::OffHeap,
    binaries: Vec<RefcBinary>,
    nodes: Vec<NodeEntry>,
}

impl Default for OffHeapTable {
    /// An empty off-heap table.
    ///
    /// Implemented by hand rather than derived: [`beam_gc::OffHeap`] is not `Default`.
    fn default() -> OffHeapTable {
        OffHeapTable::new()
    }
}

impl Drop for OffHeapTable {
    /// Releases every node reference this table registered (`erts_deref_node_entry`).
    ///
    /// The MSO and refc-binary lists need no release step: their handles decrement their own
    /// refcounts as they drop (`beam_gc`/`beam_alloc`).
    fn drop(&mut self) {
        for entry in &self.nodes {
            entry.release_all();
        }
    }
}

impl OffHeapTable {
    /// An empty off-heap table.
    pub fn new() -> OffHeapTable {
        OffHeapTable {
            inner: beam_gc::OffHeap::new(),
            binaries: Vec::new(),
            nodes: Vec::new(),
        }
    }

    /// Adds `handle` and returns its slot.
    pub fn push(&mut self, handle: MsoHandle) -> usize {
        self.inner.insert(handle)
    }

    /// The handle at `slot`, or `None` when the slot is out of range (or holds a non-MSO
    /// entry, which this MSO-typed view never stores).
    pub fn get(&self, slot: usize) -> Option<&MsoHandle> {
        self.inner.get(slot).and_then(beam_gc::OffHeapEntry::as_mso)
    }

    /// Clones the handle at `slot` (**bumping its refcount**), or `None` when out of range.
    pub fn clone_slot(&self, slot: usize) -> Option<MsoHandle> {
        self.inner
            .get(slot)
            .and_then(beam_gc::OffHeapEntry::as_mso)
            .cloned()
    }

    /// The number of off-heap objects held.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the table holds no off-heap objects.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Iterates the held handles in slot order.
    pub fn iter(&self) -> impl Iterator<Item = &MsoHandle> {
        self.inner
            .iter()
            .filter_map(beam_gc::OffHeapEntry::as_mso)
    }

    /// The apparent (Erlang-visible) size of the referenced **MSO** objects, in bytes.
    ///
    /// Delegates to [`beam_gc::OffHeap::overhead_bytes`]; since the MSO list holds only MSO
    /// entries, it is the sum of [`MsoHandle::apparent_size`] over the slots. The refc
    /// binaries tracked alongside are *not* counted here — see
    /// [`binaries`](OffHeapTable::binaries).
    pub fn overhead_bytes(&self) -> usize {
        self.inner.overhead_bytes()
    }

    /// Consumes the table and returns its handles in slot order (the `Message`'s off-heap list).
    ///
    /// `beam_gc::OffHeap` has no move-out API, so each handle is **cloned** (refcount++) and
    /// then the consumed list drops (refcount--) — the net refcount of every returned handle
    /// is therefore unchanged, matching the previous move-based behaviour.
    pub fn into_handles(self) -> Vec<MsoHandle> {
        self.inner
            .iter()
            .filter_map(beam_gc::OffHeapEntry::as_mso)
            .cloned()
            .collect()
    }

    /// Removes and returns every held handle, leaving the table empty.
    ///
    /// `send` uses this to hand the copied message its off-heap list: the destination table built
    /// up by `copy_struct` *is* the message's off-heap list, and is expected to start empty for
    /// each send (mirroring the C per-message `off_heap`).
    ///
    /// As in [`into_handles`](OffHeapTable::into_handles), each handle is cloned (refcount++)
    /// before the replaced-out list drops (refcount--), so the returned handles' net refcount is
    /// unchanged. The replacement list is fresh, so slot numbering restarts at `0`.
    pub fn drain_handles(&mut self) -> Vec<MsoHandle> {
        let inner = core::mem::replace(&mut self.inner, beam_gc::OffHeap::new());
        inner
            .iter()
            .filter_map(beam_gc::OffHeapEntry::as_mso)
            .cloned()
            .collect()
    }

    /// The raw off-heap entry at `slot`, or `None` when the slot is out of range.
    ///
    /// The untyped companion of [`get`](OffHeapTable::get): it exposes the underlying
    /// [`beam_gc::OffHeapEntry`] whether it is an MSO or a binary.
    pub fn get_entry(&self, slot: usize) -> Option<&beam_gc::OffHeapEntry> {
        self.inner.get(slot)
    }

    // -- refc binaries ------------------------------------------------------

    /// Adds the refc binary `binary` and returns its slot.
    ///
    /// The table **owns** the handle; the caller normally passes a clone of the source
    /// process's handle (refcount++), so the shared payload outlives neither side.
    pub fn push_binary(&mut self, binary: RefcBinary) -> usize {
        self.binaries.push(binary);
        self.binaries.len() - 1
    }

    /// The refc binary at `slot`, cloned (**bumping its refcount**), or `None` when the
    /// slot is out of range.
    pub fn binary_at(&self, slot: usize) -> Option<RefcBinary> {
        self.binaries.get(slot).cloned()
    }

    /// The refc binary whose payload starts at `data_ptr`, cloned (**bumping its
    /// refcount**), or `None` when no held binary has that data pointer.
    ///
    /// This resolves an on-heap `BinRef.val` (the frozen contract: the *first payload
    /// byte*, [`RefcBinary::data_ptr`]) back to its owning handle — the lookup the copy
    /// engine needs to share, rather than deep-copy, the payload.
    pub fn binary_by_data_ptr(&self, data_ptr: *const u8) -> Option<RefcBinary> {
        self.binaries
            .iter()
            .find(|binary| binary.data_ptr().cast_const() == data_ptr)
            .cloned()
    }

    /// The held refc binaries, in slot order.
    pub fn binaries(&self) -> &[RefcBinary] {
        &self.binaries
    }

    /// Removes and returns every held refc binary, leaving that list empty.
    ///
    /// `send` uses this to hand the copied message its binary list (see the crate
    /// `CONTEXT.md`, Design Decisions): the destination table built up by `copy_struct`
    /// *is* the message's off-heap ownership, and is expected to start empty for each send.
    pub fn drain_binaries(&mut self) -> Vec<RefcBinary> {
        core::mem::take(&mut self.binaries)
    }

    // -- external-thing nodes ----------------------------------------------

    /// The node entry for `(node, creation)`, or `None` when this table tracks no such node.
    pub fn node_by_identity(&self, node: beam_term::prelude::Eterm, creation: u32) -> Option<&NodeEntry> {
        self.nodes
            .iter()
            .find(|entry| entry.identity_eq(node, creation))
    }

    /// Registers one reference to a node in this table's node list and returns its slot.
    ///
    /// One entry per **distinct** node (`erts_ref_node_entry`, `copy.c:882`): an already
    /// present `(node, creation)` keeps its single list entry and has this new reference
    /// counted onto it, so a term holding two references to the same node leaves one entry
    /// whose [`refcount`](NodeEntry::refcount) is `2`. A new node stores `entry`; the count
    /// it carries is the first reference.
    pub fn push_node(&mut self, entry: NodeEntry) -> usize {
        if let Some(slot) = self.nodes.iter().position(|held| held.same_node(&entry)) {
            self.nodes[slot].add_reference();
            return slot;
        }
        self.nodes.push(entry);
        self.nodes.len() - 1
    }

    /// The node entries this table tracks, in slot order.
    pub fn nodes(&self) -> &[NodeEntry] {
        &self.nodes
    }

    /// Removes and returns every node entry, leaving the node list empty.
    ///
    /// `send` uses this to hand the copied message its node references, mirroring the
    /// off-heap list ([`drain_handles`](OffHeapTable::drain_handles)).
    pub fn drain_nodes(&mut self) -> Vec<NodeEntry> {
        core::mem::take(&mut self.nodes)
    }

    // -- roll-back for an optimistic copy --------------------------------

    /// Whether the table is empty in **every** category: no MSO handle, no refc binary and no
    /// node reference.
    ///
    /// [`is_empty`](OffHeapTable::is_empty) covers only the MSO list (that type's slot space), so a
    /// caller that needs "this table holds nothing at all" must ask this instead. The copy engine's
    /// clean-destination gate does: a destination this returns `true` for is one the optimistic
    /// single-pass attempt can roll back wholesale.
    pub(crate) fn is_fully_empty(&self) -> bool {
        self.inner.is_empty() && self.binaries.is_empty() && self.nodes.is_empty()
    }

    /// Releases every reference this table registered and empties all three lists.
    ///
    /// The roll-back half of the copy engine's optimistic single-pass attempt (`copy::copy_struct`)
    /// on a destination that was **clean** when the attempt began: the attempt refcounts the
    /// off-heap objects it copies (`clone_slot`, `push_binary`, `push_node`), so abandoning it must
    /// give those references back or the source objects would leak a count per failed copy.
    ///
    /// The MSO handles and refc binaries release through their own `Drop` when the drained lists
    /// drop (`drain_handles` clones-then-drops the shared list, so the net effect of draining plus
    /// dropping the result is exactly the one reference each push added). A [`NodeEntry`] must be
    /// released explicitly: its count is `release_all`'s, not `Drop`'s, because the count belongs
    /// to the registering table rather than to the handle.
    ///
    /// Not `pub`: the roll-back is only sound on a table the caller knows started empty.
    pub(crate) fn rollback(&mut self) {
        drop(self.drain_handles());
        drop(self.drain_binaries());
        for entry in self.drain_nodes() {
            entry.release_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_gc::Mso;

    #[test]
    fn push_get_len_and_bytes_track_slots() {
        let mut table = OffHeapTable::new();
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.get(0).is_none());
        assert!(table.clone_slot(0).is_none());
        assert_eq!(table.overhead_bytes(), 0);

        assert_eq!(table.push(Mso::new(11u32, 8)), 0);
        assert_eq!(table.push(Mso::new(22u32, 16)), 1);
        assert_eq!(table.len(), 2);
        assert!(!table.is_empty());
        assert!(table.get(2).is_none());
        assert_eq!(table.get(0).and_then(|h| h.downcast_ref::<u32>()), Some(&11));
        assert_eq!(table.get(1).and_then(|h| h.downcast_ref::<u32>()), Some(&22));
        assert_eq!(table.overhead_bytes(), 24);
    }

    #[test]
    fn clone_slot_bumps_the_refcount_and_into_handles_drains_all_slots() {
        let mut table = OffHeapTable::new();
        let slot = table.push(Mso::new(7u32, 4));
        assert_eq!(table.get(slot).map(MsoHandle::refcount), Some(1));
        assert!(table.clone_slot(9).is_none());

        let clone = table.clone_slot(slot).expect("slot 0 exists");
        assert_eq!(clone.refcount(), 2);
        assert_eq!(clone.downcast_ref::<u32>(), Some(&7));

        let handles = table.into_handles();
        assert_eq!(handles.len(), 1);
        assert_eq!(handles[0].downcast_ref::<u32>(), Some(&7));
        // The clone is the only extra reference; dropping it releases the shared object.
        drop(clone);
        assert_eq!(handles[0].refcount(), 1);
    }

    #[test]
    fn drain_handles_returns_every_slot_and_empties_the_table() {
        let mut table = OffHeapTable::new();
        table.push(Mso::new(1u32, 4));
        table.push(Mso::new(2u32, 4));

        let drained = table.drain_handles();
        assert_eq!(drained.len(), 2);
        assert_eq!(drained[0].downcast_ref::<u32>(), Some(&1));
        assert_eq!(drained[1].downcast_ref::<u32>(), Some(&2));

        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
        assert!(table.get(0).is_none());
        assert!(table.drain_handles().is_empty());
    }

    #[test]
    fn drain_handles_leaves_the_table_reusable_with_slots_restarting_at_zero() {
        let mut table = OffHeapTable::new();
        table.push(Mso::new(1u32, 4));
        table.push(Mso::new(2u32, 4));
        let drained = table.drain_handles();
        assert_eq!(drained.len(), 2);

        // Nothing held: the emptied table accepts a new object and numbers it from 0 again,
        // exactly as the previous `Vec`-backed implementation did.
        assert!(table.is_empty());
        assert_eq!(table.iter().count(), 0);

        assert_eq!(table.push(Mso::new(9u32, 8)), 0);
        assert_eq!(table.len(), 1);
        assert_eq!(table.get(0).and_then(|h| h.downcast_ref::<u32>()), Some(&9));
        assert!(table.get(1).is_none());
        assert_eq!(table.overhead_bytes(), 8);
    }

    #[test]
    fn binaries_are_tracked_beside_the_mso_list() {
        let mut table = OffHeapTable::new();
        let binary = RefcBinary::new(&[1, 2, 3, 4]).expect("the payload allocates");
        let data_ptr = binary.data_ptr();

        assert_eq!(table.push_binary(binary.clone()), 0);
        assert_eq!(table.binaries().len(), 1);
        // The MSO-typed views (and the MSO slot space) never see a binary.
        assert!(table.is_empty());
        assert!(table.get_entry(0).is_none());
        assert_eq!(table.binary_at(0).map(|b| b.as_bytes().to_vec()), Some(vec![1, 2, 3, 4]));
        assert!(table.binary_at(1).is_none());

        let resolved = table.binary_by_data_ptr(data_ptr).expect("the payload is held");
        assert_eq!(resolved.as_bytes(), &[1, 2, 3, 4]);
        assert!(table.binary_by_data_ptr(core::ptr::null()).is_none());

        let drained = table.drain_binaries();
        assert_eq!(drained.len(), 1);
        assert!(table.binaries().is_empty());
        drop(drained);
    }

    #[test]
    fn push_node_dedups_by_identity_and_counts_references() {
        use crate::node::NodeEntry;
        use beam_term::prelude::*;

        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));

        let mut table = OffHeapTable::new();
        assert_eq!(table.push_node(NodeEntry::new(node, 3)), 0);
        assert_eq!(table.push_node(NodeEntry::new(node, 3)), 0, "one entry per distinct node");
        assert_eq!(table.nodes().len(), 1);
        assert_eq!(table.nodes()[0].refcount(), 2, "one reference per push");

        // A different creation is a different node.
        assert_eq!(table.push_node(NodeEntry::new(node, 4)), 1);
        assert_eq!(table.nodes().len(), 2);
        assert!(table.node_by_identity(node, 3).is_some());
        assert!(table.node_by_identity(node, 9).is_none());

        let drained = table.drain_nodes();
        assert_eq!(drained.len(), 2);
        assert!(table.nodes().is_empty());
    }

    #[test]
    fn dropping_the_table_releases_its_node_references() {
        use crate::node::NodeEntry;
        use beam_term::prelude::*;

        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));

        let mut table = OffHeapTable::new();
        table.push_node(NodeEntry::new(node, 1));
        table.push_node(NodeEntry::new(node, 1));
        let held = table.nodes()[0].handle();
        assert_eq!(held.refcount(), 2);

        drop(table);
        assert_eq!(held.refcount(), 0, "the table released both references");
    }
}
