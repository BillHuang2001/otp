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

//! Failure constructors for the crate.
//!
//! There is **no crate-local error enum** (workspace `CONTEXT.md` §3): every
//! fallible op returns [`beam_common::Result`], i.e. [`beam_common::BeamError`].
//! This module only centralises the constructors for the failures the table ops
//! raise, so the reason each maps to is stated in exactly one place.
//!
//! These helpers are `pub(crate)` — they are implementation details of the
//! crate, not part of its public API.

use beam_common::{BeamError, ExitReason, Raise};

/// The `badarg` failure of a malformed table operation (bad object, bad
/// `keypos`, missing element, unsupported match spec, …).
pub(crate) fn badarg() -> BeamError {
    BeamError::BadArg
}

/// The typed **"unsupported operation"** raise: the operation has no answer in
/// this crate's M1 coverage, so it must never return a *wrong* one.
///
/// It is a BEAM `error`-class raise whose reason atom is
/// `unsupported_ets: <what>` — the crate's counterpart of
/// `beam_engine::error::unsupported`/`beam_bifs::error::unsupported_bif`. Silence
/// is not an option (workspace §3), and neither is fabricating a result: a caller
/// that gets this error knows the table op is outside the implemented subset.
pub(crate) fn unsupported(what: &str) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(format!(
        "unsupported_ets: {what}"
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::ExceptionClass;

    #[test]
    fn badarg_is_the_badarg_error() {
        assert!(matches!(badarg(), BeamError::BadArg));
        // The workspace error type reports the BEAM error atom for it.
        assert_eq!(badarg().as_atom(), Some("badarg"));
    }

    #[test]
    fn unsupported_is_an_error_class_raise_naming_the_operation() {
        let err = unsupported("ets:lookup_element/3 on a bag");
        assert!(err.is_raise(), "the unsupported marker is an explicit raise");
        assert!(!matches!(err, BeamError::BadArg));
        let raise = err.into_raise().expect("unsupported carries a raise");
        assert_eq!(raise.class, ExceptionClass::Error);
        // The reason atom is built from the operation name, so it stays greppable.
        assert_eq!(
            raise.reason,
            ExitReason::atom("unsupported_ets: ets:lookup_element/3 on a bag")
        );
    }
}
