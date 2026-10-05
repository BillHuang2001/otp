//! `pub(crate)` layout helpers: per-object word counts, off-heap-container identification and
//! the off-heap reference encoding used by the copier.
//!
//! # Stub
//!
//! This module is the frozen internal layout seam; the implementation pass fills the bodies.
//! Ground truth: `copy.c`'s `move_boxed`/`size_object_x` and the frozen `BinRef` container thing
//! `[HEADER_BIN_REF, val, next]` (`ERL_BIN_REF_SIZE = 3`, `beam_term::boxed`), where M1 stores
//! the MSO slot in `val` and leaves `next` unused (see the crate `CONTEXT.md`, Design Decisions).
#![allow(dead_code)] // stub module: consumed by the implementation pass

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

/// The number of heap words the object headed by `header` occupies, header word included.
///
/// The *single-object* companion of `beam_term::size_object` (which gives the whole graph's word
/// count): this is what the incremental allocator inside `copy_struct` reserves per object.
/// Ground truth: `copy.c`'s `move_boxed`.
pub(crate) fn object_words(header: Eterm) -> usize {
    let _ = header;
    unimplemented!("TODO(layout::object_words): per-object word count, header included")
}

/// Whether `header` is the head word of an off-heap reference container.
pub(crate) fn is_off_heap_container(header: Eterm) -> bool {
    let _ = header;
    unimplemented!("TODO(layout::is_off_heap_container): header == HEADER_BIN_REF")
}

/// Whether the compound term `term` is an off-heap reference container.
pub(crate) fn is_off_heap_term(term: Eterm) -> bool {
    let _ = term;
    unimplemented!("TODO(layout::is_off_heap_term): dereference and test the container header")
}

/// The off-heap slot stored in the container `container` (its `val` word).
pub(crate) fn off_heap_slot(container: Eterm) -> usize {
    let _ = container;
    unimplemented!("TODO(layout::off_heap_slot): read the container's val word")
}

/// Builds an off-heap reference container for `slot` on the destination heap `to`.
pub(crate) fn make_off_heap_container(to: &mut Heap, slot: usize) -> Result<Eterm> {
    let _ = (to, slot);
    unimplemented!("TODO(layout::make_off_heap_container): allocate [HEADER_BIN_REF, slot, 0]")
}
