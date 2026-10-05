//! Loader-specific error constructors over [`beam_common::BeamError`].
//!
//! The crate uses **only** `beam_common::{Result, BeamError}` — there is no
//! crate-local error enum (`../CONTEXT.md` §3). These small helpers name the two
//! failure shapes a `.beam` reader produces so the parsing code reads clearly:
//! a structurally invalid image ([`bad_file`]) and a truncated one
//! ([`truncated`]). Both map to [`BeamError::BadArg`], the BEAM `badarg` error
//! atom, exactly as `beam_file.c` rejects malformed input.
use beam_common::BeamError;

/// A `.beam` image (or a structure within it) is structurally invalid.
///
/// The `_msg` describes the offending field for diagnostics; the error itself is
/// the BEAM `badarg` atom ([`BeamError::BadArg`]).
pub fn bad_file(_msg: &str) -> BeamError {
    BeamError::BadArg
}

/// A `.beam` structure ended before its declared length was read.
///
/// `_what` names the structure being read (for diagnostics); the error is the BEAM
/// `badarg` atom ([`BeamError::BadArg`]).
pub fn truncated(_what: &'static str) -> BeamError {
    BeamError::BadArg
}
