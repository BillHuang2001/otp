//! Off-heap refcounted large objects (MSO): [`RefCount`], [`Mso`], [`MsoHandle`] and
//! the per-process [`OffHeap`] list.
//!
//! The payload lives outside every process heap; a collection copies only the small
//! on-heap handle's scalar words and leaves every off-heap pointer verbatim, so
//! refcount changes are this module's responsibility (`inc` on a new handle,
//! destructor when the sweep drops a dead one). All of this module is safe Rust.

use core::any::Any;
use core::ptr::NonNull;
use core::sync::atomic::AtomicUsize;

/// An atomic reference count with `erts_refc_*` semantics.
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
        unimplemented!("scaffold")
    }

    /// Atomically increments the count, returning the new value.
    pub fn inc(&self) -> usize {
        unimplemented!("scaffold")
    }

    /// Atomically decrements the count, returning the new value.
    pub fn dec(&self) -> usize {
        unimplemented!("scaffold")
    }

    /// Atomically decrements the count and reports whether it fell below `min`.
    pub fn dec_and_test(&self, min: usize) -> bool {
        let _ = min;
        unimplemented!("scaffold")
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
        let _ = (payload, apparent_size);
        unimplemented!("scaffold")
    }

    /// Downcasts the payload to `T`, when it holds a `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        unimplemented!("scaffold")
    }

    /// The object's apparent (Erlang-visible) size in bytes.
    pub fn apparent_size(&self) -> usize {
        unimplemented!("scaffold")
    }

    /// The current reference count.
    pub fn refcount(&self) -> usize {
        unimplemented!("scaffold")
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
    /// Downcasts the referenced object's payload to `T`, when it holds a `T`.
    pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
        unimplemented!("scaffold")
    }

    /// The referenced object's apparent size in bytes.
    pub fn apparent_size(&self) -> usize {
        unimplemented!("scaffold")
    }

    /// The referenced object's current reference count.
    pub fn refcount(&self) -> usize {
        unimplemented!("scaffold")
    }
}

impl Clone for MsoHandle {
    fn clone(&self) -> Self {
        unimplemented!("scaffold")
    }
}

impl Drop for MsoHandle {
    fn drop(&mut self) {
        unimplemented!("scaffold")
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
        let _ = handle;
        unimplemented!("scaffold")
    }

    /// The number of off-heap objects held.
    pub fn len(&self) -> usize {
        unimplemented!("scaffold")
    }

    /// Whether the list holds no off-heap objects.
    pub fn is_empty(&self) -> bool {
        unimplemented!("scaffold")
    }

    /// The bookkeeping overhead of the list, in bytes.
    pub fn overhead_bytes(&self) -> usize {
        unimplemented!("scaffold")
    }

    /// Retains the handles for which `is_live` returns `true`, releasing the rest, and
    /// returns how many were released.
    pub fn sweep<F: FnMut(&MsoHandle) -> bool>(&mut self, is_live: F) -> usize {
        let _ = is_live;
        unimplemented!("scaffold")
    }
}
