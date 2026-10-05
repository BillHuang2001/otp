//! BIF failure constructors and the public raise builders.
//!
//! There is **no crate-local error type** (workspace `CONTEXT.md` §3): every
//! fallible BIF returns [`beam_common::Result`], and the failure vocabulary is
//! `beam_common::BeamError`. This module only names the common cases so the BIF
//! modules read like the C BIFs (`badarg()`, `badarith()`, …).
//!
//! The `pub(crate)` constructors mirror the C helpers in
//! `../../erts/emulator/beam/bif.c` (`badarg`, `badarith`); the public
//! `raise_*` builders construct the `error`/`exit`/`throw` BEAM exceptions the
//! engine turns into an actual raise.

use beam_common::{BeamError, ExitReason, Raise};

/// `badarg` — an argument failed a type/range/format check.
pub(crate) fn badarg() -> BeamError {
    BeamError::BadArg
}

/// `badarith` — arithmetic on a non-numeric term.
pub(crate) fn badarith() -> BeamError {
    BeamError::BadArith
}

/// `badarity` — a call with the wrong number of arguments.
pub(crate) fn badarity() -> BeamError {
    BeamError::BadArity
}

/// `system_limit` — a hard runtime limit was exceeded (e.g. 60-bit overflow).
pub(crate) fn system_limit() -> BeamError {
    BeamError::SystemLimit
}

/// `undef` — the `(module, function, arity)` is not a registered BIF.
pub(crate) fn undefined_bif() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("undef")))
}

/// `unsupported_bif` — a registered seam (e.g. `send`) has no provider wired up.
pub(crate) fn unsupported_bif() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("unsupported_bif")))
}

/// Builds an `error`-class raise with `reason`.
pub fn raise_error(reason: ExitReason) -> BeamError {
    BeamError::Raise(Raise::error(reason))
}

/// Builds an `exit`-class raise with `reason`.
pub fn raise_exit(reason: ExitReason) -> BeamError {
    BeamError::Raise(Raise::exit(reason))
}

/// Builds a `throw`-class raise with `reason`.
pub fn raise_throw(reason: ExitReason) -> BeamError {
    BeamError::Raise(Raise::throw(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::ExceptionClass;

    #[test]
    fn failure_constructors_map_to_the_expected_variants() {
        assert!(matches!(badarg(), BeamError::BadArg));
        assert!(matches!(badarith(), BeamError::BadArith));
        assert!(matches!(badarity(), BeamError::BadArity));
        assert!(matches!(system_limit(), BeamError::SystemLimit));
    }

    #[test]
    fn undefined_and_unsupported_are_error_raises() {
        for (err, atom) in [
            (undefined_bif(), "undef"),
            (unsupported_bif(), "unsupported_bif"),
        ] {
            match err {
                BeamError::Raise(raise) => {
                    assert_eq!(raise.class, ExceptionClass::Error);
                    assert_eq!(raise.reason, ExitReason::atom(atom));
                }
                other => panic!("expected a raise, got {other:?}"),
            }
        }
    }

    #[test]
    fn raise_builders_set_the_class_and_reason() {
        let reason = ExitReason::atom("boom");
        let cases = [
            (raise_error(reason.clone()), ExceptionClass::Error),
            (raise_exit(reason.clone()), ExceptionClass::Exit),
            (raise_throw(reason.clone()), ExceptionClass::Throw),
        ];
        for (err, class) in cases {
            match err {
                BeamError::Raise(raise) => {
                    assert_eq!(raise.class, class);
                    assert_eq!(raise.reason, reason);
                }
                other => panic!("expected a raise, got {other:?}"),
            }
        }
    }
}
