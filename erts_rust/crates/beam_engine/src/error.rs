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

/// The C's `EXC_BADFUN` marker: a host's closure seam reports that the applied fun names **no
/// fun entry** of the module it names.
///
/// `call_fun` (`../../erts/emulator/beam/beam_common.c:1696-1706`) sets `freason = EXC_BADFUN`
/// with `fvalue = fun` when the module **is** loaded and the fun is simply not defined in it;
/// `expand_error_value` (`:750-772`) then wraps the pair as `{badfun, Fun}`. Only the first half
/// can come from a host seam: the fun reaches it as a bare `Eterm`, so the host must not allocate
/// (and thereby possibly collect) before the reason term exists. The second half is therefore the
/// engine's, which holds a re-readable [`FunSource`](crate::families::funs) — see
/// [`is_bad_fun`] and `families::funs::raise_badfun`.
///
/// The marker is a **typed live-reason payload** (`()`), not merely the `badfun` atom: a user
/// `erlang:error(badfun)` raises the same class and atom with `EXC_ERROR` and must never be
/// expanded into `{badfun, Fun}`.
pub fn bad_fun() -> BeamError {
    BeamError::Raise(
        Raise::error(ExitReason::atom("badfun"))
            .with_live_reason(beam_common::LiveReason::new(())),
    )
}

/// Whether `error` is the [`bad_fun`] marker (its reason atom **and** the `()` payload).
pub fn is_bad_fun(error: &BeamError) -> bool {
    match error {
        BeamError::Raise(raise) => {
            raise.reason.as_atom() == Some("badfun")
                && raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<()>())
                    .is_some()
        }
        _ => false,
    }
}
/// The `error_handler:undefined_lambda/3` marker: a host's closure seam reports that the applied
/// fun names a module with **no loadable code** at all — the sibling of [`bad_fun`].
///
/// `call_fun` (`../../erts/emulator/beam/beam_common.c:1696-1715`) splits the two failures
/// precisely here: a module that IS loaded but defines no such fun entry is `EXC_BADFUN`
/// (`{badfun, Fun}`, [`bad_fun`]), while a module with **no current code** is handed to
/// `error_handler:undefined_lambda/3`, which — after `ensure_loaded/1` fails — raises bare
/// `undef` with the applied fun itself as the innermost cooked-stacktrace item
/// (`lib/kernel/src/error_handler.erl`'s `crash(Fun, Args) -> crash({Fun,Args,[]})`).
///
/// Like [`bad_fun`] the marker carries a `()` live-reason payload so it is a **typed**
/// discriminator: a user `erlang:error(undef)` raises the same class and atom and must never be
/// given a synthesized `{Fun, Args, []}` head item. The atom reason itself is the oracle's, so
/// the raise is already correct without the engine; only the head item needs the
/// [`FunSource`](crate::families::funs) the engine holds — see [`is_undefined_lambda`] and
/// `families::funs::fun_apply_error`.
pub fn undefined_lambda() -> BeamError {
    BeamError::Raise(
        Raise::error(ExitReason::atom("undef"))
            .with_live_reason(beam_common::LiveReason::new(())),
    )
}

/// Whether `error` is the [`undefined_lambda`] marker (its reason atom **and** the `()` payload).
pub fn is_undefined_lambda(error: &BeamError) -> bool {
    match error {
        BeamError::Raise(raise) => {
            raise.reason.as_atom() == Some("undef")
                && raise
                    .live_reason()
                    .and_then(|live| live.downcast_ref::<()>())
                    .is_some()
        }
        _ => false,
    }
}

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

/// The typed error a **non-blocking** run surfaces when a BIF answered
/// [`crate::BifOutcome::Suspend`].
///
/// The blocking-aware entries ([`crate::Machine::run_export_outcome`]/
/// [`crate::Machine::run_label_outcome`]/[`crate::Machine::resume_outcome`] and their
/// `*_with_tier` twins) report the park as a [`crate::RunOutcome::Suspend`] instead;
/// the plain [`crate::Machine::run_export`]/[`crate::Machine::run_label`] (and the JIT
/// backends' `run_export`) fold that outcome into this error, because a non-blocking
/// entry cannot suspend the process and must never invent a result.
///
/// It is deliberately **not** [`blocked`]: a suspended BIF is not a `receive` with an
/// empty mailbox, and the two must stay distinguishable to a host reading the reason
/// atom. Like every other engine diagnostic it is a [`BeamError::Raise`] whose reason
/// atom reads `unsupported_opcode: run: process suspended by a BIF (host must resume)`;
/// this constructor is the **single definition** of that string.
pub fn suspended() -> BeamError {
    unsupported("run: process suspended by a BIF (host must resume)")
}