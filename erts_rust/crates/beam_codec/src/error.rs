//! Error construction for the codec.
//!
//! This crate adds **no error type of its own** (crate-layer `CONTEXT.md` §3):
//! malformed, truncated or bad-tag input maps to [`BeamError::BadArg`]; a
//! size/arity/length bound (`MAX_ARITYVAL`, an atom longer than 255 characters,
//! `BIG_ARITY_MAX`, a list/map length beyond the encodable field) maps to
//! [`BeamError::SystemLimit`]; heap exhaustion propagates as `BeamError::Alloc`
//! from the heap itself.
//!
//! A tag the codec does not support *yet* is reported through [`unsupported`],
//! which raises the [`UNSUPPORTED_REASON`] atom so a caller can tell "not
//! implemented yet" apart from "malformed input" (in particular for
//! `binary_to_term`, which must distinguish the two).

use beam_common::{BeamError, ExitReason, Raise};

use crate::tag::Tag;

/// The error-reason atom raised for a tag the codec does not support yet.
///
/// Carried as the *reason* of a `BeamError::Raise`, which is what makes the
/// distinction from [`BeamError::BadArg`] observable by a caller.
pub const UNSUPPORTED_REASON: &str = "unsupported_external_term";

/// Builds the error for a tag this milestone does not (yet) support.
///
/// Returns `BeamError::Raise(Raise::error(ExitReason::atom(UNSUPPORTED_REASON)))`.
/// The offending `tag` is a parameter so call sites read naturally and a future
/// refinement can attach it to the reason without an API change; the M1 reason
/// itself is the fixed [`UNSUPPORTED_REASON`] atom.
pub fn unsupported(tag: Tag) -> BeamError {
    let _ = tag;
    BeamError::Raise(Raise::error(ExitReason::atom(UNSUPPORTED_REASON)))
}

/// Whether `err` is the "unsupported external term" error built by [`unsupported`].
///
/// Detectable without matching on the (non-exhaustive) error enum:
/// `error::is_unsupported(&err)`.
pub fn is_unsupported(err: &BeamError) -> bool {
    match err {
        BeamError::Raise(raise) => raise.reason.as_atom() == Some(UNSUPPORTED_REASON),
        _ => false,
    }
}

/// The error for malformed, truncated or otherwise invalid wire input.
pub(crate) fn bad_arg() -> BeamError {
    BeamError::BadArg
}

/// The error for input that exceeds a hard wire or runtime bound.
pub(crate) fn system_limit() -> BeamError {
    BeamError::SystemLimit
}
