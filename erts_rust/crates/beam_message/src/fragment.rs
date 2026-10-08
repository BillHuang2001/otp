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

//! The **message-fragment model**: heap fragments ([`HeapFragment`]) and the ordered chain of
//! them a message owns ([`Fragments`]).
//!
//! A message payload that does not fit the receiver's process heap is not dropped: the sender
//! allocates a **heap fragment** and copies the payload into it, so the payload has a stable home
//! until the receiver moves it onto its own heap. This module owns that model.
//!
//! # Ground truth
//!
//! The legacy C `ErlHeapFragment` (`erts/emulator/beam/erl_message.h:198-205`) is
//! `{ ErlHeapFragment *next; ErlOffHeap off_heap; Uint alloc_size; Uint used_size; Eterm mem[1]; }`
//! — a bump heap region (`mem[1..alloc_size]`, `used_size` words used) plus its own off-heap list
//! (`ErlOffHeap`, `erl_message.h:89-92`). A message's `ErtsMessage` links a chain of fragments
//! (`bp->next`) and points its payload root (`m[0]`) *into* one of them.
//! Every copied `BinRef`/`ExternalThing`/fun is threaded onto the fragment's `off_heap` during the
//! copy (`copy.c:895-906`), so the fragment — not the sender — owns them while the message lives.
//!
//! On receive the fragment's off-heap list is spliced into the process's MSO list and the payload
//! is later moved onto the process heap: `erts_move_multi_frags` (`copy.c:2049-2111`) for a
//! multi-fragment chain and `erts_copy_one_frag` (`erl_gc.c:2513-2613`) for a single fragment. Our
//! [`Fragments::move_into`] is the Rust analogue of that move.
//!
//! # The Rust substitution (documented deviation)
//!
//! The C move is a GC-style **relink**: the terms' off-heap references are raw pointers to the
//! off-heap objects, so relinking the object list needs no term rewrite (only the in-place move
//! markers of `erts_move_multi_frags`). Our M1 encoding stores an off-heap reference as a **slot
//! index** into the owning table (see the crate `CONTEXT.md`, Design Decisions), so the move must
//! re-materialise the term with fresh slots: [`Fragments::move_into`] therefore copies each root
//! with [`copy_struct`] (the fragment's off-heap table as source, the receiver's as destination),
//! which re-registers — refcounts — every off-heap object the payload references into the
//! destination table with fresh slots, and then releases the fragment's own references. The **net
//! refcount is unchanged by the move** (one increment from the copy, one decrement from the
//! fragment's release), exactly as the C relink leaves it, so the ownership transfer is faithful
//! even though the mechanism (copy + release) differs from the C's in-place move markers.
//!
//! `unsafe` does not appear here: the fragment is built from the safe [`Heap`] bump allocator, the
//! copier and the [`OffHeapTable`] API.
//!
//! # The fragment contract (what a host relies on)
//!
//! The oversized-message model is a small, explicit contract a runtime (a host such as `beam_rt`)
//! builds its send/receive path on:
//!
//! * **A fragment is sized to the whole payload and never reallocates.** [`HeapFragment::new`]
//!   takes the region's capacity, and [`send_fragmented`](crate::send_fragmented) sizes it to at
//!   least `size_object(term)`, so **one** fragment holds a whole payload. The backing [`Heap`]
//!   never reallocates, so a root pointer returned by [`HeapFragment::copy_root`] stays valid for
//!   the fragment's lifetime, including after the fragment (and the [`Fragments`] chain holding it)
//!   is moved: the pointer-stability invariant [`crate::Message::payload`] depends on. Splitting
//!   one payload across a chain is **not** done here: a caller that wants a size-bounded split
//!   builds the chain itself ([`Fragments::push`]).
//! * **A fragmented message's off-heap objects live in the fragment.** While the message is queued
//!   the fragment's own off-heap table owns the payload's MSO handles and refc-binary handles (and
//!   node entries), so the [`Message`](crate::Message)'s `off_heap`/`binaries`/`nodes` lists are
//!   empty for it (see [`crate::send_fragmented`]).
//! * **[`Fragments::move_into`] is the receiver's move-back, and it is one-shot and
//!   net-refcount-preserving.** It copies each root onto the receiver's heap (fresh off-heap slots)
//!   and then releases the fragment's own references: the net refcount of every moved off-heap
//!   object is unchanged (one increment from the copy, one decrement from the release), exactly as
//!   the C relink leaves it. Ownership is given up only after the **whole** chain has moved, so a
//!   mid-move failure never leaves the chain half-stripped.

use beam_alloc::{AllocRef, Heap};
use beam_common::Result;
use beam_term::prelude::*;

use crate::copy::{copy_struct, CopyContext};
use crate::off_heap::OffHeapTable;

/// A message heap fragment: a bump-allocated heap region, the off-heap objects its payload
/// references, and the root term(s) that live in the region.
///
/// Mirrors the C `ErlHeapFragment`: [`heap`](HeapFragment::heap) is the `mem[1..alloc_size]` region
/// (whose `used` words are [`used`](HeapFragment::used)), [`off_heap`](HeapFragment::off_heap) is
/// the fragment's own off-heap list, and [`roots`](HeapFragment::roots) records the payload roots
/// (`m[0]` pointing into `mem`) so [`Fragments::move_into`] knows what to move.
///
/// The backing [`Heap`] **never reallocates**, so a root pointer handed out while a fragment is
/// filled stays valid after the fragment (and the [`Fragments`] chain holding it) is moved — the
/// pointer-stability invariant the message model relies on.
pub struct HeapFragment {
    heap: Heap,
    off_heap: OffHeapTable,
    roots: Vec<Eterm>,
}

impl HeapFragment {
    /// A new fragment with `capacity_words` words of heap, backed by the default allocator.
    pub fn new(capacity_words: usize) -> Result<HeapFragment> {
        HeapFragment::with_allocator(beam_alloc::default_allocator().clone(), capacity_words)
    }

    /// A new fragment with `capacity_words` words of heap, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, capacity_words: usize) -> Result<HeapFragment> {
        Ok(HeapFragment {
            heap: Heap::with_allocator(alloc, capacity_words)?,
            off_heap: OffHeapTable::new(),
            roots: Vec::new(),
        })
    }

    /// The fragment's heap region (`mem[1..alloc_size]`).
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The fragment's heap region, mutably — for callers that build terms directly.
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The off-heap objects the payload references (the fragment's `off_heap`).
    pub fn off_heap(&self) -> &OffHeapTable {
        &self.off_heap
    }

    /// The off-heap table, mutably.
    pub fn off_heap_mut(&mut self) -> &mut OffHeapTable {
        &mut self.off_heap
    }

    /// The number of heap words handed out (`used_size`).
    pub fn used(&self) -> usize {
        self.heap.used()
    }

    /// The heap region's capacity in words (`alloc_size`).
    pub fn capacity(&self) -> usize {
        self.heap.capacity()
    }

    /// The words still available in the region.
    pub fn available(&self) -> usize {
        self.heap.available()
    }

    /// The administrative overhead of the fragment's **MSO** objects, in bytes (the C
    /// `ErlOffHeap.overhead` the collector uses to force a GC).
    pub fn off_heap_overhead_bytes(&self) -> usize {
        self.off_heap.overhead_bytes()
    }

    /// Copies `term` from a process whose off-heap table is `src` into this fragment's heap and
    /// records the copy as a root.
    ///
    /// This is the `new_message_buffer` + copy step of the C sender
    /// (`erts_try_alloc_message_on_heap`, `erl_message.c:622-691`): the copied term's off-heap
    /// references are **refcounted into this fragment's** off-heap table (fresh slots), so the
    /// fragment owns them. The returned root points into this fragment's heap.
    pub fn copy_root(&mut self, term: Eterm, src: &OffHeapTable) -> Result<Eterm> {
        let root = {
            // Destructured so the heap and the off-heap table are borrowed disjointly (the copier
            // needs both at once).
            let HeapFragment { heap, off_heap, .. } = self;
            let mut ctx = CopyContext::new(src, off_heap);
            copy_struct(term, heap, &mut ctx)?
        };
        self.roots.push(root);
        Ok(root)
    }

    /// Records `root` (which must point into this fragment's heap) as a payload root.
    pub fn push_root(&mut self, root: Eterm) {
        self.roots.push(root);
    }

    /// The payload roots this fragment holds.
    pub fn roots(&self) -> &[Eterm] {
        &self.roots
    }

    /// Whether `term`'s object (its first word) lies inside this fragment's heap region.
    ///
    /// Immediates and literal-area terms are never *inside* a fragment (they live elsewhere), so
    /// they report `false`. This is the check a receiver uses to confirm a message's payload is
    /// fragment-resident.
    pub fn contains(&self, term: Eterm) -> bool {
        match crate::copy::identity(term) {
            Some(addr) => {
                // `hend - capacity` is the region's base (`Heap` never reallocates).
                let base = self.heap.hend().wrapping_sub(self.heap.capacity()) as usize;
                let end = self.heap.hend() as usize;
                addr >= base && addr < end
            }
            None => false,
        }
    }
}

/// An ordered chain of [`HeapFragment`]s — the message-buffer model of a large message.
///
/// The C links fragments through `ErlHeapFragment.next`; this is that chain as a `Vec`, preserving
/// order (the chain order is the order the roots must be moved back in).
#[derive(Default)]
pub struct Fragments {
    fragments: Vec<HeapFragment>,
}

impl Fragments {
    /// An empty chain.
    pub fn new() -> Fragments {
        Fragments {
            fragments: Vec::new(),
        }
    }

    /// Whether the chain holds no fragments.
    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    /// The number of fragments in the chain.
    pub fn len(&self) -> usize {
        self.fragments.len()
    }

    /// Appends `fragment` at the tail (the chain tail is `ErlHeapFragment.next`).
    pub fn push(&mut self, fragment: HeapFragment) {
        self.fragments.push(fragment);
    }

    /// Iterates the fragments in chain order.
    pub fn iter(&self) -> impl Iterator<Item = &HeapFragment> {
        self.fragments.iter()
    }

    /// Iterates the fragments in chain order, mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut HeapFragment> {
        self.fragments.iter_mut()
    }

    /// The total used heap words across the whole chain (the process's message-buffer accounting,
    /// `used_size` summed — `erts_link_mbuf_to_proc`, `erl_message.c:562-590`).
    pub fn total_used_words(&self) -> usize {
        self.fragments.iter().map(HeapFragment::used).sum()
    }

    /// The total administrative overhead of the chain's **MSO** objects, in bytes.
    pub fn off_heap_overhead_bytes(&self) -> usize {
        self.fragments
            .iter()
            .map(HeapFragment::off_heap_overhead_bytes)
            .sum()
    }

    /// Moves every fragment's roots onto the destination heap `dst`, and transfers the fragments'
    /// off-heap objects into `dst_off_heap`, returning the moved roots in chain order (each
    /// fragment's roots in their recorded order).
    ///
    /// This is the Rust analogue of `erts_move_multi_frags`/`erts_copy_one_frag`: for each fragment
    /// in order it copies each root with [`copy_struct`] (the fragment's off-heap table as source,
    /// `dst_off_heap` as destination) — so the moved term references **fresh** slots in
    /// `dst_off_heap`, the receiver's table — and then releases the fragment's own off-heap
    /// references, handing ownership to the destination. Refcounted **refc binaries and MSO
    /// objects move as handles**: the payload is shared (same [`RefcBinary::data_ptr`]), never
    /// deep-copied.
    ///
    /// Ownership is given up only after **every** root in the chain has been copied: the fragment
    /// tables are released in a second pass once the whole move has succeeded. A mid-move failure
    /// (destination heap exhaustion, or a root whose off-heap slot does not resolve) therefore
    /// yields the typed [`BeamError`](beam_common::BeamError) with **no** fragment stripped: the
    /// chain still owns every off-heap reference it started with. The words the failed attempt
    /// already wrote into `dst`, and the destination-table references it registered for the roots it
    /// did copy, are the caller's to discard on that path; a retry needs a larger `dst` and a fresh
    /// `dst_off_heap`.
    ///
    /// The move is **one-shot**, as in C: a second `move_into` of a payload that carried off-heap
    /// references finds the roots' slots unresolved (a typed `BadArg`), because the fragment tables
    /// are now empty; a payload with no off-heap references would simply be copied again.
    ///
    /// [`RefcBinary::data_ptr`]: beam_alloc::RefcBinary::data_ptr
    pub fn move_into(
        &mut self,
        dst: &mut Heap,
        dst_off_heap: &mut OffHeapTable,
    ) -> Result<Vec<Eterm>> {
        let mut moved = Vec::new();
        for fragment in self.fragments.iter_mut() {
            let HeapFragment { off_heap, roots, .. } = fragment;
            for &root in roots.iter() {
                let mut ctx = CopyContext::new(off_heap, dst_off_heap);
                moved.push(copy_struct(root, dst, &mut ctx)?);
            }
        }
        // Ownership transfer, only once the whole chain has moved: `dst_off_heap` now owns a clone
        // of every off-heap object the copied payloads reference, so each fragment gives up its own
        // references (its `Drop` releases any node entries it held).
        for fragment in self.fragments.iter_mut() {
            release_off_heap(&mut fragment.off_heap);
        }
        Ok(moved)
    }
}

/// Gives up a fragment's own off-heap references after a move.
///
/// Taking the table leaves a fresh one in its place; dropping the taken table releases its node
/// entries ([`OffHeapTable`]'s `Drop`) and its MSO / refc-binary handles (each handle's own
/// `Drop`).
fn release_off_heap(off_heap: &mut OffHeapTable) {
    drop(core::mem::take(off_heap));
}

#[cfg(test)]
mod tests {
    use beam_alloc::Heap;
    use beam_gc::Mso;
    use beam_term::compare;
    use beam_term::prelude::*;

    use super::{Fragments, HeapFragment};
    use crate::layout;
    use crate::off_heap::{MsoHandle, OffHeapTable};

    /// Builds the list `[e0, e1, ...]` on `heap`, tail-first (no recursion).
    fn build_list(heap: &mut Heap, elems: &[Eterm]) -> Eterm {
        let mut acc = NIL;
        for &elem in elems.iter().rev() {
            acc = heap.cons(elem, acc).expect("the list fits");
        }
        acc
    }

    #[test]
    fn a_new_fragment_has_full_capacity_and_no_roots() {
        let fragment = HeapFragment::new(16).expect("the fragment allocates");
        assert_eq!(fragment.capacity(), 16);
        assert_eq!(fragment.used(), 0);
        assert_eq!(fragment.available(), 16);
        assert!(fragment.roots().is_empty());
        assert!(fragment.off_heap().is_empty());
        assert_eq!(fragment.off_heap_overhead_bytes(), 0);
    }

    #[test]
    fn copy_root_fills_the_region_and_records_the_root() {
        let mut src = Heap::new(16).expect("source heap");
        let term = build_list(&mut src, &[make_small(1), make_small(2), make_small(3)]);
        let src_off = OffHeapTable::new();

        let mut fragment = HeapFragment::new(16).expect("the fragment allocates");
        let root = fragment.copy_root(term, &src_off).expect("the copy fits");

        assert_eq!(fragment.used(), 6, "three cons cells");
        assert_eq!(fragment.available(), 10);
        assert_eq!(fragment.roots().len(), 1);
        assert_eq!(fragment.roots()[0].raw(), root.raw());
        assert!(fragment.contains(root), "the root lives in the fragment");
        assert!(!fragment.contains(term), "the source term is elsewhere");
        assert!(compare::eq(term, root, &AtomTable::new()));
    }

    #[test]
    fn contains_rejects_immediates_and_the_fragment_boundary() {
        let fragment = HeapFragment::new(4).expect("the fragment allocates");
        assert!(!fragment.contains(NIL));
        assert!(!fragment.contains(make_small(1)));
    }

    #[test]
    fn fragments_chain_reports_its_shape() {
        let mut chain = Fragments::new();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
        assert_eq!(chain.total_used_words(), 0);

        chain.push(HeapFragment::new(8).expect("a fragment"));
        chain.push(HeapFragment::new(16).expect("a fragment"));
        assert!(!chain.is_empty());
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.iter().count(), 2);
        assert_eq!(chain.total_used_words(), 0, "nothing written yet");

        let mut chain = Fragments::default();
        assert!(chain.is_empty());
        chain.push(HeapFragment::new(8).expect("a fragment"));
        assert_eq!(chain.len(), 1);
    }

    #[test]
    fn move_into_copies_the_root_onto_the_destination_heap() {
        let mut src = Heap::new(32).expect("source heap");
        let term = build_list(&mut src, &[make_small(10), make_small(20)]);
        let src_off = OffHeapTable::new();

        let mut fragment = HeapFragment::new(32).expect("the fragment allocates");
        fragment.copy_root(term, &src_off).expect("the copy fits");
        let mut chain = Fragments::new();
        chain.push(fragment);

        let mut dst = Heap::new(32).expect("destination heap");
        let mut dst_off = OffHeapTable::new();
        let moved = chain.move_into(&mut dst, &mut dst_off).expect("the move succeeds");

        assert_eq!(moved.len(), 1);
        assert_eq!(dst.used(), 4, "two cons cells on the destination heap");
        assert!(compare::eq(term, moved[0], &AtomTable::new()));
        assert!(
            !chain.iter().next().expect("one fragment").contains(moved[0]),
            "the moved root lives on the destination heap, not the fragment"
        );
    }

    #[test]
    fn move_into_transfers_the_off_heap_objects() {
        let mut src_off = OffHeapTable::new();
        src_off.push(Mso::new(0xABCdu32, 8));

        let mut src = Heap::new(16).expect("source heap");
        // SAFETY: the heap owns these three words (the off-heap reference container).
        let container = unsafe {
            let p = src.alloc_words(3).expect("the container fits");
            *p = HEADER_BIN_REF;
            *p.add(1) = Eterm::from_raw(0);
            *p.add(2) = Eterm::from_raw(0);
            make_boxed(p as *const Eterm)
        };

        let mut fragment = HeapFragment::new(16).expect("the fragment allocates");
        fragment
            .copy_root(container, &src_off)
            .expect("the copy fits");
        assert_eq!(fragment.off_heap().len(), 1);
        assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + fragment");

        let mut chain = Fragments::new();
        chain.push(fragment);

        let mut dst = Heap::new(16).expect("destination heap");
        let mut dst_off = OffHeapTable::new();
        let moved = chain.move_into(&mut dst, &mut dst_off).expect("the move succeeds");

        assert_eq!(dst_off.len(), 1, "the object moved into the destination table");
        assert_eq!(dst_off.get(0).and_then(|h| h.downcast_ref::<u32>()), Some(&0xABCD));
        assert!(
            chain.iter().next().expect("one fragment").off_heap().is_empty(),
            "the fragment gave up its references"
        );
        assert_eq!(src_off.get(0).map(MsoHandle::refcount), Some(2), "source + destination");
        // The moved container references the destination slot (the copy re-registers it).
        assert!(layout::is_off_heap_term(moved[0]));
        assert_eq!(layout::off_heap_slot(moved[0]), 0);
    }
}
