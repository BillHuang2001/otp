//! Failure constructors for the crate.
//!
//! There is **no crate-local error enum** (workspace `CONTEXT.md` §3): every
//! fallible op returns [`beam_common::Result`], i.e. [`beam_common::BeamError`].
//! This module only centralises the constructors for the failures the table ops
//! raise, so the reason each maps to is stated in exactly one place.
//!
//! These helpers are `pub(crate)` — they are implementation details of the
//! crate, not part of its public API.

use beam_common::BeamError;

/// The `badarg` failure of a malformed table operation (bad object, bad
/// `keypos`, missing element, unsupported match spec, …).
pub(crate) fn badarg() -> BeamError {
    BeamError::BadArg
}
