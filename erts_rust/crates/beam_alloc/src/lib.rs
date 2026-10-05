#![doc = "Allocator layer (erts_alloc replacement): carriers, arenas and the per-process heap bump allocator."]

//! Allocation primitives backing the Rust ERTS rewrite.
//!
//! Implements the memory-model half of Part II §D (allocators and carriers, the
//! `erts_alloc` replacement) plus the Stage-0 foundations of Part II §H: process
//! heap carriers, size classes and the bump allocator the copy engine and GC use.

/// Returns this crate's name; placeholder until the real API lands.
pub fn beam_alloc() -> &'static str {
    "beam_alloc"
}
