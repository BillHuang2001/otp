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

//! The **exit-reason translation**.
//!
//! When a slice ends, the runner has to name the *reason term* a failing process exits
//! with ([`exit_reason`], plus the `normal` atom a returning process exits with via
//! [`normal`]). The finite `receive … after T` deadline of a process that just parked is
//! **not** here: it is recovered by the engine's cross-module-aware
//! [`Machine::wait_deadline`](beam_engine::Machine::wait_deadline).

use beam_common::BeamError;
use beam_term::Eterm;

use crate::runtime::Runtime;

/// The `normal` exit-reason term (the reason a process that returns exits with).
pub(super) fn normal(runtime: &mut Runtime) -> Eterm {
    runtime.intern("normal")
}

/// The exit-reason **term** for a `BeamError` raised while running a process.
///
/// An error that carries an atom reason — the standard `badarg`/`badarith`/… atoms, or a
/// raise whose reason is an atom (`unsupported_opcode: …`, `badmatch`, …) — becomes that
/// atom. A raise that carries a **structured** reason as a live term (`{badmap, M}`,
/// `{badkey, K}`, …) becomes that very term, so the process's exit reason is the real
/// value the producer built rather than a reshaped atom. Anything else (runtime plumbing
/// such as `TermNotFound`) has no atom of its own, so its
/// [`Display`](std::fmt::Display) text is interned as the reason instead: the failure
/// stays observable and is never silently replaced by a plausible-looking value.
pub(super) fn exit_reason(runtime: &mut Runtime, error: &BeamError) -> Eterm {
    if let Some(name) = error.as_atom() {
        return runtime.intern(name);
    }
    if let BeamError::Raise(raise) = error {
        if let Some(name) = raise.reason.as_atom() {
            return runtime.intern(name);
        }
    }
    // A structured reason travels as a live term (the `beam_common::Raise` convention;
    // the payload is the reason as an `Eterm`, built on a BIF heap that M1 never
    // resets or moves). Return it directly — no reshaping, no loss.
    if let Some(term) = crate::error::live_reason_term(error) {
        return term;
    }
    runtime.intern(&error.to_string())
}
