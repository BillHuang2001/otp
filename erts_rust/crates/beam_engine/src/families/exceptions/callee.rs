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

//! The **synthesized callee head item** of an `undef` raise.
//!
//! C's `build_stacktrace` (`../../erts/emulator/beam/beam_common.c:1204-1292`)
//! resolves the exception's saved raw `StackTrace` through
//! `erts_lookup_function_info`, and the **innermost** item it builds carries the
//! exception's saved **arguments** — `erts_build_mfa_item`'s `args`
//! (`../../erts/emulator/beam/utils.c:3430-3457`) is the argument *list*, not the
//! arity the engine's metadata-only raise snapshot can supply. Real OTP depends on
//! that: an `undef` names its missing callee **only** through the trace, so its
//! first item is `{Module, Function, ArgList, []}` and the whole
//! `logger_server:call_h/4` probe (`lib/kernel/src/logger_server.erl:537-561`)
//! matches on exactly that shape:
//!
//! ```text
//! try apply(M, F, A) catch error:undef:[{M, F, Args, _} | _] -> ... end
//! ```
//!
//! The engine's raise snapshot is `(context, func, pc)` metadata, so its items can
//! never carry an argument list (see [`super::stacktrace`], residual 2). The sites
//! that raise the `undef` **do** know the callee and its arguments — a `call_ext*`
//! names it in the import table and has its arguments in `x(0..arity)`, and the
//! `apply`/`apply(M,F,A)` register shape names it in `x(arity)`/`x(arity+1)` with
//! the arguments in `x(0..arity)` — so those sites materialize the one item the
//! snapshot cannot:
//!
//! ```text
//! {CalleeModule, CalleeFunction, ArgList, []}
//! ```
//!
//! (`Location` is the **empty list**: the callee does not exist, so there is no
//! source position to name — which is what the oracle prints.) The item travels as
//! the raise's own **raw-trace word** (the `trace` argument of
//! `Machine::throw_with_trace`), never as a machine-held heap term: the raw trace
//! is published in `x(2)` and the compiled handler moves it into `x(0)`, so it is
//! inside the frozen root set and is relocated in place by every collection
//! ([`crate::context::RootSet`]). [`Machine::raise_bif_error_with_head`] raises the
//! item through the normal funnel and arms the one-shot `pending_callee_head`
//! latch immediately before the `throw_with_trace` that lands it; `build_stacktrace`
//! then conses it in front of the snapshot's frames (see `symbolic_stacktrace` in
//! [`super::stacktrace`]).
//!
//! **Never a fabricated frame.** The item is built *only* at a real failing call
//! site, from that site's own callee identity and the callee's **real** arguments,
//! and only for a raise that can actually land (a live catch frame). Anything else
//! keeps today's behaviour — an unmodified raise.

use beam_common::{BeamError, Result};
use beam_loader::{ImportRef, Module};
use beam_term::{make_atom, Eterm, NIL};

use crate::context::ExecutionContext;
use crate::machine::Machine;
use crate::terms::{self, Sink};

/// Whether `error` is the `undef` an unresolvable call target raises: class `error`
/// with the **bare reason atom** `undef`.
///
/// The host's shape is `beam_rt`'s `error::undef()`
/// (`BeamError::Raise(Raise::error(ExitReason::atom("undef")))`), whose reason is an
/// atom and therefore names itself through **both** `Raise::reason` (the
/// `ExitReason`, whose `as_atom` also sees an owned-`String`-backed name) and — for
/// a `&'static str`-backed name — [`BeamError::as_atom`]. Both are consulted, so the
/// gate does not depend on which representation the host used.
pub(crate) fn is_unresolved_undef(error: &BeamError) -> bool {
    if error.as_atom() == Some("undef") {
        return true;
    }
    matches!(error, BeamError::Raise(raise) if raise.reason.as_atom() == Some("undef"))
}

/// The callee `(module, function)` **atom terms** the import `target` names, or
/// `None` when the import reference is out of range (nothing to name).
///
/// An atom is an immediate, so the two terms are safe to hold across the rooted
/// reservation [`callee_head_item`] takes — they can never move.
pub(crate) fn import_callee_atoms(module: &Module, target: ImportRef) -> Option<(Eterm, Eterm)> {
    let import = module.imports.get(target.0 as usize)?;
    Some((make_atom(import.module), make_atom(import.function)))
}

/// [`callee_head_item`] for a `call_ext*`'s import reference: the callee identity
/// comes from `module.imports[target]`, and `argc` is the instruction's own arity
/// operand (the arguments it marshalled from `x(0..argc)`).
///
/// `None` when the import reference is out of range — there is no callee to name, so
/// the raise keeps today's behaviour.
pub(crate) fn import_callee_head_item(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    module: &Module,
    target: ImportRef,
    argc: u32,
    error: &BeamError,
) -> Result<Option<Eterm>> {
    match import_callee_atoms(module, target) {
        Some((callee_module, callee_function)) => {
            callee_head_item(machine, ctx, error, callee_module, callee_function, argc)
        }
        None => Ok(None),
    }
}

/// The synthesized innermost cooked-stacktrace item for an `undef` raised by the
/// call `callee_module:callee_function/argc`, or `None` when nothing must be
/// synthesized.
///
/// `callee_module`/`callee_function` are **atom immediates** (they never move — safe
/// across the reservation below); the callee's real arguments are the registers
/// `x(0..argc)` and are read **after** it, honoring the frozen reserve → read →
/// write ordering (a moving collector may run under the one rooted reservation).
///
/// Returns `None` — today's behaviour, an unmodified raise — when any of:
///
/// * `error` is not the bare `undef` ([`is_unresolved_undef`]);
/// * **no catch frame is live**, so the raise cannot land and the cooked trace is
///   never reached (the *uncaught* path derives its frames from the raise snapshot,
///   which this module deliberately does not touch);
/// * the callee's arguments are not readable as registers (a malformed call, not a
///   runtime value) — nothing is fabricated in their place.
///
/// Never fabricates a frame: the item is built only from a real failing call site's
/// own callee identity and its **real** arguments.
pub(crate) fn callee_head_item(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    error: &BeamError,
    callee_module: Eterm,
    callee_function: Eterm,
    argc: u32,
) -> Result<Option<Eterm>> {
    if !is_unresolved_undef(error) || machine.catch_frame_depth() == 0 {
        return Ok(None);
    }
    // A 4-element tuple (`5` words) plus one cons cell (`2`) per argument — the
    // whole graph in ONE rooted reservation.
    let total = 2 * argc as usize + 5;
    let mut sink = terms::reserve_slack(machine, ctx, total)?;
    // Post-collection read: the callee's real arguments, in order. An unreadable
    // register is not a runtime value (`argc` comes from the instruction), so the
    // honest answer is to synthesize nothing and keep the raise as it was.
    let mut args = Vec::with_capacity(argc as usize);
    for i in 0..argc {
        match machine.x(i) {
            Some(value) => args.push(value),
            None => return Ok(None),
        }
    }
    // Children before the parent, in the order `total` counts: the argument list
    // (`2 * argc` words) then the item's own 4-element tuple (`5`).
    let arg_list = sink.list(&args)?;
    let item = sink.tuple(&[callee_module, callee_function, arg_list, NIL])?;
    Ok(Some(item))
}
