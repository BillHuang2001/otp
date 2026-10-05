//! `pub(crate)` error constructors — the crate adds no error type of its own.
//!
//! Every fallible path in `beam_io` returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module only centralises the
//! construction of the failures the I/O layer can raise, exactly as
//! `beam_process::error` and `beam_message::error` do for their crates. The mapping is
//! fixed by the crate `CONTEXT.md`:
//!
//! * an I/O or syscall failure (`read`/`write`/`pipe`/registration) ⇒ [`io`]
//!   (`BeamError::Io`),
//! * an argument fault (a bad fd, a bad interest mask, an unknown driver name) ⇒
//!   [`badarg`] (`BeamError::BadArg`),
//! * id or poll-registration exhaustion ⇒ [`system_limit`] (`BeamError::SystemLimit`).
//!
//! `mio` reports its own failures as [`std::io::Error`]; callers hand those to
//! [`io`] so the crate never grows a second error enum.

use beam_common::BeamError;

/// The error for an I/O or syscall failure (`BeamError::Io`).
pub(crate) fn io(err: std::io::Error) -> BeamError {
    BeamError::Io(err)
}

/// The error for an argument fault: a bad fd, a bad interest mask or an unknown driver
/// name (`badarg`).
pub(crate) fn badarg() -> BeamError {
    BeamError::BadArg
}

/// The error raised when the port/registration number space is exhausted (`system_limit`).
pub(crate) fn system_limit() -> BeamError {
    BeamError::SystemLimit
}
