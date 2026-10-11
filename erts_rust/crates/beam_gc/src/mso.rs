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

//! Off-heap refcounted large objects (MSO) and off-heap refcounted binaries:
//! [`Mso`], [`MsoHandle`], [`OffHeapEntry`] and the per-process [`OffHeap`] list.
//!
//! The payload lives outside every process heap; a collection copies only the small
//! on-heap handle's scalar words and leaves every off-heap pointer verbatim, so
//! refcount changes are this module's responsibility (`inc` on a new handle,
//! destructor when the sweep drops a dead one).
//!
//! The atomic reference count is the workspace-wide primitive
//! [`beam_alloc::RefCount`] — re-exported here as [`RefCount`]; this crate defines no
//! count of its own (the crate-layer contract forbids duplicating a type across
//! crates). `beam_alloc::RefcBinary` — an off-heap refcounted byte buffer — is
//! refcounted by that same primitive, and the per-process [`OffHeap`] list holds both
//! kinds of off-heap object, so a **refc binary** whose on-heap `BinRef` handle becomes
//! garbage is reclaimed by the very same [`OffHeap::sweep`].
//!
//! The list exposes slot-indexed read access alongside the push-only writes: while
//! [`OffHeap::push`] is fire-and-forget, [`OffHeap::insert`] returns the slot an entry was
//! stored under, and [`OffHeap::get`]/[`OffHeap::iter`] read the entries back (by slot, or
//! in slot order), so an on-heap reference that stores a slot number can resolve it. Slots
//! are **stable for the list's lifetime**: [`OffHeap::sweep`] releases dead entries *in
//! place* — it never shifts, reorders or reuses a slot — so a `val` word a collection wrote
//! stays resolvable across any number of sweeps.
//!
//! This module is safe Rust apart from two narrow `unsafe` blocks — the `NonNull` deref in
//! [`MsoHandle`] and the `Box::from_raw` free in its `Drop` — each of which carries a
//! `// SAFETY:` rationale (the handle owns a live, boxed [`Mso`] for its whole lifetime).

use core::any::Any;
use core::ptr::NonNull;

use beam_alloc::RefcBinary;

/// An atomic reference count with `erts_refc_*` semantics.
///
/// Re-exported from `beam_alloc`, the workspace's *single* refcount primitive (the
/// lowest common ancestor of [`RefcBinary`] and the MSO container below) — see
/// [`beam_alloc::RefCount`] for the `Acquire`/`AcqRel` ordering guarantees.
pub use beam_alloc::RefCount;

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

/// One registered off-heap object of the per-process [`OffHeap`] list.
///
/// Two kinds of off-heap object are accounted for today:
///
/// - [`OffHeapEntry::Mso`] — a generic refcounted large object ([`MsoHandle`]).
/// - [`OffHeapEntry::Binary`] — a refcounted large binary
///   ([`beam_alloc::RefcBinary`]): the off-heap payload an on-heap `BinRef` word points
///   at. Both kinds share the one [`RefCount`] protocol, so one list can own either.
///
/// The variant says *what* the off-heap object is; *which* on-heap reference keeps it
/// alive is the caller's concern — see [`OffHeap::sweep`].
pub enum OffHeapEntry {
    /// A generic refcounted large object.
    Mso(MsoHandle),
    /// A refcounted large binary payload.
    Binary(RefcBinary),
}

impl OffHeapEntry {
    /// The bookkeeping overhead this entry contributes to [`OffHeap::overhead_bytes`],
    /// in bytes: an [`Mso`]'s apparent (Erlang-visible) size, or a [`RefcBinary`]'s
    /// payload length.
    pub fn overhead_bytes(&self) -> usize {
        match self {
            OffHeapEntry::Mso(handle) => handle.apparent_size(),
            OffHeapEntry::Binary(binary) => binary.len(),
        }
    }

    /// Alias for [`overhead_bytes`](OffHeapEntry::overhead_bytes), matching the
    /// [`MsoHandle::apparent_size`] naming.
    pub fn apparent_size(&self) -> usize {
        self.overhead_bytes()
    }

    /// The [`MsoHandle`], when this entry is an [`Mso`](OffHeapEntry::Mso).
    pub fn as_mso(&self) -> Option<&MsoHandle> {
        match self {
            OffHeapEntry::Mso(handle) => Some(handle),
            OffHeapEntry::Binary(_) => None,
        }
    }

    /// The [`RefcBinary`], when this entry is a [`Binary`](OffHeapEntry::Binary).
    pub fn as_binary(&self) -> Option<&RefcBinary> {
        match self {
            OffHeapEntry::Binary(binary) => Some(binary),
            OffHeapEntry::Mso(_) => None,
        }
    }
}

impl From<MsoHandle> for OffHeapEntry {
    fn from(handle: MsoHandle) -> Self {
        OffHeapEntry::Mso(handle)
    }
}

impl From<RefcBinary> for OffHeapEntry {
    fn from(binary: RefcBinary) -> Self {
        OffHeapEntry::Binary(binary)
    }
}

/// The per-process list of off-heap large objects (`MSO(p)`).
///
/// The list owns a reference to each registered object, so a collection that fails to
/// find an object reachable can drop its entry and let the refcount do the rest:
/// [`sweep`](OffHeap::sweep) releases the entries the caller reports dead, and the
/// off-heap payload is freed when the released handle held the last reference (an
/// [`Mso`] payload at its zero refcount, a [`RefcBinary`]'s bytes at theirs).
///
/// # Slot stability
///
/// Storage is **slot-stable**: the list is addressed by the slot index an on-heap
/// off-heap reference stores in its `val` word. [`sweep`](OffHeap::sweep) releases a dead
/// entry *in place* (marks its slot released) and never removes, shifts, reorders or reuses
/// a slot, so every **surviving** entry keeps its original slot index for the list's whole
/// lifetime — a `val` word a collection wrote stays resolvable across any number of sweeps.
/// A released slot resolves to `None` ([`get`](OffHeap::get)) and is never handed out again:
/// [`insert`](OffHeap::insert) always appends at a fresh index.
///
/// # The liveness contract
///
/// *Which* on-heap reference keeps a registered off-heap object alive — the `BinRef`
/// word of a binary, the magic reference of a fun, the pointer in a sub-binary, … — is
/// **the caller's concern** (`beam_process`): a collection walks its rootset (and the
/// message queue / saved arguments) and passes a predicate telling this list what it
/// found live. `OffHeap` deliberately does not interpret on-heap terms itself; it only
/// applies the caller's predicate and releases everything else. That keeps the "what is
/// a root" policy in one place (the process) and this list a pure ownership container.
pub struct OffHeap {
    /// One slot per registered object, in slot order; `None` marks a slot a sweep released.
    /// A slot is never removed, so `entries.len()` is the next fresh slot index.
    entries: Vec<Option<OffHeapEntry>>,
    /// The number of **live** (non-released) entries, maintained by `push`/`insert`/`sweep`
    /// so [`len`](OffHeap::len)/[`is_empty`](OffHeap::is_empty) stay O(1).
    live: usize,
}

impl OffHeap {
    /// An empty off-heap list.
    pub fn new() -> OffHeap {
        OffHeap {
            entries: Vec::new(),
            live: 0,
        }
    }

    /// Adds an off-heap object to the list, appending it at a fresh slot.
    ///
    /// Accepts anything convertible into an [`OffHeapEntry`]: a [`MsoHandle`] (e.g. the
    /// result of [`Mso::new`]) or a [`RefcBinary`]. The slot it lands at is stable for the
    /// list's lifetime (see the type-level *slot stability* contract); use
    /// [`insert`](OffHeap::insert) when that slot is needed.
    pub fn push(&mut self, entry: impl Into<OffHeapEntry>) {
        self.entries.push(Some(entry.into()));
        self.live += 1;
    }

    /// Adds an off-heap object to the list and returns its slot.
    ///
    /// The returned slot is the index the on-heap off-heap-reference container stores in
    /// its `val` word (mirroring `beam_message::OffHeapTable::push`), so the object is
    /// resolvable again with [`get`](OffHeap::get). Slots are stable for the list's
    /// lifetime: `insert` only ever appends, at an index no earlier entry (live or
    /// released) occupied, so neither a later [`insert`](OffHeap::insert) nor a
    /// [`sweep`](OffHeap::sweep) that releases other entries ever invalidates a returned slot.
    pub fn insert(&mut self, entry: impl Into<OffHeapEntry>) -> usize {
        self.entries.push(Some(entry.into()));
        self.live += 1;
        self.entries.len() - 1
    }

    /// The live entry at `slot`, or `None` when the slot is out of range **or has been
    /// released** by a [`sweep`](OffHeap::sweep).
    pub fn get(&self, slot: usize) -> Option<&OffHeapEntry> {
        self.entries.get(slot).and_then(Option::as_ref)
    }

    /// Iterates the **live** entries in slot order, skipping released slots.
    pub fn iter(&self) -> impl Iterator<Item = &OffHeapEntry> {
        self.entries.iter().filter_map(Option::as_ref)
    }

    /// The number of **live** off-heap objects held (released slots are not counted).
    pub fn len(&self) -> usize {
        self.live
    }

    /// Whether the list holds no live off-heap objects (it may still have released slots).
    pub fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// The bookkeeping overhead of the list, in bytes: the sum over its **live** entries of
    /// each object's apparent size ([`OffHeapEntry::overhead_bytes`]) — an MSO's
    /// Erlang-visible size or a binary's payload length. This is the off-heap payload
    /// the process is charged for, not the list's own `Vec` capacity.
    pub fn overhead_bytes(&self) -> usize {
        self.iter().map(OffHeapEntry::overhead_bytes).sum()
    }

    /// Releases, **in place**, the entries for which `is_live` returns `false`, and returns
    /// how many were released.
    ///
    /// A released entry's slot is left in the list as a released slot, so every
    /// **surviving** entry keeps its original slot index — a `val` word that names a
    /// survivor stays valid (the type-level *slot stability* contract). A released slot is
    /// never reused: a later [`insert`](OffHeap::insert) appends at a fresh index, so a `val`
    /// that names a released slot resolves to `None` ([`get`](OffHeap::get)) rather than to
    /// an unrelated object.
    ///
    /// Dropping a released entry runs its refcount decrement, which frees the off-heap
    /// payload when the list held the last reference (the MSO destructor at zero; a
    /// `RefcBinary`'s buffer at zero). Each released entry is dropped **exactly once** — a
    /// slot already released by an earlier sweep is left untouched and its destructor never
    /// runs again. The caller decides liveness — see the type-level *liveness contract*.
    pub fn sweep<F: FnMut(&OffHeapEntry) -> bool>(&mut self, mut is_live: F) -> usize {
        let mut released = 0;
        for slot in &mut self.entries {
            // An already-released slot (`None`) is dead for good: leave it alone, and never
            // call `is_live` for it again.
            if slot.as_ref().is_some_and(|entry| !is_live(entry)) {
                // Releasing in place: this drops the entry exactly once, running its refcount
                // decrement (and freeing the object whose last reference this list held).
                *slot = None;
                released += 1;
            }
        }
        self.live -= released;
        released
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use beam_alloc::{AllocatorConfig, AllocRef, CarrierAllocator};

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

    /// An isolated carrier allocator, so a test can observe a binary's payload being
    /// freed through `AllocStats` without racing the process-wide registry allocator.
    fn test_allocator(name: &str) -> AllocRef {
        CarrierAllocator::new(AllocatorConfig::builder().name(name).build())
            .expect("allocator construction maps nothing")
            .share()
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
    fn off_heap_entry_accessors_dispatch_on_the_variant() {
        let alloc = test_allocator("gc-mso-entry-accessors");
        let mso = OffHeapEntry::from(Mso::new(5u32, 24));
        let binary =
            OffHeapEntry::from(RefcBinary::with_allocator(alloc, b"abcde").expect("allocate"));

        assert_eq!(mso.overhead_bytes(), 24);
        assert_eq!(mso.apparent_size(), 24);
        assert!(mso.as_mso().is_some());
        assert!(mso.as_binary().is_none());

        assert_eq!(binary.overhead_bytes(), 5);
        assert_eq!(binary.apparent_size(), 5);
        assert!(binary.as_binary().is_some());
        assert!(binary.as_mso().is_none());
        assert_eq!(
            binary.as_binary().map(|b| b.as_bytes()),
            Some(&b"abcde"[..])
        );
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
        let released = off_heap.sweep(|entry| entry.apparent_size() == 10);
        assert_eq!(released, 2);
        assert_eq!(off_heap.len(), 1);
        assert_eq!(off_heap.overhead_bytes(), 10);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        let survivor = off_heap
            .get(0)
            .expect("the survivor is still reachable at its original slot");
        assert_eq!(
            survivor
                .as_mso()
                .expect("the survivor is an MSO entry")
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
        assert_eq!(
            off_heap.get(0).expect("slot 0").as_mso().expect("an MSO entry").refcount(),
            1
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // Two entries for one object: releasing both runs the destructor exactly once.
        let shared = Mso::new(counter(&drops, 11), 16);
        let mut two = OffHeap::new();
        two.push(shared.clone());
        two.push(shared);
        assert_eq!(
            two.get(0).expect("slot 0").as_mso().expect("an MSO entry").refcount(),
            2
        );
        assert_eq!(two.sweep(|_| false), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1); // only the 9-byte object's payload

        assert_eq!(off_heap.sweep(|_| false), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn off_heap_reclaims_a_refc_binary_when_swept() {
        let alloc = test_allocator("gc-mso-binary-sweep");

        let binary =
            RefcBinary::with_allocator(alloc.clone(), b"hello off-heap binary").expect("allocate");
        // A handle kept *outside* the list observes the shared refcount and the payload.
        let observer = binary.clone();
        assert_eq!(observer.refcount(), 2);
        assert_eq!(alloc.stats().live_blocks, 1, "one payload block is live");

        let mut off_heap = OffHeap::new();
        off_heap.push(binary); // via `From<RefcBinary>`
        assert_eq!(off_heap.len(), 1);
        assert!(!off_heap.is_empty());
        assert_eq!(off_heap.overhead_bytes(), observer.len());
        assert_eq!(observer.as_bytes(), b"hello off-heap binary");
        assert_eq!(
            off_heap
                .get(0)
                .expect("the entry is still at its original slot 0")
                .as_binary()
                .map(RefcBinary::as_bytes),
            Some(&b"hello off-heap binary"[..])
        );

        // Nothing on the heap references the binary any more: the sweep releases the
        // list's reference, leaving only the outside observer's.
        assert_eq!(off_heap.sweep(|_| false), 1);
        assert!(off_heap.is_empty());
        assert_eq!(off_heap.len(), 0);
        assert_eq!(off_heap.overhead_bytes(), 0);
        assert_eq!(observer.refcount(), 1, "the list released its reference");
        assert_eq!(
            alloc.stats().live_blocks,
            1,
            "the payload survives while the outside handle lives"
        );
        assert_eq!(
            observer.as_bytes(),
            b"hello off-heap binary",
            "the payload is still intact after the list released it"
        );

        // Dropping the last handle frees the payload exactly once (no leak, no double free):
        // the allocator's live-block accounting returns to zero.
        drop(observer);
        assert_eq!(alloc.stats().live_blocks, 0, "the payload was freed");
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn off_heap_mixes_mso_and_binary_entries_and_sweeps_selectively() {
        let drops = Arc::new(AtomicUsize::new(0));
        let alloc = test_allocator("gc-mso-mixed");

        let binary = RefcBinary::with_allocator(alloc.clone(), b"mixed entry").expect("allocate");
        let binary_observer = binary.clone();
        assert_eq!(binary_observer.refcount(), 2);

        let mut off_heap = OffHeap::new();
        // The list holds the MSO's only reference (the local handle is dropped here), so
        // sweeping it must run its payload's destructor.
        off_heap.push(Mso::new(counter(&drops, 3), 64));
        off_heap.push(binary);
        assert_eq!(off_heap.len(), 2);
        assert_eq!(off_heap.overhead_bytes(), 64 + binary_observer.len());

        // Keep the binary, release the MSO.
        let released = off_heap.sweep(|entry| entry.as_binary().is_some());
        assert_eq!(released, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1, "the MSO payload was destroyed");
        assert_eq!(off_heap.len(), 1);
        assert_eq!(off_heap.overhead_bytes(), binary_observer.len());
        // The MSO (slot 0) was released in place; the binary survives at its **original** slot 1
        // — a sweep must not shift a survivor down into the released slot.
        let survivor = off_heap
            .get(1)
            .expect("the binary survives at its original slot");
        assert!(survivor.as_binary().is_some());
        assert!(survivor.as_mso().is_none());
        assert!(
            off_heap.get(0).is_none(),
            "the released MSO slot resolves to `None`, never to the shifted-in binary"
        );
        assert_eq!(
            binary_observer.refcount(),
            2,
            "the surviving list entry plus the outside observer"
        );

        // Now release the binary too: the refcount falls to the outside observer's single
        // reference, so the payload is *not* freed yet.
        assert_eq!(off_heap.sweep(|entry| entry.as_mso().is_some()), 1);
        assert!(off_heap.is_empty());
        assert_eq!(off_heap.overhead_bytes(), 0);
        assert_eq!(binary_observer.refcount(), 1, "the list released the binary");
        assert_eq!(
            alloc.stats().live_blocks,
            1,
            "the payload is still owned by the outside observer"
        );
        assert_eq!(binary_observer.as_bytes(), b"mixed entry");
        assert_eq!(drops.load(Ordering::SeqCst), 1, "no further MSO destructor");

        drop(binary_observer);
        assert_eq!(alloc.stats().live_blocks, 0, "the last handle freed the payload");
        assert_eq!(alloc.stats().live_bytes, 0);
    }

    #[test]
    fn insert_returns_increasing_slots_and_interleaves_with_push() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut off_heap = OffHeap::new();

        // `insert` returns the slot the entry was stored under, starting at 0.
        assert_eq!(off_heap.insert(Mso::new(counter(&drops, 0), 10)), 0);
        assert_eq!(off_heap.len(), 1);

        // The legacy `push` stays fire-and-forget: its return type is still `()`.
        let () = off_heap.push(Mso::new(counter(&drops, 1), 20));
        assert_eq!(off_heap.len(), 2);

        // Slots keep increasing across both inserters, and `len` tracks the total.
        assert_eq!(off_heap.insert(Mso::new(counter(&drops, 2), 30)), 2);
        let () = off_heap.push(Mso::new(counter(&drops, 3), 40));
        assert_eq!(off_heap.insert(Mso::new(counter(&drops, 4), 50)), 4);
        assert_eq!(off_heap.len(), 5);
        assert_eq!(off_heap.overhead_bytes(), 10 + 20 + 30 + 40 + 50);
    }

    #[test]
    fn get_returns_the_entry_at_a_slot_or_none_when_out_of_range() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut off_heap = OffHeap::new();
        off_heap.insert(Mso::new(counter(&drops, 42), 12));
        off_heap.insert(Mso::new(counter(&drops, 43), 24));

        let first = off_heap.get(0).expect("slot 0 is present");
        assert_eq!(first.overhead_bytes(), 12);
        assert_eq!(
            first
                .as_mso()
                .expect("an MSO entry")
                .downcast_ref::<DropCounter>()
                .map(|c| c.value),
            Some(42)
        );

        let second = off_heap.get(1).expect("slot 1 is present");
        assert_eq!(second.overhead_bytes(), 24);
        assert_eq!(
            second
                .as_mso()
                .expect("an MSO entry")
                .downcast_ref::<DropCounter>()
                .map(|c| c.value),
            Some(43)
        );

        assert!(off_heap.get(2).is_none(), "just past the end");
        assert!(off_heap.get(usize::MAX).is_none(), "far out of range");
    }

    #[test]
    fn iter_yields_every_entry_in_slot_order() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut off_heap = OffHeap::new();
        for (value, size) in [(0u32, 10usize), (1, 20), (2, 30), (3, 40)] {
            off_heap.insert(Mso::new(counter(&drops, value), size));
        }
        // A `push` mixes in and must still land last (slot 4).
        off_heap.push(Mso::new(counter(&drops, 4), 50));

        let sizes: Vec<usize> = off_heap.iter().map(OffHeapEntry::overhead_bytes).collect();
        assert_eq!(sizes, vec![10, 20, 30, 40, 50], "slot order is insertion order");
        assert_eq!(off_heap.iter().count(), off_heap.len());
    }

    #[test]
    fn mixed_entries_round_trip_through_insert_get_and_iter() {
        let alloc = test_allocator("gc-mso-mixed-slots");
        let drops = Arc::new(AtomicUsize::new(0));

        let mut off_heap = OffHeap::new();
        let mso_slot = off_heap.insert(Mso::new(counter(&drops, 5), 32));
        let binary_slot = off_heap
            .insert(RefcBinary::with_allocator(alloc, b"slot binary").expect("allocate"));
        assert_eq!((mso_slot, binary_slot), (0, 1));

        // Each slot resolves to the entry of the kind that was inserted there.
        let mso = off_heap.get(mso_slot).expect("slot 0 is the MSO");
        assert_eq!(
            mso.as_mso()
                .expect("an MSO entry")
                .downcast_ref::<DropCounter>()
                .map(|c| c.value),
            Some(5)
        );
        assert!(mso.as_binary().is_none());
        assert_eq!(
            off_heap
                .get(binary_slot)
                .expect("slot 1 is the binary")
                .as_binary()
                .map(RefcBinary::as_bytes),
            Some(&b"slot binary"[..])
        );
        assert!(off_heap.get(binary_slot).expect("present").as_mso().is_none());

        // `iter` walks both kinds in slot order.
        let is_mso: Vec<bool> = off_heap.iter().map(|e| e.as_mso().is_some()).collect();
        assert_eq!(is_mso, vec![true, false]);
        assert_eq!(off_heap.overhead_bytes(), 32 + b"slot binary".len());
    }

    #[test]
    fn surviving_slots_stay_valid_after_a_retaining_sweep() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut off_heap = OffHeap::new();
        for (value, size) in [(0u32, 10usize), (1, 20), (2, 30)] {
            off_heap.insert(Mso::new(counter(&drops, value), size));
        }

        // A sweep that keeps every entry releases nothing, so nothing is removed and no
        // compaction happens: each survivor is still reachable at its original slot.
        assert_eq!(off_heap.sweep(|_| true), 0);
        assert_eq!(off_heap.len(), 3);
        for (slot, size) in [(0usize, 10usize), (1, 20), (2, 30)] {
            let entry = off_heap.get(slot).expect("survivor at its original slot");
            assert_eq!(entry.overhead_bytes(), size);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // Interleaving an insert after the retaining sweep still appends: the next slot is 3.
        assert_eq!(off_heap.insert(Mso::new(counter(&drops, 3), 40)), 3);
    }

    /// **The slot-stable sweep contract**, which the previous compaction (a `Vec::retain`) broke:
    /// releasing a *middle* entry must not shift the survivors down, because an on-heap off-heap
    /// reference stores the slot index in its `val` word. Fails on the old compacting code — there
    /// the survivor of slot 4 would have moved to slot 3 and slot 1 would name it.
    #[test]
    fn sweep_releases_in_place_and_keeps_every_surviving_slot_stable() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut off_heap = OffHeap::new();

        // Five entries at slots 0..=4, each with a distinguishable size and payload value.
        for (value, size) in [(0u32, 10usize), (1, 20), (2, 30), (3, 40), (4, 50)] {
            assert_eq!(
                off_heap.insert(Mso::new(counter(&drops, value), size)),
                value as usize
            );
        }
        assert_eq!(off_heap.len(), 5);
        assert_eq!(off_heap.overhead_bytes(), 150);

        // Release the **middle** entries (slots 1 and 3) and keep slots 0, 2 and 4.
        let released = off_heap.sweep(|entry| matches!(entry.apparent_size(), 10 | 30 | 50));
        assert_eq!(released, 2, "slots 1 and 3 were released");
        // (e) each released entry's destructor ran exactly once.
        assert_eq!(
            drops.load(Ordering::SeqCst),
            2,
            "each released entry's destructor ran exactly once"
        );

        // (a) every survivor is still reachable at its **original** slot, object intact.
        for (slot, expected_value, expected_size) in
            [(0usize, 0u32, 10usize), (2, 2, 30), (4, 4, 50)]
        {
            let entry = off_heap
                .get(slot)
                .unwrap_or_else(|| panic!("slot {slot} must survive at its original index"));
            assert_eq!(entry.overhead_bytes(), expected_size, "slot {slot}");
            assert_eq!(
                entry
                    .as_mso()
                    .expect("an MSO entry")
                    .downcast_ref::<DropCounter>()
                    .map(|c| c.value),
                Some(expected_value),
                "slot {slot} still names its original object"
            );
        }

        // (b) the released slots answer `None` — never an unrelated shifted-in survivor.
        assert!(off_heap.get(1).is_none(), "released slot 1");
        assert!(off_heap.get(3).is_none(), "released slot 3");

        // (d) the live-only views count/iterate/sum the survivors alone, in slot order.
        assert_eq!(off_heap.len(), 3);
        assert!(!off_heap.is_empty());
        let live_sizes: Vec<usize> = off_heap.iter().map(OffHeapEntry::overhead_bytes).collect();
        assert_eq!(live_sizes, vec![10, 30, 50], "only the survivors, in slot order");
        assert_eq!(off_heap.iter().count(), off_heap.len());
        assert_eq!(off_heap.overhead_bytes(), 90);

        // (c) a further insert appends at a fresh index and never reuses the released slots.
        assert_eq!(off_heap.insert(Mso::new(counter(&drops, 5), 60)), 5);
        assert!(off_heap.get(1).is_none(), "slot 1 stays released");
        assert!(off_heap.get(3).is_none(), "slot 3 stays released");
        let fresh = off_heap.get(5).expect("the fresh entry is at slot 5");
        assert_eq!(
            fresh
                .as_mso()
                .expect("an MSO entry")
                .downcast_ref::<DropCounter>()
                .map(|c| c.value),
            Some(5)
        );
        assert_eq!(off_heap.len(), 4);
        assert_eq!(off_heap.overhead_bytes(), 10 + 30 + 50 + 60);

        // A second sweep leaves the already-released slots untouched (no destructor runs again):
        // exactly one *further* release, the 30-byte slot 2.
        assert_eq!(off_heap.sweep(|entry| entry.apparent_size() != 30), 1);
        assert_eq!(
            drops.load(Ordering::SeqCst),
            3,
            "each released entry was dropped exactly once, ever"
        );
        assert_eq!(off_heap.len(), 3);
        assert_eq!(off_heap.overhead_bytes(), 10 + 50 + 60);

        // The three live entries are dropped exactly once more when the list itself drops.
        drop(off_heap);
        assert_eq!(drops.load(Ordering::SeqCst), 6);
    }
}
