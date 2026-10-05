//! The copy engine: `copy_struct`, the iterative copy of a term graph between two process heaps.
//!
//! # Stub
//!
//! The public seam ([`CopyContext`], [`copy_struct`], [`copy_struct_simple`], [`size_of_copy`])
//! is frozen here; the implementation pass fills the bodies. The copier is *iterative* (an
//! explicit worklist — a deep list must never recurse on the native stack); immediates and
//! literal-area terms are **shared**, everything else is copied, and off-heap references are
//! refcounted rather than deep-copied. Ground truth: `copy.c`'s `copy_struct_x`.

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use crate::off_heap::OffHeapTable;

/// The copy boundary: the source process's off-heap table (to resolve off-heap references) and
/// the destination's (where the referenced off-heap objects are re-registered).
pub struct CopyContext<'a> {
    src: &'a OffHeapTable,
    dst: &'a mut OffHeapTable,
}

impl<'a> CopyContext<'a> {
    /// A copy boundary from `src`'s off-heap table into `dst`'s.
    pub fn new(src: &'a OffHeapTable, dst: &'a mut OffHeapTable) -> CopyContext<'a> {
        CopyContext { src, dst }
    }

    /// The source process's off-heap table.
    pub fn src_off_heap(&self) -> &OffHeapTable {
        self.src
    }

    /// The destination process's off-heap table.
    pub fn dst_off_heap(&self) -> &OffHeapTable {
        self.dst
    }
}

/// Copies `term`'s graph into `to`, returning the copied root.
///
/// Iterative (an explicit worklist); immediates and literal-area terms are shared (returned
/// unchanged); cons cells, tuples/records, funs, flatmaps and off-heap containers are copied and
/// off-heap references are refcounted into `ctx`'s destination table, never deep-copied.
pub fn copy_struct(term: Eterm, to: &mut Heap, ctx: &mut CopyContext<'_>) -> Result<Eterm> {
    let _ = (term, to, ctx);
    unimplemented!("TODO(copy_struct): iterative term-graph copy (copy.c copy_struct_x)")
}

/// The common case of [`copy_struct`] with empty off-heap tables (no off-heap references).
pub fn copy_struct_simple(term: Eterm, to: &mut Heap) -> Result<Eterm> {
    let _ = (term, to);
    unimplemented!("TODO(copy_struct_simple): copy a graph that holds no off-heap references")
}

/// The flat word count [`copy_struct`] preflights against [`Heap::available`].
///
/// A thin forwarder to `beam_term::size_object` (the `size_object_x` port); exposed so callers
/// and tests can size a destination heap without re-deriving it.
pub fn size_of_copy(term: Eterm) -> usize {
    beam_term::size_object(term)
}
