//! Crate-local error type for the frozen term representation.
//!
//! `beam_common` provides the workspace-wide [`BeamError`], but term-construction
//! failures are most precisely described by this crate, so `TermError`/`TermResult`
//! stay the error type of the `try_*` constructors and accessors at trusted
//! boundaries (arithmetic BIFs, the loader, `beam_alloc`): range violations and arena
//! exhaustion surface as errors rather than silent corruption (workspace
//! `CONTEXT.md` §3).
//!
//! This module uses the declared `beam_common` dependency for exactly one thing: the
//! `From<TermError> for beam_common::BeamError` impl below, so downstream crates can
//! use `?` across the `TermError -> BeamError` boundary instead of each writing its
//! own mapping (Prevent-Duplication — the conversion is owned upstream, here).

use beam_common::{AllocFailure, AllocKind, BeamError};
use core::fmt;

/// Failure of a checked term constructor or accessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TermError {
    /// A value did not fit in a 60-bit BEAM small integer.
    SmallIntOutOfRange,
    /// An arity exceeded `MAX_ARITYVAL`, a fun header field exceeded
    /// [`crate::fun::FUN_MAX_FIELD`] (255), or an element index was out of range.
    ArityOutOfRange,
    /// A term arena had no room left for the requested number of words.
    HeapExhausted,
    /// A line of an `atom.names` source could not be parsed.
    InvalidAtomName,
    /// An atom index cannot be represented by [`crate::Atom`].
    AtomIndexOutOfRange,
    /// A map operation was applied to a term that is not a well-formed map, or a
    /// map node held a slot that is neither a leaf, a sub-node nor a collision
    /// tuple (see [`crate::map`]).
    BadMap,
    /// A bitstring construction argument was inconsistent: the supplied byte count
    /// was smaller than `ceil(n_bits / 8)` for the requested bit count, or a byte
    /// count's bit count (`bytes.len() * 8`) overflowed `usize` (see
    /// [`crate::bitstring`]).
    BadBitstring,
    /// A fun/export layout helper or accessor was applied to a term that is not a
    /// fun at all, or to a fun of the wrong kind for that accessor (see
    /// [`crate::fun`]).
    BadFun,
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            TermError::SmallIntOutOfRange => "small integer out of range",
            TermError::ArityOutOfRange => "arity out of range",
            TermError::HeapExhausted => "heap exhausted",
            TermError::InvalidAtomName => "invalid atom name",
            TermError::AtomIndexOutOfRange => "atom index out of range",
            TermError::BadMap => "not a map",
            TermError::BadBitstring => "invalid bitstring byte count",
            TermError::BadFun => "not a fun",
        };
        f.write_str(msg)
    }
}

impl core::error::Error for TermError {}

/// Reconciles this crate's [`TermError`] with the workspace error type so dependents
/// can `?` across the boundary (workspace `CONTEXT.md` §3: one error type upstream).
///
/// The mapping is lossy by design — `BeamError` carries only the BEAM error atom the
/// engine raises:
///
/// | `TermError` | `BeamError` | BEAM reason |
/// |---|---|---|
/// | `SmallIntOutOfRange` | `BadArith` | `badarith` |
/// | `ArityOutOfRange` | `SystemLimit` | `system_limit` |
/// | `HeapExhausted` | `Alloc(..)` | `out_of_memory` |
/// | `InvalidAtomName` | `BadArg` | `badarg` |
/// | `AtomIndexOutOfRange` | `SystemLimit` | `system_limit` |
/// | `BadMap` | `BadArg` | `badarg` |
/// | `BadBitstring` | `BadArg` | `badarg` |
/// | `BadFun` | `BadArg` | `badarg` |
impl From<TermError> for BeamError {
    fn from(error: TermError) -> Self {
        match error {
            // A 60-bit small-integer overflow is an arithmetic-range failure.
            TermError::SmallIntOutOfRange => BeamError::BadArith,
            // Exceeding `MAX_ARITYVAL` is a hard runtime limit.
            TermError::ArityOutOfRange => BeamError::SystemLimit,
            // Arena exhaustion is an allocation failure. `TermError::HeapExhausted`
            // does not carry a byte count, so `requested` is reported as 0 (unknown).
            TermError::HeapExhausted => BeamError::Alloc(AllocFailure::new(AllocKind::Heap, 0)),
            // A malformed `atom.names` line is an argument/format failure.
            TermError::InvalidAtomName => BeamError::BadArg,
            // Running past `Atom::MAX` is a hard runtime limit.
            TermError::AtomIndexOutOfRange => BeamError::SystemLimit,
            // A malformed map reached a map accessor: an argument failure.
            TermError::BadMap => BeamError::BadArg,
            // A bitstring built from an inconsistent byte count: an argument failure.
            TermError::BadBitstring => BeamError::BadArg,
            // A non-fun (or wrong-kind fun) reached a fun layout helper: an argument
            // failure.
            TermError::BadFun => BeamError::BadArg,
        }
    }
}

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
        assert_eq!(TermError::BadMap.to_string(), "not a map");
        assert_eq!(
            TermError::BadBitstring.to_string(),
            "invalid bitstring byte count"
        );
        assert_eq!(TermError::BadFun.to_string(), "not a fun");
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

    #[test]
    fn converts_into_workspace_beam_error() {
        assert!(matches!(
            BeamError::from(TermError::SmallIntOutOfRange),
            BeamError::BadArith
        ));
        assert!(matches!(
            BeamError::from(TermError::ArityOutOfRange),
            BeamError::SystemLimit
        ));
        match BeamError::from(TermError::HeapExhausted) {
            BeamError::Alloc(failure) => {
                assert_eq!(failure.kind, AllocKind::Heap);
                assert_eq!(failure.requested, 0);
            }
            other => panic!("expected BeamError::Alloc, got {other:?}"),
        }
        assert!(matches!(
            BeamError::from(TermError::InvalidAtomName),
            BeamError::BadArg
        ));
        assert!(matches!(
            BeamError::from(TermError::AtomIndexOutOfRange),
            BeamError::SystemLimit
        ));
        assert!(matches!(BeamError::from(TermError::BadMap), BeamError::BadArg));
        assert!(matches!(
            BeamError::from(TermError::BadBitstring),
            BeamError::BadArg
        ));
        assert!(matches!(BeamError::from(TermError::BadFun), BeamError::BadArg));
    }

    #[test]
    fn conversion_works_with_try() {
        fn fallible() -> beam_common::Result<u8> {
            let _ = Err::<u8, _>(TermError::SmallIntOutOfRange)?;
            Ok(0)
        }
        assert!(matches!(fallible(), Err(BeamError::BadArith)));
    }
}
