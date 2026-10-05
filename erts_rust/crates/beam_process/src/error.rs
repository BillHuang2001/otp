//! `pub(crate)` error constructors — the crate adds no error type of its own.
//!
//! Every fallible path in this crate returns the workspace error type
//! ([`BeamError`](beam_common::BeamError)); this module only centralises the construction
//! of the failures the process/scheduler layer can raise, exactly as `beam_gc::error` and
//! `beam_message::error` do for their crates. The mapping is fixed by the crate
//! `CONTEXT.md`:
//!
//! * an unknown pid/name/monitor reference ⇒ [`unknown_process`] (`TermNotFound`),
//! * a malformed pid term or an out-of-range register index ⇒ [`bad_pid`] (`BadArg`),
//! * pid/port number-space exhaustion ⇒ [`system_limit`] (`SystemLimit`).
//!
//! Heap exhaustion during delivery is *not* modelled here: it is the underlying
//! `BeamError::Alloc` propagated after a collect-and-retry (see `Process::deliver`).

use beam_common::BeamError;

/// The error for a lookup that found no process: an unknown pid, an unregistered name or
/// a dangling monitor reference (`TermNotFound`, which has no BEAM error atom).
pub(crate) fn unknown_process() -> BeamError {
    BeamError::TermNotFound
}

/// The error for a malformed pid/port term or an out-of-range register index (`badarg`).
pub(crate) fn bad_pid() -> BeamError {
    BeamError::BadArg
}

/// The error raised when the pid or port number space is exhausted (`system_limit`).
pub(crate) fn system_limit() -> BeamError {
    BeamError::SystemLimit
}
