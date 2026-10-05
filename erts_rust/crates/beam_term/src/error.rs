//! Crate-local error type for the frozen term representation.
//!
//! `beam_common` (the workspace-wide `BeamError`) is still a placeholder, so this
//! crate keeps its own minimal `TermError`/`TermResult` pair. It is used by the
//! `try_*` constructors and accessors at trusted boundaries (arithmetic BIFs, the
//! loader, `beam_alloc`) so that range violations and arena exhaustion surface as
//! errors rather than silent corruption (workspace `CONTEXT.md` §3). Reconcile
//! with `beam_common::BeamError` once that crate lands.
//!
//! This module deliberately does **not** import anything from `beam_common`; the
//! declared dependency stays unused until the real error types land.

use core::fmt;

/// Failure of a checked term constructor or accessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermError {
    /// A value did not fit in a 60-bit BEAM small integer.
    SmallIntOutOfRange,
    /// An arity exceeded `MAX_ARITYVAL`, or an element index was out of range.
    ArityOutOfRange,
    /// A term arena had no room left for the requested number of words.
    HeapExhausted,
    /// A line of an `atom.names` source could not be parsed.
    InvalidAtomName,
    /// An atom index cannot be represented by [`crate::Atom`].
    AtomIndexOutOfRange,
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            TermError::SmallIntOutOfRange => "small integer out of range",
            TermError::ArityOutOfRange => "arity out of range",
            TermError::HeapExhausted => "heap exhausted",
            TermError::InvalidAtomName => "invalid atom name",
            TermError::AtomIndexOutOfRange => "atom index out of range",
        };
        f.write_str(msg)
    }
}

impl core::error::Error for TermError {}

/// Result alias used throughout this crate.
pub type TermResult<T> = core::result::Result<T, TermError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_messages() {
        assert_eq!(TermError::SmallIntOutOfRange.to_string(), "small integer out of range");
        assert_eq!(TermError::ArityOutOfRange.to_string(), "arity out of range");
        assert_eq!(TermError::HeapExhausted.to_string(), "heap exhausted");
        assert_eq!(TermError::InvalidAtomName.to_string(), "invalid atom name");
        assert_eq!(TermError::AtomIndexOutOfRange.to_string(), "atom index out of range");
    }

    #[test]
    fn usable_as_std_error() {
        let e: Box<dyn std::error::Error> = Box::new(TermError::HeapExhausted);
        assert_eq!(e.to_string(), "heap exhausted");
    }

    #[test]
    fn copy_and_eq() {
        let a = TermError::HeapExhausted;
        let b = a;
        assert_eq!(a, b);
    }
}
