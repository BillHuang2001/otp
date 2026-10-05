//! The off-heap (MSO) reference table: a **slot-indexed** table of [`MsoHandle`]s.
//!
//! The off-heap object model, its atomic refcount and its owning handle are entirely
//! `beam_gc::mso`'s and are only re-exported here — this crate never re-implements MSO. What
//! the copy boundary additionally needs is a *readable index*: the on-heap off-heap reference
//! stores a slot number (`val`) into the owning process's table, so the copier can resolve a
//! source slot, clone the handle (refcount++) and register it under a fresh destination slot.
//! `beam_gc::OffHeap` is push-only, so this table adds only that index association.

pub use beam_gc::MsoHandle;

/// A slot-indexed table of off-heap (MSO) handles.
///
/// The slot of a handle is the value the on-heap off-heap-reference container stores in its
/// `val` word; `push` returns it. Slots are stable for the table's lifetime (they are never
/// reused or compacted).
#[derive(Default)]
pub struct OffHeapTable {
    entries: Vec<MsoHandle>,
}

impl OffHeapTable {
    /// An empty off-heap table.
    pub fn new() -> OffHeapTable {
        OffHeapTable {
            entries: Vec::new(),
        }
    }

    /// Adds `handle` and returns its slot.
    pub fn push(&mut self, handle: MsoHandle) -> usize {
        self.entries.push(handle);
        self.entries.len() - 1
    }

    /// The handle at `slot`, or `None` when the slot is out of range.
    pub fn get(&self, slot: usize) -> Option<&MsoHandle> {
        self.entries.get(slot)
    }

    /// Clones the handle at `slot` (**bumping its refcount**), or `None` when out of range.
    pub fn clone_slot(&self, slot: usize) -> Option<MsoHandle> {
        self.entries.get(slot).cloned()
    }

    /// The number of off-heap objects held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the table holds no off-heap objects.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Iterates the held handles in slot order.
    pub fn iter(&self) -> impl Iterator<Item = &MsoHandle> {
        self.entries.iter()
    }

    /// The apparent (Erlang-visible) size of the referenced objects, in bytes.
    ///
    /// Mirrors `beam_gc::OffHeap::overhead_bytes`.
    pub fn overhead_bytes(&self) -> usize {
        self.entries.iter().map(MsoHandle::apparent_size).sum()
    }

    /// Consumes the table and returns its handles in slot order (the `Message`'s off-heap list).
    pub fn into_handles(self) -> Vec<MsoHandle> {
        self.entries
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
}
