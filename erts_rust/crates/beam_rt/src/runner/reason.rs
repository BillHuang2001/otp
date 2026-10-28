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
//!
//! # Two reasons for one death (the bare one and the signalled one)
//!
//! ERTS keeps two renderings of an abnormal termination and they are **not** the same term
//! for every class:
//!
//! * the **bare** reason ([`exit_reason`]) — the raise's own reason, untouched by its class;
//!   this is what the `=ERROR REPORT====` block's `Error in process … with exit value:`
//!   line renders as `{Reason, Stacktrace}` ([`crate::report`]);
//! * the **signalled** reason ([`signalled_reason`]) — the *exit value* itself, i.e. what a
//!   linked peer's `{'EXIT', Pid, R}` and a monitor's `{'DOWN', …}` carry: the same
//!   `{Reason, Stacktrace}` pair for an `error` class, `{{nocatch, Value}, Stacktrace}` for
//!   an uncaught `throw`, and the bare reason for an `exit` class.
//!
//! Both are built from [`crate::report::exit_value`] — the one definition of ERTS's exit
//! value — so the report and the signal can never disagree about one death.

use beam_alloc::Heap;
use beam_common::{BeamError, ExceptionClass, Result};
use beam_term::Eterm;

use crate::report;
use crate::runtime::Runtime;

/// The `normal` exit-reason term (the reason a process that returns exits with).
pub(super) fn normal(runtime: &mut Runtime) -> Eterm {
    runtime.intern("normal")
}

/// The **bare atom `killed`** — the reason a process terminated by its `max_heap_size` bound
/// exits with.
///
/// This is the C's `ERTS_MAX_HEAP_SIZE_KILL` self-exit (`erl_gc.c`'s `check_max_heap_size`
/// kills the process with the atom `killed`, **not** `kill`): the termination is performed by
/// the runner through the ordinary exit path (the ordinary `ProcessTable::terminate`), so it is
/// untrappable by the victim — no `{'EXIT', …}` signal is delivered to it — while a linked peer
/// that traps exits observes `{'EXIT', Pid, killed}` and a monitor `'DOWN' … killed`.
pub(super) fn killed(runtime: &mut Runtime) -> Eterm {
    runtime.intern("killed")
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
    // A structured reason travels as a live term (the `beam_common::Raise` convention).
    //
    // The slot holds a **runtime-lifetime** term: the heap that built it — the raising
    // process's own young heap, or the pooled BIF region (rewound after every call) — is gone by
    // the time a reader gets here, so the producer stabilized the term at the escape boundary
    // (`Runtime::stabilize_error_live_reason`, called by this module's `Runner` arm and by
    // `Runtime::as_action_process`) *before* the owning process was terminated. Return it
    // directly — no reshaping, no loss, and no stale pointer to dereference.
    if let Some(term) = crate::error::live_reason_term(error) {
        return term;
    }
    runtime.intern(&error.to_string())
}

/// The reason term a dying process **signals** to its links and monitors.
///
/// This is ERTS's *exit value* — the term the C's `terminate_proc` hands to
/// `erts_dsig_send_exit`/the monitor machinery, not the raw raise reason:
///
/// * an **`error`**-class raise ⇒ `{Reason, Stacktrace}`;
/// * an uncaught **`throw`** ⇒ `{{nocatch, Value}, Stacktrace}` (the C promotes a throw that
///   reaches the top of the stack to that error, `beam_common.c:508-516`);
/// * an **`exit`**-class raise ⇒ the reason **bare** (`bye`, `normal`, `shutdown`, `kill`, …) —
///   unchanged from [`exit_reason`];
/// * a **non-raise** [`BeamError`] (runtime plumbing with no `Raise`, hence no exception class
///   and no stacktrace) ⇒ the bare interned [`Display`](std::fmt::Display) text, i.e. today's
///   shape. This asymmetry is deliberate: the exit value's `{Reason, Stacktrace}` pair is a
///   property of an *exception*, and a plumbing failure has none, so inventing an empty
///   stacktrace for it would fabricate a shape the oracle never produces.
///
/// The **base** reason comes from [`exit_reason`], so the atom/structured/plumbing rules are
/// defined once. The pair is built with [`crate::report::exit_value`] — the same builder the
/// crash report renders — on a private [`Heap`], and copied onto the runtime-lifetime `-eval`
/// store ([`Runtime::stabilize_term`]) **while that heap is still alive**: the term points into
/// the heap's block, so the copy is what lets the reason outlive both the heap and the dying
/// process's own heap.
///
/// # Errors
///
/// The private heap's allocation failure, [`crate::report::exit_value`]'s, or the copy's
/// (`error:system_limit`). A caller must **not** turn such a failure into a lost death: use
/// [`signalled_or_bare`], which falls back to the bare reason and reports the failure through
/// the env-gated [`crate::debug_trace`] channel — the same rule
/// [`crate::report::report_crash`] documents for a report it cannot build.
pub(super) fn signalled_reason(runtime: &mut Runtime, error: &BeamError) -> Result<Eterm> {
    let bare = exit_reason(runtime, error);
    build_signalled_value(runtime, error, bare)
}

/// [`signalled_reason`] with the documented fallback: the built exit value, else `bare`.
///
/// A death is never lost and no shape is fabricated — an exit value that cannot be built
/// (allocation failure, `error:system_limit`) degrades to the bare reason, which is a faithful
/// and always-available term, and the failure is surfaced through the env-gated
/// [`crate::debug_trace`] channel ([`crate::report::report_crash`]'s rule for a report it
/// cannot build).
pub(super) fn signalled_or_bare(bare: Eterm, built: Result<Eterm>) -> Eterm {
    match built {
        Ok(term) => term,
        Err(failure) => {
            crate::debug_trace::trace(format_args!(
                "the signalled exit value could not be built ({failure:?}); \
                 signalling the bare reason instead"
            ));
            bare
        }
    }
}

/// The fallible half of [`signalled_reason`]: the exit value for `error`, or the bare reason
/// for the two shapes that have none.
fn build_signalled_value(
    runtime: &mut Runtime,
    error: &BeamError,
    bare: Eterm,
) -> Result<Eterm> {
    let BeamError::Raise(raise) = error else {
        // No `Raise`, so no exception class and no stacktrace (see `signalled_reason`).
        return Ok(bare);
    };
    let class = raise.class;
    if !matches!(class, ExceptionClass::Error | ExceptionClass::Throw) {
        // The `exit` class signals its reason bare, and a class this runtime does not know is
        // signalled with the reason it carries rather than reshaped (`report::exit_value`'s
        // rule for its own `_` arm).
        return Ok(bare);
    }
    let frames = raise.stacktrace.frames.as_slice();
    // The heap owns the words the built term points into, so it must outlive the copy and nothing
    // else: the local is released when this function returns, after `stabilize_term` has walked
    // the block into the runtime store.
    let mut heap = Heap::new(report::term_words(frames))?;
    let value = report::exit_value(runtime, &mut heap, class, bare, frames)?;
    runtime.stabilize_term(value)
}
