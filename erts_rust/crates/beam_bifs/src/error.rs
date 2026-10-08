// %CopyrightBegin%
//
// SPDX-License-Identifier: Apache-2.0
//
// Copyright Ericsson AB 2025-2026. All Rights Reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// %CopyrightEnd%

//! BIF failure constructors, the structured-reason raise builders and the public
//! raise builders.
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
//!
//! # Structured reasons carry their term
//!
//! A reason that is *not* an atom — the tuple reasons `{badmap, Map}`,
//! `{badkey, Key}`, `{badrecord, Term}`, `{badfield, {{Module, Name}, Key}}`,
//! `{case_clause, Value}` and any reason a caller hands to `erlang:error/1` — cannot
//! live in [`beam_common::ExitReason`], which is term-independent (workspace
//! `CONTEXT.md` §3/§2: `beam_common` has no `beam_term` dependency, so
//! [`ExitReason::Structured`](beam_common::ExitReason::Structured) is a unit variant).
//! [`structured_error`] therefore builds the real reason tuple and attaches it to the
//! returned [`Raise`] through the opaque
//! [`Raise::with_live_reason`](beam_common::Raise::with_live_reason) slot, whose
//! workspace convention is that the payload **is** the live reason term: a consumer
//! recovers it with
//! `raise.live_reason().and_then(|live| live.downcast_ref::<Eterm>())`.
//!
//! The [tag](structured_error) atom is resolved through
//! [`BifContext::intern_atom`] — already interned, or interned by the runtime's
//! `AtomInterner` hook. Every tag used here (`badmap`, `badkey`, `badrecord`,
//! `badfield`, `case_clause`) is a predefined atom of the legacy emulator's
//! `../../erts/emulator/beam/atom.names` (lines 127, 128, 174).

use beam_common::{BeamError, ExceptionClass, ExitReason, LiveReason, Raise};
use beam_term::Eterm;

use crate::context::BifContext;

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

/// `function_clause` — a `lists.erl`-style no-clause failure
/// (`error:function_clause`; the arguments live in the stacktrace, not the reason).
pub(crate) fn function_clause() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("function_clause")))
}

/// `undef` — the `(module, function, arity)` is not a registered BIF.
pub(crate) fn undefined_bif() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("undef")))
}

/// `unsupported_bif` — a registered seam (e.g. `send`) has no provider wired up.
pub(crate) fn unsupported_bif() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom("unsupported_bif")))
}

/// An `error`-class raise whose live reason **is** `term`.
///
/// The raise's [`reason`](Raise::reason) is
/// [`ExitReason::Structured`](beam_common::ExitReason::Structured) —
/// `beam_common` cannot compare or print a term, so the term travels out of band in
/// the [`live_reason`](Raise::live_reason) slot (see the module docs). Every
/// observable part other than the term is therefore exactly as before: the class is
/// `error`, `BeamError::as_atom` answers `None`, and `Raise`'s `PartialEq`/`Debug`
/// ignore the payload.
pub(crate) fn raise_error_term(term: Eterm) -> BeamError {
    raise_term(ExceptionClass::Error, term)
}

/// A raise of `class` whose live reason **is** `term`.
///
/// The `beam_common::ExitReason` is always
/// [`ExitReason::Structured`](beam_common::ExitReason::Structured) (a term-carrying
/// reason is not representable there); `class` selects the `error`/`exit`/`throw`
/// exception class. Used by [`raise_error_term`] and by `reason.rs` to reproduce the
/// oracle's non-atom reasons of `erlang:error/1,2`, `exit/1`, `throw/1` and
/// `nif_error/1,2`.
pub(crate) fn raise_term(class: ExceptionClass, term: Eterm) -> BeamError {
    let reason = ExitReason::Structured;
    let raise = match class {
        ExceptionClass::Error => Raise::error(reason),
        ExceptionClass::Exit => Raise::exit(reason),
        ExceptionClass::Throw => Raise::throw(reason),
        // `ExceptionClass` is `#[non_exhaustive]`; an unknown class cannot name a
        // reason that is not an atom, so `error` is the only faithful choice.
        _ => Raise::error(reason),
    };
    BeamError::Raise(raise.with_live_reason(LiveReason::new(term)))
}

/// The `error` raise for the structured reason `{Tag, Elements…}`.
///
/// Builds the reason tuple on the context heap (the tag atom first, then `elements`)
/// and attaches it as the live reason term of an [`ExitReason::Structured`] raise.
///
/// # Errors
///
/// Propagates a heap-allocation failure while building the tuple. The one understood
/// recovery is a tag atom that is **neither interned nor internable**
/// ([`BeamError::TermNotFound`] — no `AtomInterner` hook is installed): the reason
/// cannot be named, so the failure degrades to the bare [`badarg`] this crate reported
/// before the live-reason slot existed. A fabricated or substituted atom is never used.
pub(crate) fn structured_error(
    ctx: &mut BifContext<'_>,
    tag: &str,
    elements: &[Eterm],
) -> beam_common::Result<BeamError> {
    let tag = match ctx.intern_atom(tag) {
        Ok(tag) => tag,
        Err(BeamError::TermNotFound) => return Ok(badarg()),
        Err(other) => return Err(other),
    };
    let mut words = Vec::with_capacity(elements.len() + 1);
    words.push(tag);
    words.extend_from_slice(elements);
    Ok(raise_error_term(ctx.tuple(&words)?))
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
    use beam_alloc::Heap;
    use beam_common::ExceptionClass;
    use beam_process::ProcessId;
    use beam_term::{make_small, AtomTable};

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

    /// A table holding the structured-reason tag atoms, plus one unrelated name.
    fn atoms() -> AtomTable {
        let mut atoms = AtomTable::new();
        atoms.seed(["badmap", "badkey", "badrecord", "badfield", "case_clause"]);
        atoms
    }

    #[test]
    fn raise_error_term_carries_the_term_and_keeps_the_structured_reason() {
        let err = raise_error_term(make_small(7));
        match err {
            BeamError::Raise(raise) => {
                assert_eq!(raise.class, ExceptionClass::Error);
                assert_eq!(raise.reason, ExitReason::Structured);
                assert_eq!(raise.reason.as_atom(), None);
                let live = raise
                    .live_reason()
                    .expect("a structured reason carries its term");
                assert_eq!(live.downcast_ref::<Eterm>(), Some(&make_small(7)));
                assert!(live.downcast_ref::<String>().is_none());
            }
            other => panic!("expected a raise, got {other:?}"),
        }
        // The term-independent surface is unchanged: no error atom.
        assert_eq!(raise_error_term(make_small(7)).as_atom(), None);
    }

    #[test]
    fn structured_error_builds_the_tagged_tuple() {
        let mut heap = Heap::new(64).expect("test heap");
        let atoms = atoms();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let err = structured_error(&mut ctx, "badmap", &[make_small(5)]).expect("the tag is seeded");
        match err {
            BeamError::Raise(raise) => {
                assert_eq!(raise.class, ExceptionClass::Error);
                assert_eq!(raise.reason, ExitReason::Structured);
                let term = *raise
                    .live_reason()
                    .expect("the reason term is attached")
                    .downcast_ref::<Eterm>()
                    .expect("the payload is the live reason term");
                // The term is a real `{badmap, 5}` tuple, structurally equal to an
                // independently built one (a boxed `Eterm` compares by address).
                let tag = ctx.atom("badmap").expect("seeded");
                let expected = ctx.tuple(&[tag, make_small(5)]).expect("the reason tuple fits");
                assert!(beam_term::compare::eq(term, expected, &atoms));
                assert_eq!(crate::access::tuple_element(term, 0), tag);
                assert_eq!(crate::access::tuple_element(term, 1), make_small(5));
            }
            other => panic!("expected a raise, got {other:?}"),
        }
    }

    #[test]
    fn structured_error_degrades_to_badarg_when_the_tag_cannot_be_named() {
        let mut heap = Heap::new(64).expect("test heap");
        // An empty table with no `AtomInterner` hook: `badmap` cannot be named.
        let atoms = AtomTable::new();
        let mut ctx = BifContext::new(&mut heap, &atoms, ProcessId::new(1, 0));

        let err = structured_error(&mut ctx, "badmap", &[make_small(5)])
            .expect("the understood recovery is not an error");
        assert!(matches!(err, BeamError::BadArg));
    }
}
