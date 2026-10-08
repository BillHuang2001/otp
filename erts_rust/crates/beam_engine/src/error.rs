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

//! Engine error constructors over the workspace error type.
//!
//! There is **no crate-local error enum** (workspace §3): every fallible engine
//! operation returns [`beam_common::Result`] (i.e. `Result<T, BeamError>`) and the
//! constructors here build the handful of engine-specific [`beam_common::BeamError`]
//! values the interpreter needs (an unsupported opcode/operand, an out-of-range
//! register, an unknown jump target).

use beam_common::{BeamError, ExitReason, Raise};

/// The engine cannot execute this instruction/operand (a Milestone-1 coverage
/// limit).
///
/// Raises a BEAM `error` with the reason atom `unsupported_opcode: <what>`; it is
/// the documented sentinel for the not-yet-implemented opcode/operand subset and
/// is **never** a panic.
pub fn unsupported(what: &str) -> BeamError {
    BeamError::Raise(Raise::error(ExitReason::atom(format!(
        "unsupported_opcode: {what}"
    ))))
}

/// An X-register index outside the register file.
///
/// Reported as the BEAM `badarg` error atom. [`BeamError::BadArg`] carries no
/// payload, so `n` is only consumed here to keep the offending index at the
/// constructor's signature; the interpreter may log it at the call site.
pub fn bad_register(n: u32) -> BeamError {
    let _ = n;
    BeamError::BadArg
}

/// A jump to a label not present in the module.
///
/// Reported as the BEAM `badarg` error atom; [`beam_common::BeamError::BadArg`]
/// carries no payload, so the offending `label` is consumed only to preserve the
/// constructor's signature.
pub fn unknown_label(label: beam_loader::Label) -> BeamError {
    let _ = label;
    BeamError::BadArg
}

/// The typed error a **non-blocking** run surfaces when a `receive` finds no
/// message available.
///
/// The blocking-aware entries ([`crate::Machine::run_export_outcome`]/
/// [`crate::Machine::run_label_outcome`]) report the suspension as a
/// [`crate::RunOutcome::Wait`] instead; the plain [`crate::Machine::run_export`]/
/// [`crate::Machine::run_label`] fold that outcome into this error, because a
/// non-blocking entry cannot suspend and must never invent a result.
///
/// Like every other engine diagnostic it is a [`BeamError::Raise`] whose reason
/// atom reads `unsupported_opcode: run: process blocked (no message available)`;
/// this constructor is the **single definition** of that string.
pub fn blocked() -> BeamError {
    unsupported("run: process blocked (no message available)")
}
