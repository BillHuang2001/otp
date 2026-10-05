//! `pub(crate)` GC allocation-failure constructors — the crate adds no error type of its own.
//!
//! Every fallible GC path returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module only centralises the
//! construction of the one GC-specific failure the collector can raise.

use beam_common::BeamError;

/// Builds the heap-exhaustion error raised while copying a term.
///
/// Heap exhaustion is a **normal** condition, reported as
/// `BeamError::Alloc(beam_common::AllocFailure::new(beam_common::AllocKind::Heap, bytes))`
/// so the caller can collect and retry — never a panic and never a crate-local
/// error enum. `words` is the number of term words the copy wanted.
pub(crate) fn heap_exhausted(words: usize) -> BeamError {
    let _ = words;
    unimplemented!("scaffold")
}
