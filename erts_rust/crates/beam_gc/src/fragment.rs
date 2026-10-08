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

//! Heap fragments — off-heap heap areas for terms built *outside* a process heap — and the
//! collector-side primitives that move them into one.
//!
//! # Why fragments exist
//!
//! A term graph is normally built inside a process heap ([`HeapSpace`]). Sometimes it cannot
//! be: an incoming message is decoded before its destination process is known, a large BIF
//! result is assembled outside the caller's heap, a NIF returns a structure spanning several
//! allocations. The C runtime builds such graphs in an [`ErlHeapFragment`]
//! (`erts/emulator/beam/erl_message.h:198-205`): a bumped region of heap words with its own
//! off-heap list and a `next` link, chained from the process' `mbuf`/`live_hf_end` fields
//! (`erl_process.h:1152-1153`). [`HeapFragment`]/[`FragmentChain`] are that structure, and the
//! collector-side half of the story is exactly what C spells out in
//! `collect_live_heap_frags`/`move_one_area` (`erl_gc.c:2455-2511`), `erts_copy_one_frag`
//! (`erl_gc.c:2513-...`) and `erts_move_multi_frags`/`move_one_frag` (`copy.c:2044-2111`).
//!
//! The words of a fragment live **outside every process space**, so a collection cannot
//! evacuate them by pointer rewriting: the flat root-set contract ([`crate::copy::Copier`] takes
//! `roots: &mut [Eterm]` and a [`crate::FromSet`] of process spaces) leaves every fragment
//! reference classified `Foreign` (see `crate::copy::Target`) verbatim. The faithful C answer —
//! and the one implemented here — is therefore **copy-in**: the fragment's live terms are copied
//! into the process heap first (this module's [`HeapFragment::copy_term`] /
//! [`FragmentChain::copy_roots_into`]), and the relocated root words then travel through the
//! ordinary flat root slice. See [the rootset contract](#the-fragment-rootset-contract) below.
//!
//! # The copy/move semantics
//!
//! [`HeapFragment::copy_term`] copies a fragment-resident term — with its whole reachable graph —
//! into a target [`HeapSpace`], returning the **relocated root**:
//!
//! * an immediate, a literal-area term, a term already in the destination or a term in any other
//!   space is *shared*: it is returned unchanged (nothing is duplicated);
//! * a term that points **into the source** is copied, its referenced objects included, so the
//!   copy never points back into the fragment;
//! * the copy is **non-destructive** (the fragment's words are only read) and **iterative** (an
//!   explicit worklist — a deep list never recurses on the native stack);
//! * shared sub-terms are copied once (an explicit memo keyed on the source address).
//!
//! Per-object sizing and child enumeration are **not** re-implemented here: they are `move_size`
//! and `for_each_ref` from [`crate::copy`], the very primitives the Cheney copier and the Immix
//! mark-region space share. That keeps the frozen layout rules (fun inlined descriptors,
//! flatmap/HAMT nodes, `HEADER_SUB_BITS`'s `orig`) in exactly one place.
//!
//! Off-heap objects (a refc binary's payload, an MSO) are **never** walked: the collector's rule
//! is that an off-heap pointer is copied verbatim and liveness is the caller's concern. What a
//! copy *must* do is settle **ownership**: [`HeapFragment::copy_term`] shares the payload (the
//! fragment keeps owning it) and [`HeapFragment::copy_term_into`] **transfers** the fragment's
//! whole off-heap list to the destination's [`OffHeap`] (the C `OH_OVERHEAD(off_heap,
//! bp->off_heap.overhead)` transfer of `erts_copy_one_frag`), bumping each handle's refcount into
//! the destination and releasing the fragment's own reference — net refcount unchanged.
//!
//! # The fragment rootset contract
//!
//! The C collector roots fragments *inside* the collection (`Process.mbuf` .. `live_hf_end` is a
//! third rootset next to `x[0..live)` and the stack, `do_minor`/`major_collection`). This crate
//! takes a **flat `&mut [Eterm]` root slice** and a [`crate::FromSet`] of process spaces, so a
//! fragment pointer cannot be relocated by the collector — it is classified `Foreign` and left
//! verbatim. The faithful, documented approach is therefore:
//!
//! 1. **copy in** — [`FragmentRoots::materialize`] (or [`FragmentChain::copy_roots_into`] /
//!    [`HeapFragment::copy_term`]) copies every fragment-resident root into the process heap,
//!    rewriting the caller's root slot **in place** (exactly the C `move_one_area`'s
//!    "relocate the word that named the fragment object"); and
//! 2. **then collect** — the (now heap-resident) root words are gathered into the ordinary flat
//!    root slice ([`FragmentRoots::gather_roots`]) and handed to [`crate::collect`], which
//!    relocates them like any other root ([`FragmentRoots::scatter_roots`] writes them back).
//!
//! After step 1 nothing points into the fragment, so step 2 is a completely ordinary collection.
//! This is `collect_live_heap_frags` split into an explicit, caller-driven pair of steps.
//!
//! # Accounting
//!
//! [`FragmentCounters`] is a clonable, shared accounting handle: fragment create/drop counts,
//! the words handed out, and a live off-heap-byte gauge. A test (or the runtime) proves "no leak"
//! by observing `live() == 0`, `off_heap_bytes() == 0` and a backing allocator's
//! `AllocStats::live_blocks == 0` after dropping everything.
//!
//! # `unsafe`
//!
//! This module is an explicitly-marked word-level module: the copy primitives read the source
//! fragment's words, reserve destination words and hand both to [`crate::copy::for_each_ref`].
//! Every `unsafe` block carries a `// SAFETY:` rationale and every `unsafe fn` a `# Safety`
//! section; there is no `unwrap`/panic on runtime data.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use beam_alloc::{AllocRef, Heap};
use beam_common::Result;
use beam_term::prelude::{
    boxed_val, is_boxed, is_list, list_val, make_boxed, make_list, primary_tag, ptr_val, Eterm,
    PrimaryTag,
};

use crate::copy::{for_each_ref, move_size};
use crate::error::heap_exhausted;
use crate::mso::{OffHeap, OffHeapEntry};
use crate::space::HeapSpace;

// ---------------------------------------------------------------------------
// Fragment accounting
// ---------------------------------------------------------------------------

/// The mutable counters behind [`FragmentCounters`].
#[derive(Debug, Default)]
struct Counters {
    created: AtomicUsize,
    dropped: AtomicUsize,
    words: AtomicUsize,
    off_heap_bytes: AtomicUsize,
    off_heap_bytes_peak: AtomicUsize,
}

/// Shared, clonable accounting for heap fragments.
///
/// Fragments created with the same handle report into the same counters, so a caller (or a test)
/// can watch a whole chain — or a whole process' fragment traffic — with one handle, and prove
/// there is no leak: after every fragment is dropped, [`live`](FragmentCounters::live) and
/// [`off_heap_bytes`](FragmentCounters::off_heap_bytes) are back to `0`.
///
/// The counters are *observability only*: they never influence an allocation decision and every
/// method is total.
#[derive(Clone, Debug)]
pub struct FragmentCounters {
    inner: Arc<Counters>,
}

impl FragmentCounters {
    /// A fresh, zeroed counter set.
    pub fn new() -> FragmentCounters {
        FragmentCounters {
            inner: Arc::new(Counters::default()),
        }
    }

    /// Fragments created under this handle.
    pub fn created(&self) -> usize {
        self.inner.created.load(Ordering::Relaxed)
    }

    /// Fragments dropped under this handle.
    pub fn dropped(&self) -> usize {
        self.inner.dropped.load(Ordering::Relaxed)
    }

    /// Fragments currently alive (`created - dropped`).
    ///
    /// Never underflows: `dropped` is only ever incremented for a fragment that was counted at
    /// creation.
    pub fn live(&self) -> usize {
        self.created() - self.dropped()
    }

    /// The total capacity words handed to fragments created under this handle (cumulative).
    pub fn words(&self) -> usize {
        self.inner.words.load(Ordering::Relaxed)
    }

    /// The off-heap bytes the live fragments currently account for (a *current gauge*, not a
    /// cumulative total): every byte registered with
    /// [`HeapFragment::register_off_heap`] minus every byte released by
    /// [`HeapFragment::sweep_off_heap`], [`HeapFragment::move_off_heap_into`] or the fragment's
    /// own `Drop`.
    pub fn off_heap_bytes(&self) -> usize {
        self.inner.off_heap_bytes.load(Ordering::Relaxed)
    }

    /// The high-water mark of [`off_heap_bytes`](FragmentCounters::off_heap_bytes), in bytes.
    pub fn off_heap_bytes_peak(&self) -> usize {
        self.inner.off_heap_bytes_peak.load(Ordering::Relaxed)
    }

    /// Records a new fragment of `words` capacity words.
    fn note_created(&self, words: usize) {
        self.inner.created.fetch_add(1, Ordering::Relaxed);
        self.inner.words.fetch_add(words, Ordering::Relaxed);
    }

    /// Records a dropped fragment.
    fn note_dropped(&self) {
        self.inner.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Records `bytes` of off-heap payload entering a fragment's list.
    fn note_off_heap(&self, bytes: usize) {
        let live = self
            .inner
            .off_heap_bytes
            .fetch_add(bytes, Ordering::Relaxed)
            .saturating_add(bytes);
        self.inner
            .off_heap_bytes_peak
            .fetch_max(live, Ordering::Relaxed);
    }

    /// Records `bytes` of off-heap payload leaving a fragment's list (released or transferred).
    ///
    /// Saturating: a mismatch in the caller's bookkeeping can never wrap the gauge to a huge
    /// value.
    fn release_off_heap(&self, bytes: usize) {
        let _ = self.inner.off_heap_bytes.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |current| Some(current.saturating_sub(bytes)),
        );
    }
}

impl Default for FragmentCounters {
    fn default() -> FragmentCounters {
        FragmentCounters::new()
    }
}

// ---------------------------------------------------------------------------
// The fragment rootset hook
// ---------------------------------------------------------------------------

/// The caller's fragment-resident root slots.
///
/// See the module-level [fragment rootset contract](self#the-fragment-rootset-contract): the
/// collector cannot relocate a fragment pointer, so a fragment root must be **materialized** into
/// the process heap first ([`materialize`](FragmentRoots::materialize), which rewrites the slot in
/// place) and then gathered into the collector's flat root slice
/// ([`gather_roots`](FragmentRoots::gather_roots)).
///
/// The shape mirrors `beam_process::SavedArgs` (the other additive sub-rootset container): a flat
/// `Vec<Eterm>` with a `gather`/`scatter` pair, so a caller composes both the same way.
#[derive(Debug, Default, Clone)]
pub struct FragmentRoots {
    slots: Vec<Eterm>,
}

impl FragmentRoots {
    /// An empty root list.
    pub fn new() -> FragmentRoots {
        FragmentRoots { slots: Vec::new() }
    }

    /// An empty root list with room for `capacity` slots.
    pub fn with_capacity(capacity: usize) -> FragmentRoots {
        FragmentRoots {
            slots: Vec::with_capacity(capacity),
        }
    }

    /// The number of root slots.
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether there are no root slots.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Drops every root slot.
    pub fn clear(&mut self) {
        self.slots.clear();
    }

    /// Adds one root slot.
    pub fn push(&mut self, term: Eterm) {
        self.slots.push(term);
    }

    /// The root slots, in order.
    pub fn as_slice(&self) -> &[Eterm] {
        &self.slots
    }

    /// The root slots, in order, mutably.
    pub fn as_mut_slice(&mut self) -> &mut [Eterm] {
        &mut self.slots
    }

    /// Copies every slot that points into `chain` into the process heap `to` — rewriting the slot
    /// **in place** with the relocated root — and moves `chain`'s off-heap lists into
    /// `dst_off_heap`. Returns how many slots were relocated.
    ///
    /// This is step 1 of the rootset contract. After it returns, no slot points into `chain`, so
    /// the slots may be gathered ([`gather_roots`](FragmentRoots::gather_roots)) into the
    /// collector's flat root slice.
    pub fn materialize(
        &mut self,
        chain: &mut FragmentChain,
        to: &mut HeapSpace,
        dst_off_heap: &mut OffHeap,
    ) -> Result<usize> {
        chain.copy_roots_into(&mut self.slots, to, dst_off_heap)
    }

    /// Appends every slot to `out` and returns the **start offset** — the index in `out` at which
    /// this container's slots begin.
    ///
    /// Pairing with [`scatter_roots`](FragmentRoots::scatter_roots) with the same `start` makes
    /// the two a clean round-trip around a collection.
    pub fn gather_roots(&self, out: &mut Vec<Eterm>) -> usize {
        let start = out.len();
        out.extend_from_slice(&self.slots);
        start
    }

    /// Writes the (relocated) slots back, starting at `start` — the offset returned by
    /// [`gather_roots`](FragmentRoots::gather_roots).
    ///
    /// Defensive and total: a slice too short to cover the whole list, or a `start` past its end,
    /// simply leaves the uncovered tail untouched (no panic).
    pub fn scatter_roots(&mut self, roots: &[Eterm], start: usize) {
        let Some(available) = roots.len().checked_sub(start) else {
            return;
        };
        let count = available.min(self.slots.len());
        self.slots[..count].copy_from_slice(&roots[start..start + count]);
    }
}

// ---------------------------------------------------------------------------
// HeapFragment
// ---------------------------------------------------------------------------

/// One heap fragment: a bump-allocated region of term words **outside** any process
/// [`HeapSpace`], its own off-heap list, and its accounting.
///
/// The words are an ordinary [`beam_alloc::Heap`] (the production bump arena — no new allocator),
/// so term construction goes through [`heap_mut`](HeapFragment::heap_mut) exactly as it does for a
/// process space. The terms written into it are **immutable**, as everywhere in ERTS.
///
/// The off-heap list is the workspace's single [`OffHeap`] container, so a refc binary's payload
/// or an MSO referenced from the fragment is owned here until it is swept
/// ([`sweep_off_heap`](HeapFragment::sweep_off_heap)) or transferred
/// ([`move_off_heap_into`](HeapFragment::move_off_heap_into)). Register entries through
/// [`register_off_heap`](HeapFragment::register_off_heap) so the byte gauge stays exact.
pub struct HeapFragment {
    heap: Heap,
    off_heap: OffHeap,
    counters: FragmentCounters,
}

impl HeapFragment {
    /// A fragment of `capacity_words` term words, accounted to a fresh counter set and backed by
    /// the default allocator.
    pub fn new(capacity_words: usize) -> Result<HeapFragment> {
        HeapFragment::with_allocator_and_counters(
            beam_alloc::default_allocator().clone(),
            FragmentCounters::new(),
            capacity_words,
        )
    }

    /// A fragment of `capacity_words` term words backed by `alloc`, accounted to a fresh counter
    /// set.
    pub fn with_allocator(alloc: AllocRef, capacity_words: usize) -> Result<HeapFragment> {
        HeapFragment::with_allocator_and_counters(
            alloc,
            FragmentCounters::new(),
            capacity_words,
        )
    }

    /// A fragment of `capacity_words` term words accounted to `counters`, backed by the default
    /// allocator.
    pub fn with_counters(
        counters: FragmentCounters,
        capacity_words: usize,
    ) -> Result<HeapFragment> {
        HeapFragment::with_allocator_and_counters(
            beam_alloc::default_allocator().clone(),
            counters,
            capacity_words,
        )
    }

    /// A fragment of `capacity_words` term words backed by `alloc` and accounted to `counters`.
    pub fn with_allocator_and_counters(
        alloc: AllocRef,
        counters: FragmentCounters,
        capacity_words: usize,
    ) -> Result<HeapFragment> {
        let heap = Heap::with_allocator(alloc, capacity_words)?;
        counters.note_created(capacity_words);
        Ok(HeapFragment {
            heap,
            off_heap: OffHeap::new(),
            counters,
        })
    }

    /// The accounting handle this fragment reports into.
    pub fn counters(&self) -> FragmentCounters {
        self.counters.clone()
    }

    // -- word-area geometry (the `ErlHeapFragment.mem`/`alloc_size`/`used_size` fields) --------

    /// Total capacity in term words (`alloc_size`).
    pub fn capacity_words(&self) -> usize {
        self.heap.capacity()
    }

    /// Words written so far (`used_size`).
    pub fn used_words(&self) -> usize {
        self.heap.used()
    }

    /// Words still available.
    pub fn available_words(&self) -> usize {
        self.heap.available()
    }

    /// The first word of the fragment's area.
    pub fn base(&self) -> *mut Eterm {
        // `hend` is `base + capacity`, so subtracting `capacity` words recovers `base` exactly
        // (including for a zero-capacity fragment, where the two coincide). `wrapping_sub` on a
        // raw pointer computes an address arithmetically, without dereferencing anything.
        self.heap.hend().wrapping_sub(self.heap.capacity())
    }

    /// The current allocation pointer (`hp`).
    pub fn top(&self) -> *mut Eterm {
        self.heap.hp()
    }

    /// One past the last word of the fragment (`hend`).
    pub fn limit(&self) -> *mut Eterm {
        self.heap.hend()
    }

    /// Whether `ptr` addresses a word of the fragment's **live** region `[base, top)`.
    pub fn contains(&self, ptr: *const Eterm) -> bool {
        let addr = ptr as usize;
        let base = self.base() as usize;
        let top = self.top() as usize;
        base <= addr && addr < top
    }

    /// Whether the boxed/list term `term` refers into this fragment.
    pub fn contains_term(&self, term: Eterm) -> bool {
        (is_boxed(term) || is_list(term)) && self.contains(ptr_val(term) as *const Eterm)
    }

    /// Term construction for this fragment.
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// Term construction for this fragment, mutably.
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    // -- the off-heap list ---------------------------------------------------------------------

    /// The fragment's off-heap list (read-only).
    ///
    /// Mutate it through [`register_off_heap`](HeapFragment::register_off_heap) /
    /// [`sweep_off_heap`](HeapFragment::sweep_off_heap) /
    /// [`move_off_heap_into`](HeapFragment::move_off_heap_into) so the byte accounting stays
    /// exact.
    pub fn off_heap(&self) -> &OffHeap {
        &self.off_heap
    }

    /// The number of off-heap objects the fragment owns.
    pub fn off_heap_len(&self) -> usize {
        self.off_heap.len()
    }

    /// The apparent (off-heap payload) bytes the fragment owns.
    pub fn off_heap_bytes(&self) -> usize {
        self.off_heap.overhead_bytes()
    }

    /// Registers an off-heap object (an [`beam_alloc::RefcBinary`] or an MSO handle) and returns
    /// the slot an on-heap off-heap reference stores in its `val` word.
    ///
    /// Mirrors `beam_message::OffHeapTable::push`: slots are append-only and stable until the
    /// list is swept or transferred.
    pub fn register_off_heap(&mut self, entry: impl Into<OffHeapEntry>) -> usize {
        let entry = entry.into();
        let bytes = entry.overhead_bytes();
        let slot = self.off_heap.insert(entry);
        self.counters.note_off_heap(bytes);
        slot
    }

    /// Retains the off-heap entries for which `is_live` returns `true`, releasing the rest, and
    /// returns how many were released.
    ///
    /// Liveness is the **caller's** concern (the collector never walks off-heap payloads): the
    /// caller inspects the on-heap references it kept and answers for each entry.
    pub fn sweep_off_heap<F>(&mut self, is_live: F) -> usize
    where
        F: FnMut(&OffHeapEntry) -> bool,
    {
        let before = self.off_heap.overhead_bytes();
        let released = self.off_heap.sweep(is_live);
        let after = self.off_heap.overhead_bytes();
        self.counters.release_off_heap(before - after);
        released
    }

    /// **Transfers** the fragment's whole off-heap list into `dst` and returns how many entries
    /// moved.
    ///
    /// Every handle is cloned into `dst` (refcount++) and the fragment's own reference is then
    /// released (refcount--), so the net refcount of each payload is unchanged: ownership simply
    /// moves. This is the C `erts_copy_one_frag` transfer (`OH_OVERHEAD(off_heap,
    /// bp->off_heap.overhead)`; `erl_gc.c:2523`).
    ///
    /// Entries land in `dst` in slot order, so the entry that was at fragment slot `s` is at
    /// destination slot `dst.len()` (measured *before* the call) `+ s`.
    pub fn move_off_heap_into(&mut self, dst: &mut OffHeap) -> usize {
        let count = self.off_heap.len();
        let bytes = self.off_heap.overhead_bytes();
        for entry in self.off_heap.iter() {
            match entry {
                OffHeapEntry::Mso(handle) => dst.push(handle.clone()),
                OffHeapEntry::Binary(binary) => dst.push(binary.clone()),
            }
        }
        // Release the fragment's own references: the destination now owns them, so the net
        // refcount of every payload is unchanged.
        self.off_heap.sweep(|_| false);
        self.counters.release_off_heap(bytes);
        count
    }

    // -- the fragment -> heap copy primitives --------------------------------------------------

    /// Copies `root` — and its whole reachable graph — from this fragment into the process heap
    /// `to`, returning the relocated root.
    ///
    /// * An immediate, a literal-area term, a term already in `to`, or any other term that does
    ///   not point into this fragment is returned **unchanged** (shared, never duplicated).
    /// * A term that points into this fragment, and everything reachable from it that also lives
    ///   in this fragment, is copied; the result never points back into the fragment.
    ///
    /// The copy is **non-destructive**: the fragment is only read, so it stays usable. It is also
    /// **iterative** (no native-stack recursion on a deep list).
    ///
    /// # Off-heap ownership
    ///
    /// An on-heap off-heap reference is copied verbatim, so the copy still names this fragment's
    /// off-heap payloads and the fragment keeps owning them. Use
    /// [`copy_term_into`](HeapFragment::copy_term_into) to hand that ownership to the
    /// destination instead (the `move` form the C uses when the fragment is about to be freed).
    ///
    /// # Cross-fragment graphs
    ///
    /// A graph that spans several fragments cannot be copied by a single fragment: a pointer into
    /// a *sibling* fragment is outside this fragment and is therefore shared verbatim (and would
    /// dangle once that sibling dies). Copy a multi-fragment graph through
    /// [`FragmentChain::copy_roots_into`], which treats the whole chain as the source.
    pub fn copy_term(&self, root: Eterm, to: &mut HeapSpace) -> Result<Eterm> {
        let contains = |ptr: *const Eterm| self.contains(ptr);
        GraphCopier::new(&contains, to).copy_root(root)
    }

    /// Copies every root in `roots` from this fragment into `to`, rewriting the slots in place,
    /// and returns how many slots were relocated.
    ///
    /// One memo spans the whole slice, so structure shared *between* roots is copied once — just
    /// as the C moves a fragment area object by object.
    pub fn copy_terms(&self, roots: &mut [Eterm], to: &mut HeapSpace) -> Result<usize> {
        let contains = |ptr: *const Eterm| self.contains(ptr);
        let mut copier = GraphCopier::new(&contains, to);
        let mut relocated = 0;
        for root in roots.iter_mut() {
            let copy = copier.copy_root(*root)?;
            if copy != *root {
                relocated += 1;
            }
            *root = copy;
        }
        Ok(relocated)
    }

    /// [`copy_term`](HeapFragment::copy_term), plus a **transfer** of this fragment's whole
    /// off-heap list to `dst_off_heap` — the "move the fragment into the heap" primitive of the C
    /// `erts_copy_one_frag`.
    ///
    /// After the call the fragment owns no off-heap objects, and the destination owns what the
    /// copied graph references, so the fragment may be dropped without freeing a live payload.
    pub fn copy_term_into(
        &mut self,
        root: Eterm,
        to: &mut HeapSpace,
        dst_off_heap: &mut OffHeap,
    ) -> Result<Eterm> {
        let relocated = self.copy_term(root, to)?;
        self.move_off_heap_into(dst_off_heap);
        Ok(relocated)
    }
}

impl Drop for HeapFragment {
    fn drop(&mut self) {
        // The list is about to drop, releasing whatever it still owns: give the accounting back
        // first (the gauge must not keep bytes the fragment no longer holds).
        self.counters.release_off_heap(self.off_heap.overhead_bytes());
        self.counters.note_dropped();
    }
}

// ---------------------------------------------------------------------------
// FragmentChain
// ---------------------------------------------------------------------------

/// A chain of fragments — the C `ErlHeapFragment.next` linked list (`Process.mbuf` /
/// `live_hf_end`, `erl_process.h:1152-1153`), used for a message or result that streams across
/// several areas.
///
/// The chain is an ordered, contiguous container (insertion order is the `next` order); `first`
/// and `last` are the C list head and tail. It owns its fragments, so dropping the chain drops
/// them all.
///
/// The chain's **off-heap list** is the union of its members' lists: the C stores the
/// multi-fragment list in the first fragment (`factory->off_heap = &bp->off_heap`,
/// `erts_message.c:1206`) and `erts_move_multi_frags` walks every fragment; here each fragment
/// owns its own list and the chain presents the aggregate/transfer view, which is the same set of
/// objects.
pub struct FragmentChain {
    fragments: Vec<HeapFragment>,
}

impl FragmentChain {
    /// An empty chain.
    pub fn new() -> FragmentChain {
        FragmentChain {
            fragments: Vec::new(),
        }
    }

    /// Appends `fragment` (the C `bp->next = p->mbuf` link).
    pub fn push(&mut self, fragment: HeapFragment) {
        self.fragments.push(fragment);
    }

    /// The number of fragments in the chain.
    pub fn len(&self) -> usize {
        self.fragments.len()
    }

    /// Whether the chain holds no fragments.
    pub fn is_empty(&self) -> bool {
        self.fragments.is_empty()
    }

    /// Drops every fragment.
    pub fn clear(&mut self) {
        self.fragments.clear();
    }

    /// The first fragment (the C list head).
    pub fn first(&self) -> Option<&HeapFragment> {
        self.fragments.first()
    }

    /// The last fragment (the C list tail).
    pub fn last(&self) -> Option<&HeapFragment> {
        self.fragments.last()
    }

    /// The fragment at `index`.
    pub fn get(&self, index: usize) -> Option<&HeapFragment> {
        self.fragments.get(index)
    }

    /// The fragment at `index`, mutably.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut HeapFragment> {
        self.fragments.get_mut(index)
    }

    /// The fragments, in chain order.
    pub fn iter(&self) -> impl Iterator<Item = &HeapFragment> {
        self.fragments.iter()
    }

    /// The fragments, in chain order, mutably.
    pub fn iter_mut(&mut self) -> impl Iterator<Item = &mut HeapFragment> {
        self.fragments.iter_mut()
    }

    /// The total words written across every fragment.
    pub fn used_words(&self) -> usize {
        self.fragments.iter().map(HeapFragment::used_words).sum()
    }

    /// The total capacity across every fragment.
    pub fn capacity_words(&self) -> usize {
        self.fragments
            .iter()
            .map(HeapFragment::capacity_words)
            .sum()
    }

    /// The number of off-heap objects the chain owns.
    pub fn off_heap_len(&self) -> usize {
        self.fragments.iter().map(HeapFragment::off_heap_len).sum()
    }

    /// The off-heap payload bytes the chain owns.
    pub fn off_heap_bytes(&self) -> usize {
        self.fragments
            .iter()
            .map(HeapFragment::off_heap_bytes)
            .sum()
    }

    /// Every off-heap entry of the chain, in fragment-then-slot order.
    pub fn off_heap_iter(&self) -> impl Iterator<Item = &OffHeapEntry> {
        self.fragments
            .iter()
            .flat_map(|fragment| fragment.off_heap().iter())
    }

    /// Whether `ptr` addresses a word of any fragment of the chain.
    pub fn contains(&self, ptr: *const Eterm) -> bool {
        self.fragments.iter().any(|fragment| fragment.contains(ptr))
    }

    /// Whether the boxed/list term `term` refers into any fragment of the chain.
    pub fn contains_term(&self, term: Eterm) -> bool {
        self.fragments
            .iter()
            .any(|fragment| fragment.contains_term(term))
    }

    /// Transfers every fragment's off-heap list into `dst` (see
    /// [`HeapFragment::move_off_heap_into`]) and returns how many entries moved.
    pub fn move_off_heap_into(&mut self, dst: &mut OffHeap) -> usize {
        self.fragments
            .iter_mut()
            .map(|fragment| fragment.move_off_heap_into(dst))
            .sum()
    }

    /// Copies every root in `roots` that points into **any** fragment of this chain into the
    /// process heap `to`, rewriting each such slot in place, and returns how many slots were
    /// relocated.
    ///
    /// This is the chain-level copy-in of the [rootset contract](self#the-fragment-rootset-contract)
    /// (`erts_move_multi_frags`): the whole chain is the source, so a graph spanning fragments is
    /// copied correctly and its sharing is preserved (one memo spans the whole call).
    ///
    /// When at least one slot was relocated the chain's off-heap lists are **transferred** to
    /// `dst_off_heap` (the C transfer is unconditional on a used fragment; a chain whose terms
    /// were all shared owns nothing the destination needs, so nothing is moved then).
    pub fn copy_roots_into(
        &mut self,
        roots: &mut [Eterm],
        to: &mut HeapSpace,
        dst_off_heap: &mut OffHeap,
    ) -> Result<usize> {
        let relocated = {
            let contains = |ptr: *const Eterm| self.contains(ptr);
            let mut copier = GraphCopier::new(&contains, to);
            let mut relocated = 0;
            for root in roots.iter_mut() {
                let copy = copier.copy_root(*root)?;
                if copy != *root {
                    relocated += 1;
                }
                *root = copy;
            }
            relocated
        };
        if relocated > 0 {
            self.move_off_heap_into(dst_off_heap);
        }
        Ok(relocated)
    }
}

impl Default for FragmentChain {
    fn default() -> FragmentChain {
        FragmentChain::new()
    }
}

impl<'a> IntoIterator for &'a FragmentChain {
    type Item = &'a HeapFragment;
    type IntoIter = core::slice::Iter<'a, HeapFragment>;

    fn into_iter(self) -> Self::IntoIter {
        self.fragments.iter()
    }
}

impl<'a> IntoIterator for &'a mut FragmentChain {
    type Item = &'a mut HeapFragment;
    type IntoIter = core::slice::IterMut<'a, HeapFragment>;

    fn into_iter(self) -> Self::IntoIter {
        self.fragments.iter_mut()
    }
}

// ---------------------------------------------------------------------------
// The graph copier (the `unsafe` core)
// ---------------------------------------------------------------------------

/// The iterative, non-destructive term-graph copier used by every fragment copy primitive.
///
/// One copier = one memo (sharing) + one worklist (iteration). Object sizing
/// ([`move_size`]) and child enumeration ([`for_each_ref`]) are the shared word-level primitives
/// of [`crate::copy`], so the frozen layout rules live in exactly one place.
struct GraphCopier<'a> {
    /// "Is this pointer inside the source?" — one fragment's range, or a whole chain's.
    contains: &'a dyn Fn(*const Eterm) -> bool,
    /// The destination process heap.
    to: &'a mut HeapSpace,
    /// Source address -> already-produced destination term (sharing).
    memo: HashMap<usize, Eterm>,
    /// Destination objects whose reference words still hold source terms.
    work: Vec<(*mut Eterm, usize)>,
    objects: usize,
    words: usize,
}

impl<'a> GraphCopier<'a> {
    /// A copier from the source described by `contains` into `to`.
    fn new(contains: &'a dyn Fn(*const Eterm) -> bool, to: &'a mut HeapSpace) -> GraphCopier<'a> {
        GraphCopier {
            contains,
            to,
            memo: HashMap::new(),
            work: Vec::new(),
            objects: 0,
            words: 0,
        }
    }

    /// Copies `root` (and its whole reachable source graph) into the destination, returning the
    /// relocated root.
    ///
    /// Iterative: the worklist is drained to completion, so a deep list never recurses on the
    /// native stack.
    fn copy_root(&mut self, root: Eterm) -> Result<Eterm> {
        let relocated = self.copy_object(root)?;
        while let Some((dst, total)) = self.work.pop() {
            // SAFETY: `dst` addresses the `total`-word object this copier has just written into
            // `to` (every one of its words is initialised); `for_each_ref` only *visits* the
            // object's reference words, and the visitor rewrites exactly the word it is given.
            unsafe { for_each_ref(dst, total, &mut |slot| self.relocate_slot(slot)) }?;
        }
        Ok(relocated)
    }

    /// Rewrites the reference word at `slot` — a word inside an object this copier owns in the
    /// destination — so it points at that referenced object's copy.
    fn relocate_slot(&mut self, slot: *mut Eterm) -> Result<()> {
        // SAFETY: `for_each_ref` guarantees `slot` addresses a live, writable reference word of
        // an object this copier owns.
        let term = unsafe { *slot };
        let relocated = self.copy_object(term)?;
        // SAFETY: as above — the word is writable and stays a well-formed term.
        unsafe { *slot = relocated };
        Ok(())
    }

    /// Copies the single object `term` names (when it lives in the source), returning the
    /// destination term. Never recurses: a boxed object is queued for child fix-up, a cons cell's
    /// two words are themselves queued by [`copy_root`](GraphCopier::copy_root).
    fn copy_object(&mut self, term: Eterm) -> Result<Eterm> {
        match primary_tag(term) {
            // A bare header (only met inside an object the scan already sized) or an immediate is
            // not a pointer: nothing to copy.
            PrimaryTag::Immed1 | PrimaryTag::Header => Ok(term),
            PrimaryTag::Boxed => {
                let src = boxed_val(term) as *const Eterm;
                if !(self.contains)(src) {
                    // Literal area, another space, the destination itself: shared verbatim.
                    return Ok(term);
                }
                if let Some(&done) = self.memo.get(&(src as usize)) {
                    return Ok(done);
                }
                // SAFETY: `contains` established that `src` addresses the source area's words, so
                // the object's header word is live and readable.
                let header = unsafe { *src };
                let total = move_size(header, src);
                let dst = self.reserve(total)?;
                // SAFETY: `src` is a live `total`-word object (its own header sized it) and `dst`
                // is a distinct, exclusively owned `total`-word destination region.
                unsafe { core::ptr::copy_nonoverlapping(src, dst, total) };
                let relocated = make_boxed(dst as *const Eterm);
                self.finish_object(src as usize, relocated, dst, total);
                Ok(relocated)
            }
            PrimaryTag::List => {
                let src = list_val(term) as *const Eterm;
                if !(self.contains)(src) {
                    return Ok(term);
                }
                if let Some(&done) = self.memo.get(&(src as usize)) {
                    return Ok(done);
                }
                let dst = self.reserve(2)?;
                // SAFETY: `src` is a live 2-word cons cell of the source area and `dst` is a
                // distinct, exclusively owned 2-word destination region.
                unsafe {
                    *dst = *src;
                    *dst.add(1) = *src.add(1);
                }
                let relocated = make_list(dst as *const Eterm);
                self.finish_object(src as usize, relocated, dst, 2);
                Ok(relocated)
            }
        }
    }

    /// Reserves `words` writable destination words, or reports heap exhaustion.
    fn reserve(&mut self, words: usize) -> Result<*mut Eterm> {
        if self.to.available_words() < words {
            return Err(heap_exhausted(words));
        }
        // SAFETY: `words` fit (`available_words` was just checked), so `alloc_words` hands out
        // `words` exclusively owned, writable words at the destination's top.
        unsafe { self.to.heap_mut().alloc_words(words) }
    }

    /// Records a freshly copied object: memoise it (sharing), queue it for child fix-up and count
    /// it.
    fn finish_object(&mut self, key: usize, relocated: Eterm, dst: *mut Eterm, total: usize) {
        self.memo.insert(key, relocated);
        self.work.push((dst, total));
        self.objects += 1;
        self.words += total;
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the fragment's own bookkeeping: geometry/range tests, the accounting
    //! counters (including their saturation), and the chain's aggregate views. The copy-in
    //! evidence lives in `tests/fragments.rs`.

    use super::*;
    use beam_alloc::default_allocator;
    use beam_term::prelude::make_small;

    fn fragment(words: usize) -> HeapFragment {
        HeapFragment::with_allocator(default_allocator().clone(), words).expect("fragment")
    }

    #[test]
    fn geometry_and_range_tests_track_the_bump_pointer() {
        let mut frag = fragment(8);
        assert_eq!(frag.capacity_words(), 8);
        assert_eq!(frag.used_words(), 0);
        assert_eq!(frag.available_words(), 8);
        assert_eq!(frag.base(), frag.top());
        assert!(!frag.contains(frag.base()), "the live region is empty");

        let tuple = frag.heap_mut().tuple(&[make_small(1)]).expect("tuple");
        assert_eq!(frag.used_words(), 2);
        assert_eq!(frag.available_words(), 6);
        assert!(frag.contains_term(tuple));
        assert!(frag.contains(frag.base()));

        // The live range is half-open: `top()` itself is not part of it.
        assert!(!frag.contains(frag.top()));
        assert_eq!(frag.limit() as usize - frag.base() as usize, 8 * 8);

        // An immediate is never "in" a fragment; neither is a foreign heap word.
        assert!(!frag.contains_term(make_small(1)));
        let local = make_small(9);
        assert!(!frag.contains(&local as *const Eterm));
    }

    #[test]
    fn counters_track_create_drop_words_and_the_off_heap_gauge() {
        let counters = FragmentCounters::new();
        assert_eq!(counters.live(), 0);
        assert_eq!(counters.words(), 0);
        assert_eq!(counters.off_heap_bytes(), 0);

        {
            let mut frag = HeapFragment::with_counters(counters.clone(), 16).expect("fragment");
            assert_eq!(counters.created(), 1);
            assert_eq!(counters.dropped(), 0);
            assert_eq!(counters.live(), 1);
            assert_eq!(counters.words(), 16);

            frag.register_off_heap(crate::mso::Mso::new(7u32, 40));
            assert_eq!(counters.off_heap_bytes(), 40);
            assert_eq!(counters.off_heap_bytes_peak(), 40);
            assert_eq!(frag.off_heap_bytes(), 40);

            // Sweeping the entry releases its bytes.
            assert_eq!(frag.sweep_off_heap(|_| false), 1);
            assert_eq!(counters.off_heap_bytes(), 0);
            assert_eq!(counters.off_heap_bytes_peak(), 40);
        }

        assert_eq!(counters.dropped(), 1);
        assert_eq!(counters.live(), 0);
        assert_eq!(counters.off_heap_bytes(), 0);
    }

    #[test]
    fn releasing_off_heap_bytes_saturates_instead_of_wrapping() {
        let counters = FragmentCounters::new();
        counters.release_off_heap(1_000);
        assert_eq!(counters.off_heap_bytes(), 0, "the gauge never wraps");
        counters.note_off_heap(8);
        assert_eq!(counters.off_heap_bytes(), 8);
        counters.release_off_heap(1_000);
        assert_eq!(counters.off_heap_bytes(), 0);
    }

    #[test]
    fn chain_aggregates_its_fragments() {
        let counters = FragmentCounters::new();
        let mut chain = FragmentChain::new();
        assert!(chain.is_empty());
        assert_eq!(chain.off_heap_len(), 0);
        assert_eq!(chain.off_heap_bytes(), 0);

        let mut first = HeapFragment::with_counters(counters.clone(), 8).expect("fragment");
        first
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("tuple");
        first.register_off_heap(crate::mso::Mso::new(1u32, 12));
        let mut second = HeapFragment::with_counters(counters.clone(), 8).expect("fragment");
        second
            .heap_mut()
            .tuple(&[make_small(3)])
            .expect("tuple");
        second.register_off_heap(crate::mso::Mso::new(2u32, 20));

        chain.push(first);
        chain.push(second);
        assert_eq!(chain.len(), 2);
        assert!(!chain.is_empty());
        assert_eq!(chain.used_words(), 3 + 2);
        assert_eq!(chain.capacity_words(), 16);
        assert_eq!(chain.off_heap_len(), 2);
        assert_eq!(chain.off_heap_bytes(), 32);
        assert_eq!(chain.off_heap_iter().count(), 2);
        assert_eq!(counters.live(), 2);
        assert_eq!(counters.off_heap_bytes(), 32);

        // `first`/`last` are the list head/tail.
        assert_eq!(chain.first().map(HeapFragment::used_words), Some(3));
        assert_eq!(chain.last().map(HeapFragment::used_words), Some(2));
        assert!(chain.get(2).is_none(), "out of range");

        chain.clear();
        assert!(chain.is_empty());
        assert_eq!(counters.live(), 0);
        assert_eq!(counters.off_heap_bytes(), 0);
    }

    #[test]
    fn fragment_roots_gather_and_scatter_round_trip() {
        let mut roots = FragmentRoots::new();
        assert!(roots.is_empty());
        roots.push(make_small(1));
        roots.push(make_small(2));
        assert_eq!(roots.len(), 2);
        assert_eq!(roots.as_slice(), &[make_small(1), make_small(2)]);

        let mut flat = vec![make_small(-1)];
        let start = roots.gather_roots(&mut flat);
        assert_eq!(start, 1);
        assert_eq!(&flat[1..], &[make_small(1), make_small(2)]);

        flat[1] = make_small(11);
        roots.scatter_roots(&flat, start);
        assert_eq!(roots.as_slice(), &[make_small(11), make_small(2)]);

        // A short slice and an out-of-range start are total.
        roots.scatter_roots(&[make_small(9)], 0);
        assert_eq!(roots.as_slice(), &[make_small(9), make_small(2)]);
        roots.scatter_roots(&[make_small(9)], 99);
        assert_eq!(roots.as_slice(), &[make_small(9), make_small(2)]);

        roots.as_mut_slice()[1] = make_small(7);
        assert_eq!(roots.as_slice(), &[make_small(9), make_small(7)]);
        roots.clear();
        assert!(roots.is_empty());
    }
}
