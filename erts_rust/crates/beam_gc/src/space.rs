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
pub struct FromSet {
    ranges: Vec<(usize, usize)>,
}

impl FromSet {
    /// An empty set of ranges.
    pub fn new() -> FromSet {
        FromSet { ranges: Vec::new() }
    }

    /// Adds the live region of `space` as a range this set evacuates.
    pub fn add(&mut self, space: &HeapSpace) {
        self.ranges
            .push((space.base() as usize, space.top() as usize));
    }

    /// Whether `ptr` falls inside any evacuated range.
    pub fn contains(&self, ptr: *const Eterm) -> bool {
        let addr = ptr as usize;
        self.ranges.iter().any(|&(lo, hi)| lo <= addr && addr < hi)
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
