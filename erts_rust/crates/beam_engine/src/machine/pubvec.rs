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

#![allow(unsafe_code)]

//! A growable array whose **header is addressable**: `#[repr(C)]` `{ ptr, len, cap }`.
//!
//! `Machine::stack` (the Erlang stack of Y slots) and `Machine::frames` (the call-frame
//! stack) are two pieces of process state the baseline JIT's emitted code must be able
//! to grow by a known amount — the `allocate` instruction's frame push. A `Vec<T>`'s
//! length lives in a private field with no guaranteed layout, so generated code cannot
//! reach it; [`PubVec`] publishes exactly the three words such a push needs, at offsets
//! computed in Rust with [`core::mem::offset_of!`], and the *contents are the storage*
//! (there is no second copy of the length to keep in step).
//!
//! # The invariant emitted code relies on
//!
//! For every live `PubVec<T>`, `ptr` is the base of an allocation of `cap` elements of
//! `T`, and the `len` elements at `[0, len)` are initialized. Emitted code may
//!
//! * read `ptr`/`len`/`cap`,
//! * write initialized `T` values into the **spare capacity** `[len, cap)`, and
//! * store a larger `len` (never past `cap`, never past what it initialized),
//!
//! and nothing else. Every method here keeps the same invariant, so the Rust side and
//! emitted code observe one store: a `push` from Rust after an emitted push sees the
//! emitted frame, and a reallocation here is immediately visible to the next emitted
//! push (the header words are the live ones, never a cached mirror).
//!
//! # Why not a `Vec` plus published copies of its fields
//!
//! Because the copy is a second source of truth. Anything in the crate can push a
//! frame (`push_frame_window`, `with_roots`' loose-term park, `trim`, a raise landing,
//! `reset`, …), so a mirror would have to be re-synchronised at every such site *and*
//! at every point generated code can run again — one missed site is a wild write into
//! freed memory. Owning the three words outright removes the class: the header pointer
//! is published once per driver loop (like `JitRuntime::current_code`), the `Machine`
//! never moves while the loop runs, and the header's contents are always the truth.
//!
//! # `unsafe` rationale
//!
//! The module is the crate's second raw-ownership type beside the term builders: it
//! re-creates a `Vec<T>` from the header around each length-changing operation and
//! `mem::forget`s it again, so the allocation strategy, the growth policy and the drop
//! glue all stay `std`'s (nothing is re-implemented). The `// SAFETY:` comments on each
//! block state which invariant of the module makes the reconstruction valid.

use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};
use core::ptr::NonNull;

/// A growable array whose `ptr`/`len`/`cap` header generated code can address.
///
/// The field order and the `#[repr(C)]` attribute are part of the contract: the
/// offsets are published to the baseline tier's emitted code (`runtime::STORE_*_OFF`),
/// so every `PubVec<T>` — whatever `T` is — lays its header out identically.
pub(crate) struct PubVec<T> {
    /// The base of the allocation (`Vec::as_mut_ptr`'s value).
    pub(crate) ptr: *mut T,
    /// The number of initialized elements (`[0, len)`).
    pub(crate) len: usize,
    /// The number of elements the allocation holds.
    pub(crate) cap: usize,
    /// `PunVec` owns `T`s; keep the drop/auto-trait story identical to `Vec<T>`'s.
    marker: PhantomData<T>,
}

// SAFETY: a `PubVec` uniquely owns its allocation exactly as a `Vec` does — the raw
// pointer buys addressability of the header, not shared ownership — so the send/sync
// conditions are `Vec<T>`'s.
unsafe impl<T: Send> Send for PubVec<T> {}
// SAFETY: `&PubVec<T>` only ever reads the elements (as `&T`), like `&Vec<T>`.
unsafe impl<T: Sync> Sync for PubVec<T> {}

impl<T> PubVec<T> {
    /// An empty store with room for `capacity` elements.
    pub(crate) fn with_capacity(capacity: usize) -> PubVec<T> {
        let mut out = PubVec {
            ptr: NonNull::dangling().as_ptr(),
            len: 0,
            cap: 0,
            marker: PhantomData,
        };
        out.adopt(Vec::with_capacity(capacity));
        out
    }

    /// The header's three words (`ptr`, `len`, `cap`).
    ///
    /// Read-only introspection, used by the crate's tests to prove the header matches
    /// the slice after every operation.
    #[cfg(test)]
    pub(crate) fn header(&self) -> (*mut T, usize, usize) {
        (self.ptr, self.len, self.cap)
    }

    /// The elements, as a slice.
    #[inline]
    pub(crate) fn as_slice(&self) -> &[T] {
        // SAFETY: `[0, len)` is initialized and within the `cap`-element allocation
        // (the module invariant); the returned borrow is tied to `&self`.
        unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
    }

    /// The elements, mutably.
    #[inline]
    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, and `&mut self` makes the borrow exclusive.
        unsafe { core::slice::from_raw_parts_mut(self.ptr, self.len) }
    }

    /// The number of elements written.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The number of elements the allocation holds.
    ///
    /// Emitted code reads this word directly out of the header (`STORE_CAP_OFF`);
    /// no Rust caller outside this module's tests needs it.
    #[cfg_attr(not(test), allow(dead_code))]
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.cap
    }

    /// The base pointer of the allocation (a fresh read of the header).
    ///
    /// Emitted code reads this word directly out of the header (`STORE_PTR_OFF`);
    /// no Rust caller outside this module's tests needs it.
    #[cfg_attr(not(test), allow(dead_code))]
    #[inline]
    pub(crate) fn as_ptr(&self) -> *const T {
        self.ptr
    }

    /// The base pointer of the allocation (a fresh read of the header).
    #[inline]
    pub(crate) fn as_mut_ptr(&mut self) -> *mut T {
        self.ptr
    }

    /// Append one element, growing the allocation if it is full.
    #[inline]
    pub(crate) fn push(&mut self, value: T) {
        if self.len == self.cap {
            self.grow(1);
        }
        // SAFETY: `len < cap` after the growth check, so `ptr + len` is inside the
        // allocation and uninitialized; the write initializes it and the length bump
        // publishes it.
        unsafe { self.ptr.add(self.len).write(value) };
        self.len += 1;
    }

    /// Remove and return the last element, or `None` when empty.
    #[inline]
    pub(crate) fn pop(&mut self) -> Option<T> {
        if self.len == 0 {
            return None;
        }
        self.len -= 1;
        // SAFETY: the element at the new `len` was initialized and is now outside the
        // published range, so the read moves it out exactly once.
        Some(unsafe { self.ptr.add(self.len).read() })
    }

    /// Keep the first `new_len` elements, dropping (or forgetting, for `Copy` types)
    /// the rest. A no-op when the store is already that short.
    #[inline]
    pub(crate) fn truncate(&mut self, new_len: usize) {
        if new_len >= self.len {
            return;
        }
        let mut vec = self.take();
        vec.truncate(new_len);
        self.adopt(vec);
    }

    /// Drop every element, keeping the allocation.
    #[inline]
    pub(crate) fn clear(&mut self) {
        let mut vec = self.take();
        vec.clear();
        self.adopt(vec);
    }

    /// Extend to `new_len` elements, filling the new ones with clones of `value`.
    ///
    /// The push path [`Machine::push_frame_window`](super::Machine) uses to size a
    /// frame's Y slots; `Eterm::default()` is `NIL`, so the fill and the growth are the
    /// `Vec::resize` a `Vec`-typed field performed.
    #[inline]
    pub(crate) fn resize(&mut self, new_len: usize, value: T)
    where
        T: Clone,
    {
        let mut vec = self.take();
        vec.resize(new_len, value);
        self.adopt(vec);
    }

    /// Reconstruct an owned `Vec` from the header, leaving an empty store behind.
    #[inline]
    fn take(&mut self) -> Vec<T> {
        // SAFETY: `ptr`/`len`/`cap` are the words of the `Vec` this store owns (the
        // module invariant), so `from_raw_parts` takes back exactly that allocation;
        // the dangling replacement is what `Vec::new()` holds.
        let vec = unsafe { Vec::from_raw_parts(self.ptr, self.len, self.cap) };
        self.ptr = NonNull::dangling().as_ptr();
        self.len = 0;
        self.cap = 0;
        vec
    }

    /// Make `vec` the storage, without dropping or reallocating it.
    #[inline]
    fn adopt(&mut self, vec: Vec<T>) {
        let mut vec = ManuallyDrop::new(vec);
        self.ptr = vec.as_mut_ptr();
        self.len = vec.len();
        self.cap = vec.capacity();
    }

    /// Grow the allocation to hold at least one more element, through `Vec::reserve`
    /// so the growth policy stays `std`'s.
    #[cold]
    #[inline(never)]
    fn grow(&mut self, additional: usize) {
        let mut vec = self.take();
        vec.reserve(additional);
        self.adopt(vec);
    }
}

impl<T> Drop for PubVec<T> {
    fn drop(&mut self) {
        // SAFETY: the store still owns its allocation (`take` is the only place that
        // gives it away, and it clears the header as it does so), so reconstructing the
        // `Vec` frees exactly the elements and the block it holds.
        drop(unsafe { Vec::from_raw_parts(self.ptr, self.len, self.cap) });
    }
}

impl<T> Deref for PubVec<T> {
    type Target = [T];

    #[inline]
    fn deref(&self) -> &[T] {
        self.as_slice()
    }
}

impl<T> DerefMut for PubVec<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut [T] {
        self.as_mut_slice()
    }
}

impl<'a, T> IntoIterator for &'a PubVec<T> {
    type Item = &'a T;
    type IntoIter = core::slice::Iter<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_slice().iter()
    }
}

impl<'a, T> IntoIterator for &'a mut PubVec<T> {
    type Item = &'a mut T;
    type IntoIter = core::slice::IterMut<'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.as_mut_slice().iter_mut()
    }
}

impl<T> Default for PubVec<T> {
    fn default() -> PubVec<T> {
        PubVec::with_capacity(0)
    }
}

impl<T: core::fmt::Debug> core::fmt::Debug for PubVec<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.as_slice().fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The header is the store: `len`/`cap`/`ptr` after every operation are the slice's
    /// own, which is what makes an emitted push visible to Rust and vice versa.
    #[test]
    fn the_header_tracks_every_mutation() {
        let mut store: PubVec<u64> = PubVec::with_capacity(4);
        assert_eq!(store.header(), (store.ptr, 0, 4));
        store.push(7);
        assert_eq!(store.as_slice(), &[7]);
        assert_eq!(store.header().1, 1);
        store.push(8);
        store.push(9);
        store.truncate(2);
        assert_eq!(store.as_slice(), &[7, 8]);
        assert_eq!(store.len(), 2);
        assert_eq!(store.pop(), Some(8));
        assert_eq!(store.pop(), Some(7));
        assert_eq!(store.pop(), None);
        store.push(1);
        store.clear();
        assert_eq!(store.len(), 0);
        assert_eq!(store.capacity(), 4, "clear keeps the allocation");
    }

    /// A growth moves the block and the header follows it: the old pointer is not kept
    /// anywhere, so the next emitted push writes into the new allocation.
    #[test]
    fn growth_republishes_the_pointer_and_capacity() {
        let mut store: PubVec<u64> = PubVec::with_capacity(1);
        let (ptr0, _, _) = store.header();
        store.push(1);
        store.push(2); // forces the growth path
        let (ptr1, len, cap) = store.header();
        assert_eq!((len, cap), (2, cap));
        assert_ne!(ptr0, ptr1, "the block moved");
        assert_eq!(store.as_slice(), &[1, 2]);
        assert_eq!(store.as_ptr(), ptr1);
        assert_eq!(store.as_mut_ptr(), ptr1);
    }

    /// `resize` is the Y-slot fill: the new words are clones of the value and the length
    /// is the whole extent, so a scan of `[0, len)` sees an initialized term everywhere.
    #[test]
    fn resize_fills_and_publishes_the_whole_extent() {
        let mut store: PubVec<u64> = PubVec::with_capacity(2);
        store.push(5);
        store.resize(6, 0);
        assert_eq!(store.as_slice(), &[5, 0, 0, 0, 0, 0]);
        assert_eq!(store.len(), 6);
        assert!(store.capacity() >= 6);
        store.resize(1, 0);
        assert_eq!(store.as_slice(), &[5]);
    }

    /// Deref/iterating borrows are the slice's: the crate-wide call sites
    /// (`for &w in &machine.stack`, `&mut machine.stack` as a `&mut [Eterm]`, index
    /// syntax) all go through these two impls.
    #[test]
    fn deref_iteration_and_indexing_are_slice_shaped() {
        let mut store: PubVec<u64> = PubVec::with_capacity(0);
        store.push(1);
        store.push(2);
        let sum: u64 = (&store).into_iter().sum();
        assert_eq!(sum, 3);
        for slot in &mut store {
            *slot *= 10;
        }
        assert_eq!(store[0], 10);
        assert_eq!(&store[..], &[10, 20]);
        let slice: &mut [u64] = &mut store;
        slice[1] = 7;
        assert_eq!(store.as_slice(), &[10, 7]);
        assert_eq!(format!("{store:?}"), "[10, 7]");
    }

    /// The store owns its elements: dropping it drops them (a leak here would be a leak
    /// per process, since `Machine` is not pooled) and frees the block.
    #[test]
    fn drop_releases_the_elements_and_the_block() {
        use std::rc::Rc;

        let witness = Rc::new(());
        {
            let mut store: PubVec<Rc<()>> = PubVec::with_capacity(1);
            store.push(Rc::clone(&witness));
            store.push(Rc::clone(&witness));
            assert_eq!(Rc::strong_count(&witness), 3);
        }
        assert_eq!(Rc::strong_count(&witness), 1);
    }

    /// An emitted push writes the spare capacity and bumps `len` itself; the Rust side
    /// then sees the frame with no commit step anywhere.
    #[test]
    fn a_raw_len_bump_inside_capacity_is_visible_to_rust() {
        let mut store: PubVec<u64> = PubVec::with_capacity(8);
        store.push(1);
        // SAFETY: this is exactly what generated code does — write two initialized
        // elements into `[len, cap)` and publish the new length.
        unsafe {
            let base = store.as_mut_ptr();
            base.add(1).write(11);
            base.add(2).write(12);
            store.len = 3;
        }
        assert_eq!(store.as_slice(), &[1, 11, 12]);
        assert_eq!(store.len(), 3);
        assert_eq!(store.pop(), Some(12));
    }
}
