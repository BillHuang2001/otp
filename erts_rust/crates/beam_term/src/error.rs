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
    /// An internal pid's process number or serial exceeded the field the id layout
    /// gives it (`ERTS_MAX_INTERNAL_PID_NUMBER` / `ERTS_MAX_INTERNAL_PID_SERIAL`,
    /// `erl_node_container_utils.h:182-183`), reported by
    /// [`crate::immediate::try_make_pid_data`].
    ///
    /// This lifts the C's call-site range check into the encoder seam: the pid
    /// decoder returns `NULL` (`external.c:3176-3179`) and `list_to_pid` jumps to
    /// `bad` (`bif.c:4582-4583`) for exactly this condition, so it reconciles to the
    /// same `badarg` rather than inventing a new reason.
    PidIdOutOfRange,
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
    /// A reference layout helper or accessor was applied to a term that is not a
    /// well-formed reference of the expected layout (see [`crate::boxed`]):
    /// [`crate::boxed::internal_ref_id`] on a term that is not an **M1
    /// internal-reference thing** — a word that is not a term, a boxed term of
    /// another kind, or a *C-layout* `ErtsORefThing` block, whose id word does not
    /// exist (reading it would run past the block).
    ///
    /// Deliberately distinct from [`TermError::RefIdentityUnavailable`]: that one is
    /// a perfectly well-formed M1 reference whose C hash cannot be reproduced, not a
    /// malformed term.
    BadRef,
    /// The operation needs identity information the term does not carry: a
    /// `make_fun3` [local fun](crate::fun::FunKind::Local) hashes through its
    /// module and `old_uniq` fields in the C (`erl_term_hashing.c:1224-1246`), but
    /// this tree's inlined 2-word descriptor (`[uniq: u64][index: u32]`) has
    /// neither, so `make_hash2`/`make_hash` cannot reproduce the oracle's value.
    ///
    /// This is *not* a malformed term — it is a well-formed term the operation
    /// cannot faithfully complete — so it is deliberately distinct from
    /// [`TermError::BadFun`] (see [`crate::hashing`]). Never substituted with a
    /// wrong value.
    FunIdentityUnavailable,
    /// The operation needs identity information the term does not carry: the C
    /// hashes an [internal reference](crate::boxed::internal_ref_numbers) through
    /// `internal_ref_numbers(ref)[0]` — the first id of the reference's **node
    /// container** (`erl_term_hashing.c:288-290` for `make_hash`, `:1454-1458` for
    /// `make_hash2`) — but this tree's M1 on-heap `Ref` thing (`[header, w1, w2]`)
    /// holds a single process-local id and no node container, so
    /// `make_hash2`/`make_hash` cannot reproduce the oracle's value.
    ///
    /// This is *not* a malformed term — it is a well-formed term the operation
    /// cannot faithfully complete — so it is deliberately distinct from
    /// [`TermError::NotATerm`] (which says "this is not a term at all"). Never
    /// substituted with a wrong value.
    RefIdentityUnavailable,
    /// A native-record layout helper or accessor was applied to a term that is not
    /// a well-formed native record (an `ErtsRecordInstance` /
    /// `ErtsRecordDefinition`), or a record construction argument was invalid:
    /// a duplicate/non-atom field key or a field count beyond `MAX_ARITYVAL` (see
    /// [`crate::record`]).
    BadRecord,
    /// The word handed to the operation is **not a term at all** — a bare header
    /// word, i.e. one whose primary tag is `HEADER` (see
    /// [`crate::tag::is_term_word`]): the C ABI's `THE_NON_VALUE` (`0`, or
    /// `make_header(0, FLOAT_SUBTAG)` in a debug build) and every other stray
    /// header word, such as a NIF handing back `enif_make_copy`'s "no value".
    ///
    /// Distinct from [`TermError::BadFun`]/[`TermError::BadMap`]/… on purpose: those
    /// say "this is a term, but not one of the right shape", whereas this says
    /// "this is not a term". The operation refuses rather than dereferencing a
    /// garbage (in release, `NULL`) `boxed_val` pointer — the C is undefined
    /// behaviour there.
    NotATerm,
    /// The term graph reachable from the word is **cyclic**: a compound object
    /// (cons cell, tuple, fun, map node, …) is reachable from itself. A cyclic graph
    /// has no flat size at all, and a naive `size_object` walk never terminates on
    /// one — the only way to build it is forged memory (a NIF handing back a graph
    /// that points into an ancestor), so it is an *invalid* graph rather than a
    /// merely large one. Reported by [`crate::size::size_object_bounded`] instead of
    /// looping.
    ///
    /// Deliberately distinct from [`TermError::TermGraphTooLarge`]: "cannot size, it
    /// is cyclic" is a malformed input, "cannot size, it is over budget" is not.
    TermGraphCyclic,
    /// The term graph reachable from the word is well-formed (acyclic) but its flat
    /// word count is larger than the budget the caller passed to
    /// [`crate::size::size_object_bounded`]. The graph can be sized — just not within
    /// that many words.
    TermGraphTooLarge,
}

impl fmt::Display for TermError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            TermError::SmallIntOutOfRange => "small integer out of range",
            TermError::PidIdOutOfRange => "internal pid id out of range",
            TermError::ArityOutOfRange => "arity out of range",
            TermError::HeapExhausted => "heap exhausted",
            TermError::InvalidAtomName => "invalid atom name",
            TermError::AtomIndexOutOfRange => "atom index out of range",
            TermError::BadMap => "not a map",
            TermError::BadBitstring => "invalid bitstring byte count",
            TermError::BadFun => "not a fun",
            TermError::BadRef => "not a reference",
            TermError::FunIdentityUnavailable => "fun identity unavailable",
            TermError::RefIdentityUnavailable => "reference identity unavailable",
            TermError::BadRecord => "not a record",
            TermError::NotATerm => "not a term",
            TermError::TermGraphCyclic => "term graph is cyclic",
            TermError::TermGraphTooLarge => "term graph exceeds the size limit",
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
/// | `PidIdOutOfRange` | `BadArg` | `badarg` |
/// | `ArityOutOfRange` | `SystemLimit` | `system_limit` |
/// | `HeapExhausted` | `Alloc(..)` | `out_of_memory` |
/// | `InvalidAtomName` | `BadArg` | `badarg` |
/// | `AtomIndexOutOfRange` | `SystemLimit` | `system_limit` |
/// | `BadMap` | `BadArg` | `badarg` |
/// | `BadBitstring` | `BadArg` | `badarg` |
/// | `BadFun` | `BadArg` | `badarg` |
/// | `BadRef` | `BadArg` | `badarg` |
/// | `FunIdentityUnavailable` | `BadArg` | `badarg` |
/// | `RefIdentityUnavailable` | `BadArg` | `badarg` |
/// | `BadRecord` | `BadArg` | `badarg` |
/// | `NotATerm` | `BadArg` | `badarg` |
/// | `TermGraphCyclic` | `BadArg` | `badarg` |
/// | `TermGraphTooLarge` | `BadArg` | `badarg` |
impl From<TermError> for BeamError {
    fn from(error: TermError) -> Self {
        match error {
            // A 60-bit small-integer overflow is an arithmetic-range failure.
            TermError::SmallIntOutOfRange => BeamError::BadArith,
            // An out-of-range pid id is the oracle's own `badarg` for both C call sites
            // (the decoder's `NULL`, `list_to_pid`'s `bad`), not a new reason.
            TermError::PidIdOutOfRange => BeamError::BadArg,
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
            // A non-reference (or a reference of the wrong on-heap layout) reached a
            // reference layout helper: an argument failure.
            TermError::BadRef => BeamError::BadArg,
            // Missing fun identity is a capability gap, not a BEAM error; the engine
            // raises the same `badarg` the C's `erts_exit`-free paths would (there is
            // no oracle reason to reproduce — the oracle simply never takes this
            // path, because its funs do carry the identity).
            TermError::FunIdentityUnavailable => BeamError::BadArg,
            // Missing reference identity is the same kind of capability gap as
            // `FunIdentityUnavailable` above: the term is well-formed, the node
            // container the C mixes in is simply not carried here.
            TermError::RefIdentityUnavailable => BeamError::BadArg,
            // A malformed native record reached a record layout helper, or a
            // record-construction argument was invalid: an argument failure.
            TermError::BadRecord => BeamError::BadArg,
            // The word is not a term at all (a bare header word): an argument
            // failure — the same `badarg` the engine would raise for a value it
            // cannot use.
            TermError::NotATerm => BeamError::BadArg,
            // A cyclic term graph cannot be sized (there is no such size). It is not a
            // BEAM-level failure the oracle ever raises — a term graph is cyclic only
            // through forged memory — so it maps to the same `badarg` the engine
            // raises for a value it cannot use. Kept distinct from
            // `TermGraphTooLarge` so the caller can tell the two apart *before* the
            // mapping loses the difference.
            TermError::TermGraphCyclic => BeamError::BadArg,
            // A well-formed graph beyond the caller's word budget is likewise an
            // argument failure (an impossible request), not a BEAM error with an
            // oracle reason.
            TermError::TermGraphTooLarge => BeamError::BadArg,
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
        assert_eq!(
            TermError::PidIdOutOfRange.to_string(),
            "internal pid id out of range"
        );
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
        assert_eq!(TermError::BadRef.to_string(), "not a reference");
        assert_eq!(
            TermError::FunIdentityUnavailable.to_string(),
            "fun identity unavailable"
        );
        assert_eq!(
            TermError::RefIdentityUnavailable.to_string(),
            "reference identity unavailable"
        );
        assert_eq!(TermError::BadRecord.to_string(), "not a record");
        assert_eq!(TermError::NotATerm.to_string(), "not a term");
        assert_eq!(
            TermError::TermGraphCyclic.to_string(),
            "term graph is cyclic"
        );
        assert_eq!(
            TermError::TermGraphTooLarge.to_string(),
            "term graph exceeds the size limit"
        );
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
    fn fun_identity_unavailable_is_not_bad_fun() {
        // The whole point of the variant: a well-formed term the operation cannot
        // complete must be distinguishable from a malformed one.
        assert_ne!(TermError::FunIdentityUnavailable, TermError::BadFun);
    }

    #[test]
    fn ref_identity_unavailable_is_not_a_malformed_term() {
        // Same rationale as `fun_identity_unavailable_is_not_bad_fun`: an internal
        // reference is a perfectly well-formed term, it just does not carry the node
        // container the C mixes into its hash — so it must be tellable from "this is
        // not a term" and from the fun gap.
        assert_ne!(TermError::RefIdentityUnavailable, TermError::NotATerm);
        assert_ne!(TermError::RefIdentityUnavailable, TermError::BadFun);
        assert_ne!(
            TermError::RefIdentityUnavailable,
            TermError::FunIdentityUnavailable
        );
    }
    #[test]
    fn bad_ref_is_not_ref_identity_unavailable() {
        // "this is not an M1 internal reference" (a shape failure) must be tellable
        // from "this is one, but its C hash is unreproducible" (an identity gap).
        assert_ne!(TermError::BadRef, TermError::RefIdentityUnavailable);
        assert_ne!(TermError::BadRef, TermError::NotATerm);
        assert_ne!(TermError::BadRef, TermError::BadFun);
    }

    #[test]
    fn term_graph_errors_are_distinguishable() {
        // The whole point of the two variants: "cannot size, it is cyclic" (a
        // malformed graph) must be tellable from "cannot size, it is too big" (a
        // well-formed graph over budget), even though both map to `badarg`.
        assert_ne!(TermError::TermGraphCyclic, TermError::TermGraphTooLarge);
        assert_ne!(TermError::TermGraphCyclic, TermError::NotATerm);
        assert_ne!(TermError::TermGraphTooLarge, TermError::NotATerm);
    }

    #[test]
    fn converts_into_workspace_beam_error() {
        assert!(matches!(
            BeamError::from(TermError::SmallIntOutOfRange),
            BeamError::BadArith
        ));
        assert!(matches!(
            BeamError::from(TermError::PidIdOutOfRange),
            BeamError::BadArg
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
        assert!(matches!(BeamError::from(TermError::BadRef), BeamError::BadArg));
        assert!(matches!(
            BeamError::from(TermError::FunIdentityUnavailable),
            BeamError::BadArg
        ));
        assert!(matches!(
            BeamError::from(TermError::RefIdentityUnavailable),
            BeamError::BadArg
        ));
        assert!(matches!(
            BeamError::from(TermError::BadRecord),
            BeamError::BadArg
        ));
        assert!(matches!(
            BeamError::from(TermError::NotATerm),
            BeamError::BadArg
        ));
        assert!(matches!(
            BeamError::from(TermError::TermGraphCyclic),
            BeamError::BadArg
        ));
        assert!(matches!(
            BeamError::from(TermError::TermGraphTooLarge),
            BeamError::BadArg
        ));
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
