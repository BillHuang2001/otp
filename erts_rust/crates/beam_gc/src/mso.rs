//! Off-heap refcounted large objects (MSO): [`RefCount`], [`Mso`], [`MsoHandle`] and
//! the per-process [`OffHeap`] list.
//!
//! The payload lives outside every process heap; a collection copies only the small
//! on-heap handle's scalar words and leaves every off-heap pointer verbatim, so
//! refcount changes are this module's responsibility (`inc` on a new handle,
//! destructor when the sweep drops a dead one).
//!
//! This module is safe Rust apart from two narrow `unsafe` blocks — the `NonNull` deref in
//! [`MsoHandle`] and the `Box::from_raw` free in its `Drop` — each of which carries a
//! `// SAFETY:` rationale (the handle owns a live, boxed [`Mso`] for its whole lifetime).

use core::any::Any;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicUsize, Ordering};

/// An atomic reference count with `erts_refc_*` semantics.
///
/// Mutations use `AcqRel` (an `Acquire` of the object's state plus a `Release` of the
/// previous owner's work) and reads use `Acquire`: taking a reference must happen-before
/// the object's payload is observed, and the final `dec_and_test(0)` must observe every
/// write any earlier owner made before its own reference disappeared.
#[repr(transparent)]
pub struct RefCount {
    count: AtomicUsize,
}

impl RefCount {
    /// Creates a reference count with the initial value `initial`.
    pub const fn new(initial: usize) -> RefCount {
        RefCount {
            count: AtomicUsize::new(initial),
        }
    }

    /// The current count.
    pub fn count(&self) -> usize {
        self.count.load(Ordering::Acquire)
    }

    /// Atomically increments the count, returning the new value.
    pub fn inc(&self) -> usize {
        self.count.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Atomically decrements the count, returning the new value.
    pub fn dec(&self) -> usize {
        self.count.fetch_sub(1, Ordering::AcqRel) - 1
    }

    /// Atomically decrements the count and reports whether it fell below `min`.
    ///
    /// This is `erts_refc_dectest`: the caller that sees `true` is the one that must
    /// destroy the object (with `min == 0`, exactly the last reference).
    pub fn dec_and_test(&self, min: usize) -> bool {
        let new = self.count.fetch_sub(1, Ordering::AcqRel) - 1;
        new <= min
    }
}

/// An off-heap, immutable, refcounted shared object (the MSO itself).
pub struct Mso {
    refc: RefCount,
    apparent_size: usize,
    payload: Box<dyn Any + Send>,
}

impl Mso {
    /// Allocates an [`Mso`] holding `payload` of `apparent_size` bytes and returns a
    /// refcounted [`MsoHandle`] to it.
    pub fn new<T: Any + Send>(payload: T, apparent_size: usize) -> MsoHandle {
        let mso = Box::new(Mso {
            refc: RefCount::new(1),
            apparent_size,
            payload: Box::new(payload),
        });
        let ptr = NonNull::new(Box::into_raw(mso)).expect("Box::into_raw never yields null");
        MsoHandle { ptr }
    }

    /// Downcasts the payload to `T`, when it holds a `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.payload.downcast_ref::<T>()
    }

    /// The object's apparent (Erlang-visible) size in bytes.
    pub fn apparent_size(&self) -> usize {
        self.apparent_size
    }

    /// The current reference count.
    pub fn refcount(&self) -> usize {
        self.refc.count()
    }
}

/// A clonable owning handle to an [`Mso`] (the on-heap binary-reference analogue).
///
/// `Clone` bumps the refcount; the last `Drop` runs the payload's destructor and frees
/// the off-heap object.
pub struct MsoHandle {
    ptr: NonNull<Mso>,
}

impl MsoHandle {
    /// Borrows the referenced [`Mso`].
    ///
    /// `ptr` is private and only ever set by [`Mso::new`], which hands out the *sole* owning
    /// handle to a freshly boxed `Mso`; every other handle is a [`Clone`] that bumped the
    /// refcount first. The boxed `Mso` is therefore alive and initialized for as long as any
    /// handle to it exists, i.e. for all of `self`'s lifetime.
    #[inline]
    fn mso(&self) -> &Mso {
        // SAFETY: see above — `self` owns a live reference, so `ptr` is non-null, aligned,
        // dereferenceable and points to an initialized `Mso`.
        unsafe { self.ptr.as_ref() }
    }

    /// Downcasts the referenced object's payload to `T`, when it holds a `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        self.mso().downcast_ref::<T>()
    }

    /// The referenced object's apparent size in bytes.
    pub fn apparent_size(&self) -> usize {
        self.mso().apparent_size()
    }

    /// The referenced object's current reference count.
    pub fn refcount(&self) -> usize {
        self.mso().refcount()
    }
}

impl Clone for MsoHandle {
    fn clone(&self) -> Self {
        // The new handle shares the same object, so it must own a reference to it.
        self.mso().refc.inc();
        MsoHandle { ptr: self.ptr }
    }
}

impl Drop for MsoHandle {
    fn drop(&mut self) {
        if self.mso().refc.dec_and_test(0) {
            // SAFETY: `dec_and_test(0)` returned `true`, so this was the last reference: no
            // other handle can reach the object, and `ptr` came from `Box::into_raw` in
            // `Mso::new`, so reclaiming it with `Box::from_raw` frees exactly that allocation
            // and runs the payload's `Drop` exactly once.
            unsafe { drop(Box::from_raw(self.ptr.as_ptr())) };
        }
    }
}

/// The per-process list of off-heap large objects (`MSO(p)`).
pub struct OffHeap {
    entries: Vec<MsoHandle>,
}

impl OffHeap {
    /// An empty off-heap list.
    pub fn new() -> OffHeap {
        OffHeap {
            entries: Vec::new(),
        }
    }

    /// Adds `handle` to the list.
    pub fn push(&mut self, handle: MsoHandle) {
        self.entries.push(handle);
    }

    /// The number of off-heap objects held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the list holds no off-heap objects.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The bookkeeping overhead of the list, in bytes.
    pub fn overhead_bytes(&self) -> usize {
        self.entries.iter().map(MsoHandle::apparent_size).sum()
    }

    /// Retains the handles for which `is_live` returns `true`, releasing the rest, and
    /// returns how many were released.
    pub fn sweep<F: FnMut(&MsoHandle) -> bool>(&mut self, mut is_live: F) -> usize {
        let before = self.entries.len();
        // Dropping the rejected handles runs their destructors (and frees the objects whose
        // last reference this list held).
        self.entries.retain(|handle| is_live(handle));
        before - self.entries.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A payload that counts how many times it has been dropped.
    struct DropCounter {
        drops: Arc<AtomicUsize>,
        value: u32,
    }

    impl Drop for DropCounter {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counter(drops: &Arc<AtomicUsize>, value: u32) -> DropCounter {
        DropCounter {
            drops: Arc::clone(drops),
            value,
        }
    }

    #[test]
    fn refcount_inc_dec_and_test() {
        let rc = RefCount::new(1);
        assert_eq!(rc.count(), 1);
        assert_eq!(rc.inc(), 2);
        assert_eq!(rc.inc(), 3);
        assert_eq!(rc.count(), 3);
        assert_eq!(rc.dec(), 2);
        assert_eq!(rc.count(), 2);
        // Falls exactly to `min` -> reports true.
        assert!(rc.dec_and_test(1));
        assert_eq!(rc.count(), 1);
        assert!(rc.dec_and_test(0));
        assert_eq!(rc.count(), 0);
        // Note: the count must never be decremented past 0 (as in `erts_refc_*`); another
        // `dec_and_test` here would underflow.
        // Staying above `min` does not report release.
        let rc = RefCount::new(4);
        assert!(!rc.dec_and_test(2)); // 3 > 2
        assert!(rc.dec_and_test(2)); // 2 <= 2
        assert_eq!(rc.count(), 2);
    }

    #[test]
    fn clone_bumps_the_refcount_and_the_last_drop_frees_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let handle = Mso::new(counter(&drops, 7), 128);
        assert_eq!(handle.refcount(), 1);
        assert_eq!(handle.apparent_size(), 128);
        assert_eq!(
            handle.downcast_ref::<DropCounter>().map(|c| c.value),
            Some(7)
        );

        let clone = handle.clone();
        assert_eq!(clone.refcount(), 2);
        assert_eq!(handle.refcount(), 2);
        assert_eq!(clone.apparent_size(), 128);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        drop(clone);
        assert_eq!(handle.refcount(), 1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            0,
            "payload must survive the first drop"
        );
        assert_eq!(
            handle.downcast_ref::<DropCounter>().map(|c| c.value),
            Some(7)
        );

        drop(handle);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "payload must be dropped exactly once"
        );
    }

    #[test]
    fn downcast_ref_checks_the_concrete_type() {
        let handle = Mso::new(1234u32, 4);
        assert_eq!(handle.downcast_ref::<u32>(), Some(&1234));
        assert_eq!(handle.downcast_ref::<u64>(), None);
        assert_eq!(handle.downcast_ref::<String>(), None);
        // A clone reaches the very same payload.
        let clone = handle.clone();
        assert_eq!(clone.downcast_ref::<u32>(), Some(&1234));
        assert_eq!(clone.refcount(), 2);
    }

    #[test]
    fn off_heap_tracks_size_and_sweeps_only_the_dead() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut off_heap = OffHeap::new();
        assert!(off_heap.is_empty());
        assert_eq!(off_heap.len(), 0);
        assert_eq!(off_heap.overhead_bytes(), 0);

        for (value, size) in [(0u32, 10usize), (1, 20), (2, 30)] {
            off_heap.push(Mso::new(counter(&drops, value), size));
        }
        assert!(!off_heap.is_empty());
        assert_eq!(off_heap.len(), 3);
        assert_eq!(off_heap.overhead_bytes(), 60);

        // Keep only the 10-byte object.
        let released = off_heap.sweep(|handle| handle.apparent_size() == 10);
        assert_eq!(released, 2);
        assert_eq!(off_heap.len(), 1);
        assert_eq!(off_heap.overhead_bytes(), 10);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert_eq!(
            off_heap.entries[0]
                .downcast_ref::<DropCounter>()
                .map(|c| c.value),
            Some(0)
        );

        // Nothing is live any more: the last entry goes too.
        assert_eq!(off_heap.sweep(|_| false), 1);
        assert!(off_heap.is_empty());
        assert_eq!(off_heap.overhead_bytes(), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 3);

        // A sweep that keeps everything releases nothing.
        off_heap.push(Mso::new(7u32, 4));
        assert_eq!(off_heap.sweep(|_| true), 0);
        assert_eq!(off_heap.len(), 1);
    }

    #[test]
    fn off_heap_releases_an_object_shared_by_two_entries_only_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let handle = Mso::new(counter(&drops, 9), 16);
        let mut off_heap = OffHeap::new();
        off_heap.push(handle.clone());
        assert_eq!(handle.refcount(), 2);

        // The list owns a reference of its own: dropping the local handle leaves the object
        // (and its payload) alive.
        drop(handle);
        assert_eq!(off_heap.entries[0].refcount(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // Two entries for one object: releasing both runs the destructor exactly once.
        let shared = Mso::new(counter(&drops, 11), 16);
        let mut two = OffHeap::new();
        two.push(shared.clone());
        two.push(shared);
        assert_eq!(two.entries[0].refcount(), 2);
        assert_eq!(two.sweep(|_| false), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1); // only the 9-byte object's payload

        assert_eq!(off_heap.sweep(|_| false), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}
