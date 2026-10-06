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
//! # Slot stability (the reason this table never sweeps)
//!
//! Slots must be **stable for the table's lifetime**: the copier writes slot indices into
//! on-heap containers, so a shifted slot would silently corrupt every reference to a later
//! object. [`OffHeapTable`] therefore uses **only** the append-only insert/read path
//! ([`beam_gc::OffHeap::insert`]/[`get`](OffHeapTable::get)/[`iter`](OffHeapTable::iter)),
//! which never reuses or reorders a slot. It **never** calls [`beam_gc::OffHeap::sweep`] —
//! that operation compacts the list and shifts the surviving slots down. Off-heap
//! reclamation is the collector's concern (`beam_gc`), not this view's.

pub use beam_gc::MsoHandle;

/// A slot-indexed table of off-heap (MSO) handles.
///
/// The slot of a handle is the value the on-heap off-heap-reference container stores in its
/// `val` word; `push` returns it. Slots are stable for the table's lifetime (they are never
/// reused or compacted) — see the module docs: this table deliberately never sweeps.
///
/// Storage is delegated to [`beam_gc::OffHeap`] (the workspace's single owner of the
/// off-heap list); this type only adds the MSO-typed access the copy boundary needs.
pub struct OffHeapTable {
    inner: beam_gc::OffHeap,
}

impl Default for OffHeapTable {
    /// An empty off-heap table.
    ///
    /// Implemented by hand rather than derived: [`beam_gc::OffHeap`] is not `Default`.
    fn default() -> OffHeapTable {
        OffHeapTable::new()
    }
}

impl OffHeapTable {
    /// An empty off-heap table.
    pub fn new() -> OffHeapTable {
        OffHeapTable {
            inner: beam_gc::OffHeap::new(),
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

    /// The apparent (Erlang-visible) size of the referenced objects, in bytes.
    ///
    /// Delegates to [`beam_gc::OffHeap::overhead_bytes`]; since this view holds only MSO
    /// entries, it is the sum of [`MsoHandle::apparent_size`] over the slots.
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
}
