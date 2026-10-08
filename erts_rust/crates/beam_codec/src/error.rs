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

/// The error-reason atom raised for a **local** fun (`make_fun3` closure) that
/// cannot be encoded to the wire.
///
/// A local fun's inlined descriptor carries only a `uniq` and an `index`; it has
/// no `module`, no 16-byte MD5 `uniq` and no creator pid, so no `NEW_FUN_EXT`
/// image can represent it faithfully. Reported through
/// [`unsupported_local_fun`] — never a mis-encoded term — and kept distinct from
/// [`UNSUPPORTED_REASON`] so a caller can tell "this fun kind has no wire form"
/// apart from "this tag is not implemented yet".
pub const LOCAL_FUN_REASON: &str = "unsupported_local_fun";

/// Builds the error for a local fun that has no faithful `NEW_FUN_EXT` encoding.
///
/// Returns `BeamError::Raise(Raise::error(ExitReason::atom(LOCAL_FUN_REASON)))`;
/// detectable with [`is_unsupported_local_fun`]. [`unsupported`] and its
/// [`UNSUPPORTED_REASON`] are unchanged and remain the general
/// "recognized tag not implemented" signal.
pub fn unsupported_local_fun() -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(LOCAL_FUN_REASON)))
}

/// Whether `err` is the local-fun encoding error built by
/// [`unsupported_local_fun`].
pub fn is_unsupported_local_fun(err: &BeamError) -> bool {
    match err {
        BeamError::Raise(raise) => raise.reason.as_atom() == Some(LOCAL_FUN_REASON),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_unsupported_reasons_are_distinct_and_detectable() {
        // The general "tag not implemented" signal is unchanged.
        assert!(is_unsupported(&unsupported(Tag::Record)));
        assert!(!is_unsupported_local_fun(&unsupported(Tag::Record)));

        // The local-fun signal is its own reason atom, kept apart from the general one.
        assert!(is_unsupported_local_fun(&unsupported_local_fun()));
        assert!(!is_unsupported(&unsupported_local_fun()));
        assert_ne!(LOCAL_FUN_REASON, UNSUPPORTED_REASON);

        // Neither detector fires on a plain error.
        assert!(!is_unsupported(&bad_arg()));
        assert!(!is_unsupported_local_fun(&bad_arg()));
        assert!(!is_unsupported(&system_limit()));
        assert!(!is_unsupported_local_fun(&system_limit()));
    }
}
