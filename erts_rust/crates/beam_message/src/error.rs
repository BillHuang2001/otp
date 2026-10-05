//! `pub(crate)` copy-failure constructors — the crate adds no error type of its own.
//!
//! Every fallible path in this crate returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module only centralises the construction of
//! the two failures the copy engine can raise, exactly as `beam_gc::error` does for the
//! collector. Heap exhaustion mid-copy is a **normal** condition (the caller may grow the
//! destination heap and retry), never a panic.

use beam_common::{AllocFailure, AllocKind, BeamError};
use beam_term::prelude::Eterm;

/// Builds the message-heap-exhaustion error raised while copying a term.
///
/// Reported as `BeamError::Alloc(AllocFailure::new(AllocKind::Message, bytes))` so the caller
/// can collect/grow and retry — never a panic and never a crate-local error enum. `words` is
/// the number of term words the copy still wanted.
pub(crate) fn heap_exhausted(words: usize) -> BeamError {
    BeamError::Alloc(AllocFailure::new(
        AllocKind::Message,
        words * core::mem::size_of::<Eterm>(),
    ))
}

/// Builds the error raised for a malformed/unsupported term layout or an out-of-range
/// off-heap slot (`badarg`).
pub(crate) fn bad_term() -> BeamError {
    BeamError::BadArg
}
