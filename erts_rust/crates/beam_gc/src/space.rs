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

//! Copying semi-spaces: [`HeapSpace`] (a thin space-semantics wrapper over
//! `beam_alloc::Heap`) and [`FromSet`] (the set of ranges a collection evacuates).
//!
//! `HeapSpace` never reallocates — growth happens by GC *replacement*, not by moving
//! live terms (workspace `CONTEXT.md` §2). `unsafe` here is confined to the
//! raw-pointer range tests.

use beam_alloc::{AllocRef, Heap};
use beam_common::Result;
use beam_term::prelude::{is_boxed, is_list, ptr_val, Eterm};

/// One copying semi-space of the per-process heap.
///
/// Adds *space* semantics (range tests, [`reset`](HeapSpace::reset), base/top/limit)
/// to the production bump heap [`beam_alloc::Heap`]; term construction goes through
/// [`heap_mut`](HeapSpace::heap_mut).
pub struct HeapSpace {
    heap: Heap,
}

impl HeapSpace {
    /// Creates an empty semi-space of `capacity_words` term words, backed by the
    /// default allocator.
    pub fn new(capacity_words: usize) -> Result<HeapSpace> {
        Ok(HeapSpace {
            heap: Heap::new(capacity_words)?,
        })
    }

    /// Creates an empty semi-space of `capacity_words` term words, backed by `alloc`.
    pub fn with_allocator(alloc: AllocRef, capacity_words: usize) -> Result<HeapSpace> {
        Ok(HeapSpace {
            heap: Heap::with_allocator(alloc, capacity_words)?,
        })
    }

    /// Wraps an existing [`beam_alloc::Heap`] as a semi-space.
    pub fn from_heap(heap: Heap) -> HeapSpace {
        HeapSpace { heap }
    }

    /// Unwraps this semi-space, returning the underlying bump heap.
    pub fn into_heap(self) -> Heap {
        self.heap
    }

    /// Total capacity in term words.
    pub fn capacity_words(&self) -> usize {
        self.heap.capacity()
    }

    /// Words handed out so far (the bump-pointer offset).
    pub fn used_words(&self) -> usize {
        self.heap.used()
    }

    /// Words still available in this space.
    pub fn available_words(&self) -> usize {
        self.heap.available()
    }

    /// The first word of the space.
    pub fn base(&self) -> *mut Eterm {
        // `hend` is `base + capacity`, so subtracting `capacity` words recovers
        // `base` exactly (including for a zero-capacity space, where the two
        // coincide). `wrapping_sub` on a raw pointer is safe: it computes an
        // address arithmetically without dereferencing anything.
        self.heap.hend().wrapping_sub(self.heap.capacity())
    }

    /// The current allocation pointer (`hp`, = `base + used_words`).
    pub fn top(&self) -> *mut Eterm {
        self.heap.hp()
    }

    /// One past the last word of the space (`hend`).
    pub fn limit(&self) -> *mut Eterm {
        self.heap.hend()
    }

    /// Rewinds the bump pointer, invalidating every previously handed-out term.
    pub fn reset(&mut self) {
        self.heap.reset();
    }

    /// Whether `ptr` points into the live region `[base, top)` of this space.
    pub fn contains(&self, ptr: *const Eterm) -> bool {
        let addr = ptr as usize;
        let base = self.base() as usize;
        let top = self.top() as usize;
        base <= addr && addr < top
    }

    /// Whether the boxed/list term `term` refers into this space.
    pub fn contains_term(&self, term: Eterm) -> bool {
        (is_boxed(term) || is_list(term)) && self.contains(ptr_val(term) as *const Eterm)
    }

    /// The underlying bump heap (term construction for this space).
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The underlying bump heap, mutably.
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The allocator backing this space's heap.
    pub fn allocator(&self) -> &AllocRef {
        self.heap.allocator()
    }
}

/// The set of from-space ranges a collection evacuates.
///
/// One range for a minor collection (the nursery); two — nursery plus mature — for a
/// fullsweep collection. A reference that falls outside every range is left verbatim
/// by the copier (the C `erts_is_literal`/area-test rule).
///
/// The ranges are stored **sorted by `lo`**, so the per-reference membership test
/// ([`contains`](FromSet::contains)) and the reachability walk's bit-index lookup
/// ([`locate`](FromSet::locate)) narrow to the single candidate range by binary search instead
/// of scanning every range — both are per-reference hot paths, and a chained nursery or a
/// partition into many ranges must not make a reference word cost more than one reference word.
///
/// Insertion (`add`/`add_range`) pays the `O(#ranges)` shift of an insertion into a sorted
/// `Vec`, which is negligible: a from-set holds one range per non-empty space of the
/// collection's heap ([`HeapSpace`] never replicates its allocation), not one per object.
pub struct FromSet {
    /// The evacuated ranges, half-open byte-address pairs `[lo, hi)`, kept **sorted by `lo`**
    /// (a later `add` of a smaller `lo` is inserted before an earlier one).
    ranges: Vec<(usize, usize)>,
}

impl FromSet {
    /// An empty set of ranges.
    pub fn new() -> FromSet {
        FromSet { ranges: Vec::new() }
    }

    /// Adds the live region of `space` as a range this set evacuates.
    pub fn add(&mut self, space: &HeapSpace) {
        self.insert(space.base() as usize, space.top() as usize);
    }

    /// Adds an explicit half-open byte-address range `[lo, hi)` this set evacuates.
    ///
    /// Used for a space whose "live region" is not its `[base, top)` fill — the mark-region
    /// mature space is one contiguous region whose free lines are inside it too, so a
    /// [`FromSet`] over it is `base .. base + capacity`.
    pub(crate) fn add_range(&mut self, lo: usize, hi: usize) {
        self.insert(lo, hi);
    }

    /// Inserts the range `[lo, hi)` at its `lo`-sorted position — the single insertion point,
    /// so `ranges`' sorted-by-`lo` invariant can never be bypassed.
    fn insert(&mut self, lo: usize, hi: usize) {
        let at = self.ranges.partition_point(|&(existing, _)| existing <= lo);
        self.ranges.insert(at, (lo, hi));
    }

    /// The evacuated ranges, as half-open byte-address pairs, **sorted by `lo`**.
    pub(crate) fn ranges(&self) -> &[(usize, usize)] {
        &self.ranges
    }

    /// Whether `ptr` falls inside any evacuated range.
    pub fn contains(&self, ptr: *const Eterm) -> bool {
        self.locate(ptr as usize).is_some()
    }

    /// The index in [`ranges`](FromSet::ranges) of a range covering `addr`, or `None`.
    ///
    /// This is the **single** place the range geometry is interpreted, so `contains` and the
    /// reachability walk's bit index can never disagree about which range (and therefore which
    /// word) an address belongs to. Ranges are sorted by `lo`, so the last one with
    /// `lo <= addr` is the only candidate: the others start after `addr`, and a covering range
    /// starting before the candidate's `lo` cannot exist because the ranges of a from-set are
    /// pairwise **disjoint** (they are the live regions of distinct, simultaneously live
    /// allocations). The walk back over ranges sharing that `lo` keeps the answer well defined
    /// even for a caller that added the very same range twice.
    pub(crate) fn locate(&self, addr: usize) -> Option<usize> {
        let end = self.ranges.partition_point(|&(lo, _)| lo <= addr);
        if end == 0 {
            return None;
        }
        let candidate_lo = self.ranges[end - 1].0;
        let mut index = end;
        while index > 0 && self.ranges[index - 1].0 == candidate_lo {
            index -= 1;
            if addr < self.ranges[index].1 {
                return Some(index);
            }
        }
        None
    }

    /// The total number of words covered by this set's ranges.
    ///
    /// Used by the copier to compute `words_reclaimed` (the from-space words not
    /// copied). A range added before the from-space was filled therefore over-counts;
    /// callers add a space only after its mutator has finished with it.
    pub fn words(&self) -> usize {
        // The ranges are byte addresses; convert the span back to term words.
        let word = core::mem::size_of::<Eterm>();
        self.ranges.iter().map(|&(lo, hi)| (hi - lo) / word).sum()
    }

    /// Whether this set has no ranges.
    pub fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

#[cfg(test)]
mod tests {
    //! Unit tests for the space semantics ([`HeapSpace`]) and the from-set range index
    //! ([`FromSet`]): geometry and the bump pointer, the live-region
    //! [`HeapSpace::contains`] / [`HeapSpace::contains_term`] tests (with the exclusive
    //! `top()` boundary and out-of-space pointers), [`HeapSpace::reset`], the
    //! [`HeapSpace::from_heap`] / [`HeapSpace::into_heap`] round-trip and the [`FromSet`]
    //! range bookkeeping.
    //!
    //! `unsafe` below only steps pointers this test just allocated itself (and never reads
    //! uninitialised words); every block carries a `// SAFETY:` rationale.

    use core::mem::size_of;
    use core::ptr::addr_of;

    use beam_alloc::default_allocator;
    use beam_term::prelude::{is_boxed, is_list, make_atom, make_small, Atom, Eterm, NIL};

    use super::*;

    /// Capacity (in term words) used by the tests that do not care about the exact size.
    const CAP: usize = 32;

    /// An empty `capacity_words`-word semi-space over the default allocator.
    fn new_space(capacity_words: usize) -> HeapSpace {
        HeapSpace::new(capacity_words).expect("the default allocator satisfies the request")
    }

    #[test]
    fn geometry_tracks_capacity_and_the_bump_pointer() {
        let mut space = new_space(CAP);
        let word = size_of::<Eterm>();

        assert_eq!(space.capacity_words(), CAP);
        assert_eq!(space.used_words(), 0);
        assert_eq!(space.available_words(), CAP);
        assert_ne!(space.base() as usize, 0, "a live space has a non-null base");
        assert_eq!(
            space.top(),
            space.base(),
            "an empty space bumps at its base"
        );
        assert_eq!(
            space.limit() as usize,
            space.base() as usize + CAP * word,
            "the limit is `capacity` words past the base"
        );

        let base = space.base();
        let limit = space.limit();

        // A partial allocation (`N < CAP`) moves only the bump pointer.
        const N: usize = 5;
        // SAFETY: `space` exclusively owns `CAP` writable words and `N < CAP`, so the
        // returned pointer covers `N` in-bounds, exclusively-owned words (never read).
        let first = unsafe { space.heap_mut().alloc_words(N) }.expect("N words fit");

        assert_eq!(first, base, "the first handed-out word is the base");
        assert_eq!(space.used_words(), N);
        assert_eq!(space.available_words(), CAP - N);
        assert_eq!(space.top() as usize, base as usize + N * word);
        assert_eq!(space.base(), base, "the base never moves");
        assert_eq!(space.limit(), limit, "the limit never moves");
        assert_eq!(space.capacity_words(), CAP, "the capacity never changes");
    }

    #[test]
    fn with_allocator_builds_a_space_over_the_given_allocator() {
        let alloc = default_allocator().clone();
        let space = HeapSpace::with_allocator(alloc.clone(), 8).expect("heap allocation");

        assert_eq!(space.capacity_words(), 8);
        assert_eq!(space.used_words(), 0);
        assert!(
            std::sync::Arc::ptr_eq(space.allocator(), &alloc),
            "the space keeps the allocator it was built with"
        );
    }

    #[test]
    fn contains_matches_the_live_region_only() {
        let mut space = new_space(16);

        // SAFETY: both requests are in-bounds of the 16-word space, which this test
        // exclusively owns; no handed-out word is read.
        let first = unsafe { space.heap_mut().alloc_words(2) }.expect("2 of 16 words");
        // SAFETY: as above — 2 of the remaining 14 words.
        let second = unsafe { space.heap_mut().alloc_words(2) }.expect("2 of 16 words");

        let top = space.top();
        let limit = space.limit();

        // The live region is `[base, top)`: every handed-out word is contained.
        assert!(space.contains(space.base() as *const Eterm));
        assert!(space.contains(first as *const Eterm));
        assert!(space.contains(second as *const Eterm));

        // `top()` is one past the last handed-out word, so it is NOT contained — nor is
        // anything in the unallocated `[top, limit)` tail, up to and including `limit`.
        assert!(!space.contains(top as *const Eterm), "top() is exclusive");
        assert!(!space.contains(top.wrapping_add(1) as *const Eterm));
        assert!(!space.contains(limit.wrapping_sub(1) as *const Eterm));
        assert!(!space.contains(limit as *const Eterm));

        // Pointers outside the block are never contained.
        assert!(!space.contains(space.base().wrapping_sub(1) as *const Eterm));
        let local: Eterm = make_small(1);
        assert!(
            !space.contains(addr_of!(local)),
            "a stack address is outside the space"
        );
    }

    #[test]
    fn contains_term_accepts_live_boxed_and_list_terms_only() {
        let mut space = new_space(16);
        let mut other = new_space(16);

        let tuple = space
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("tuple");
        let list = space.heap_mut().cons(make_small(3), NIL).expect("cons");
        let elsewhere = other.heap_mut().tuple(&[make_small(4)]).expect("tuple");

        // Boxed and list terms resident in the live region are contained.
        assert!(is_boxed(tuple));
        assert!(space.contains_term(tuple));
        assert!(is_list(list));
        assert!(space.contains_term(list));

        // Immediates never are, however they are tagged.
        assert!(!space.contains_term(make_small(7)));
        assert!(!space.contains_term(NIL));
        assert!(!space.contains_term(make_atom(Atom::from_raw(3))));

        // A boxed term resident in a *different* space is not contained here.
        assert!(is_boxed(elsewhere));
        assert!(!space.contains_term(elsewhere));
        assert!(other.contains_term(elsewhere));
    }

    #[test]
    fn reset_rewinds_the_bump_pointer_and_invalidates_live_terms() {
        let mut space = new_space(16);

        let tuple = space
            .heap_mut()
            .tuple(&[make_small(1), make_small(2)])
            .expect("tuple");
        let base = space.base();
        assert_eq!(space.used_words(), 3);
        assert!(space.contains_term(tuple));

        space.reset();

        assert_eq!(space.used_words(), 0);
        assert_eq!(space.top(), base, "reset rewinds to the base");
        assert_eq!(space.available_words(), space.capacity_words());
        assert_eq!(space.capacity_words(), 16, "reset does not resize");
        assert!(
            !space.contains_term(tuple),
            "the previously live term is no longer in the live region"
        );
        assert!(!space.contains(ptr_val(tuple)));
    }

    #[test]
    fn from_heap_and_into_heap_round_trip() {
        let mut heap = Heap::new(24).expect("heap allocation");
        heap.tuple(&[make_small(1), make_small(2)])
            .expect("tuple fits");
        let capacity = heap.capacity();
        let used = heap.used();
        let hp = heap.hp();
        let hend = heap.hend();

        let mut space = HeapSpace::from_heap(heap);

        // The wrapping space reports exactly the source heap's geometry.
        assert_eq!(space.capacity_words(), capacity);
        assert_eq!(space.used_words(), used);
        assert_eq!(space.available_words(), capacity - used);
        assert_eq!(space.top(), hp);
        assert_eq!(space.limit(), hend);
        assert_eq!(
            space.base() as usize + capacity * size_of::<Eterm>(),
            hend as usize
        );

        // The space remains a usable term arena.
        space.heap_mut().cons(make_small(3), NIL).expect("cons");

        let heap = space.into_heap();
        assert_eq!(heap.capacity(), capacity, "no reallocation, no resize");
        assert_eq!(heap.used(), used + 2, "the cons cell is 2 words");
    }

    #[test]
    fn from_set_indexes_the_added_live_ranges() {
        let mut young = new_space(16);
        let mut mature = new_space(16);
        let mut set = FromSet::new();

        assert!(set.is_empty(), "a fresh set has no ranges");
        assert_eq!(set.words(), 0);
        assert!(
            !set.contains(young.top() as *const Eterm),
            "an empty set contains nothing"
        );

        let head = young.heap_mut().cons(make_small(1), NIL).expect("cons");
        let tuple = mature
            .heap_mut()
            .tuple(&[make_small(2), make_small(3)])
            .expect("tuple");

        set.add(&young);
        assert!(!set.is_empty());
        assert_eq!(set.words(), young.used_words());
        assert!(set.contains(ptr_val(head)));
        assert!(
            !set.contains(young.top() as *const Eterm),
            "the live range is half-open: top() is not evacuated"
        );
        assert!(
            !set.contains(mature.top() as *const Eterm),
            "the mature range is not in the set yet"
        );

        set.add(&mature);
        assert!(set.contains(ptr_val(head)), "the young range is still in");
        assert!(set.contains(ptr_val(tuple)));
        assert_eq!(
            set.words(),
            young.used_words() + mature.used_words(),
            "the covered words are the sum of the two live ranges"
        );

        // Out-of-range pointers belong to no range of the set.
        assert!(!set.contains(young.base().wrapping_sub(1) as *const Eterm));
        let local: Eterm = make_small(9);
        assert!(!set.contains(addr_of!(local)));
    }

    #[test]
    fn from_set_contains_is_exact_over_many_disjoint_ranges() {
        /// More ranges than any hand-written case would add, and enough that a linear scan per
        /// reference word is the cost the sorted index removes.
        const RANGES: usize = 1200;
        let word = size_of::<Eterm>();

        // `RANGES` disjoint ranges with varying widths and varying holes between them.
        let mut expected: Vec<(usize, usize)> = Vec::with_capacity(RANGES);
        let mut next = 0x10_000usize;
        for i in 0..RANGES {
            let lo = next;
            let hi = lo + (1 + i % 7) * word;
            expected.push((lo, hi));
            next = hi + (i % 3) * word;
        }

        // Inserted in a scrambled order, so the sorted insertion is exercised too: 137 is
        // coprime with 1200, so `(i * 137) % RANGES` is a permutation of `0..RANGES`.
        let mut set = FromSet::new();
        for i in 0..RANGES {
            let (lo, hi) = expected[(i * 137) % RANGES];
            set.add_range(lo, hi);
        }

        // The stored ranges are exactly the added ones, in `lo` order (the shape the binary
        // search relies on), and `words()` still sums their spans.
        let mut sorted = expected.clone();
        sorted.sort_unstable();
        assert_eq!(set.ranges(), sorted.as_slice());
        let total_words: usize = expected.iter().map(|&(lo, hi)| (hi - lo) / word).sum();
        assert_eq!(set.words(), total_words);

        // The membership test is exactly the linear scan over the added ranges — word by word
        // inside every range, plus the words straddling both of its boundaries — and `locate`
        // agrees with it and names a range that really covers the address. `locate` is the one
        // place the geometry is interpreted (the reachability walk's bit index goes through it),
        // so the two can never disagree.
        let mut inside = 0usize;
        for &(lo, hi) in &expected {
            let mut addr = lo - 2 * word;
            while addr <= hi + 2 * word {
                let want = expected.iter().any(|&(l, h)| l <= addr && addr < h);
                assert_eq!(
                    set.contains(addr as *const Eterm),
                    want,
                    "contains({addr:#x}) must equal the scan"
                );
                match set.locate(addr) {
                    Some(index) => {
                        let (l, h) = set.ranges()[index];
                        assert!(l <= addr && addr < h, "locate names a covering range");
                    }
                    None => assert!(!want, "locate must find every covered address"),
                }
                inside += usize::from(want);
                addr += word;
            }
        }
        assert!(
            inside >= RANGES,
            "the sweep really lands inside the ranges, got {inside}"
        );

        // An address below the first range and one above the last are outside every range.
        assert!(!set.contains((expected[0].0 - word) as *const Eterm));
        let last = expected.iter().map(|&(_, hi)| hi).max().expect("a range");
        assert!(
            !set.contains(last as *const Eterm),
            "the last range is half-open"
        );
    }

    #[test]
    fn from_set_contains_handles_the_same_range_added_more_than_once() {
        let word = size_of::<Eterm>();
        let mut set = FromSet::new();
        set.add_range(0x100, 0x100 + 3 * word);
        set.add_range(0x100, 0x100 + 3 * word); // the identical range again
        set.add_range(0x100, 0x100 + word); // and a shorter one sharing its `lo`

        // Every word of the widest entry is covered, however many entries name it, and the
        // predicate is still half-open at the widest `hi`.
        for addr in [0x100, 0x100 + word, 0x100 + 2 * word] {
            assert!(
                set.contains(addr as *const Eterm),
                "the widest entry covers {addr:#x}"
            );
            let index = set.locate(addr).expect("a covering entry");
            let (lo, hi) = set.ranges()[index];
            assert!(lo <= addr && addr < hi, "locate names a covering entry");
        }
        assert!(
            !set.contains((0x100 + 3 * word) as *const Eterm),
            "the widest range is half-open"
        );
        assert_eq!(set.locate(0x100 - 1), None, "below every entry");
    }
}
