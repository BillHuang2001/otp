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
//! The baseline tier's **frame-preserving local call** lowering: the continuation store
//! for `Instr::Call`.
//!
//! The **tail** forms (`call_last` / `call_only`) need no continuation: they reuse the
//! caller's frame and are linked natively by `super::emit::link`. The
//! frame-**preserving** `call` instead writes a `Continuation::Address` into the live
//! frame — Rust-side `Machine` state a compiled block cannot address — so its store goes
//! through the helper here ([`jit_call_store`]) while the transfer to the callee stays
//! the block linker's (`super::emit::link`).
//!
//! # What the helper reproduces
//!
//! The interpreter's own arm, `src/interpreter/dispatch.rs:371` *verbatim* — the one
//! definition of a local `call`'s semantics:
//!
//! ```text
//! Instr::Call { target, arity } => {
//!     let (callee, callee_pc) = resolve(labels, *target)?;      // 1
//!     if let Some(step) = self.local_call_seam(                  // 2
//!         entry_module, module, callee, *arity,
//!         ImportFrame::Call, func, pc, ctx)? { return Ok(step); }
//!     self.set_continuation(Continuation::Address {               // 3
//!         func,                       // the CALLING function index
//!         pc: pc + 1,                 // the instruction after the `call`
//!         context: self.current_context(),   // the callee's own module context
//!     })?;
//!     if self.trace_on { self.trace_call_local(module, callee, ctx); }   // 4
//!     Ok(Step::Goto { func: callee, pc: callee_pc })              // 5
//! }
//! ```
//!
//! Step 3 is the whole reason this helper exists: the stored address names the **call
//! site** (`func` = the calling function, `pc + 1` = the next instruction) tagged with
//! the context executing **now** (the caller's, which for a same-module call is the
//! callee's context too), so the callee's eventual `return` comes back to `pc + 1` in
//! `func`'s code. `arity` is *not* part of the store — the interpreter only hands it to
//! the local-call seam (step 2) as the callee's register-argument count.
//!
//! Step 2 is the **host seam** (`ExecutionContext::resolve_local_call`): a host that
//! serves this same-module callee as a BIF makes the callee's own code never run, so the
//! continuation must **not** be stored. The helper therefore asks the seam before it
//! stores anything, on the same run-time terms the tier's two tail forms probe it
//! (`super::runtime::jit_local_call`) — the code cache is shared between runs and hosts,
//! so whether a callee is served can only be decided while the block runs, never at
//! compile time.
//!
//! Step 5 is the block linker's transfer and step 4 the (natively-suppressed) A.8 trace
//! event: a traced run never enters native code at all, so there is nothing to emit
//! here.
//!
//! # The block ABI words
//!
//! * [`CALL_STORED`] — the continuation is in place; the emitted block transfers to the
//!   callee (steps 5's transfer, through the linker).
//! * the folded [`Step`](crate::Step) of a **served** callee (step 2 returned one), or
//!   the interpreter's own typed diagnostic — the block leaves for the driver with that
//!   word, exactly as it leaves with a deopt's.
//!
//! A `(func, pc)` that does not name a `call`, or a target the label map cannot resolve,
//! is handed to [`jit_step_one`] — the interpreter's own step reports *its* exact
//! diagnostic; nothing is ever invented.

#![allow(unsafe_code)]

use beam_common::BeamError;
use beam_loader::{Instr, Module};

use crate::context::ExecutionContext;
use crate::interpreter::{ImportFrame, LabelMap};
use crate::machine::{Continuation, Machine};
use crate::Step;

use super::jit_step_one;
use super::runtime::{fold_step, JitRuntime};

/// The block-ABI word [`jit_call_store`] answers when it has **stored the
/// continuation**: "the `Call`'s `Continuation::Address` is in the live frame — transfer
/// control to the callee".
///
/// It needs a word of its own rather than the block's usual "advanced by one"
/// continuation (`encode_target(func, pc + 1)`): a **served** non-tail local call folds
/// its `Step::Next` to exactly that word (`Machine::apply_import_target` →
/// `ImportFrame::Call` + a synchronous value), and the two must not be confused — one
/// transfers to the callee, the other continues at the next instruction without ever
/// entering the callee. Negative, so it can never collide with a real
/// [`encode_target`](super::encode_target) continuation, and disjoint from every other
/// block-ABI sentinel ([`HALT`](super::HALT), [`ERROR`](super::ERROR),
/// [`ENTER`](super::ENTER), [`LOCAL_DECLINE`](super::LOCAL_DECLINE),
/// [`WAIT_BIT`](super::WAIT_BIT)). The emitted arm consumes it **inside the block** (it
/// is the fall-through condition of the frame-only continuation check), so it never
/// reaches the driver's word decoder.
pub(crate) const CALL_STORED: i64 = -5;

/// The helper's three answers, in ordinary Rust types (see [`store_continuation`]).
enum CallOutcome {
    /// The address does not name a `call`, or its target is unresolvable: let
    /// [`jit_step_one`] report the interpreter's exact diagnostic.
    Deopt,
    /// The continuation is stored ([`CALL_STORED`]).
    Stored,
    /// Hand this folded step (a **served** callee's, or the interpreter's own error
    /// variant) back to the driver.
    Fold(std::result::Result<Step, Box<BeamError>>),
}

/// Store the continuation of the local `call` at `(func, pc)` — or report what to do
/// instead.
///
/// Emitted by the block for `Instr::Call` (see `super::emit`). It has the
/// [`StepHelper`](super::runtime::StepHelper) signature, so the arm drives it through
/// the shared `emit::emit_helper_call` sequence and then branches on its answer:
///
/// * [`CALL_STORED`] — the continuation is in place; the block transfers to the callee;
/// * any other word — either the host served the same-module callee as a BIF (so the
///   callee's own code must not run, and the continuation was deliberately not stored)
///   or the instruction could not be handled here at all; the block leaves for the
///   driver with that word, whose encoding is [`fold_step`]'s — the **one** definition
///   of the block-ABI encoding.
pub(crate) extern "C" fn jit_call_store(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    let outcome = {
        // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
        // `JitRuntime` and never aliases it for the duration of the call; every pointer
        // it holds points at an object the driver owns and keeps alive for the whole
        // loop (`machine`, `module`, `entry_module`), or at a local (`labels`) that
        // outlives the loop.
        let runtime = unsafe { &mut *rt };
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live
        // `Machine`; no other `&mut Machine` exists while the helper runs (the driver is
        // blocked in the block call that reached us).
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.module` points at the module the driver is running, which
        // outlives the loop.
        let module = unsafe { &*runtime.module };
        // SAFETY: `runtime.entry_module` points at the run's entry module, which the
        // driver holds alive for the whole loop.
        let entry_module = unsafe { &*runtime.entry_module };
        // SAFETY: `runtime.labels` points at the driver's `labels` local, which outlives
        // the loop.
        let labels = unsafe { &*runtime.labels };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
        // a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        store_continuation(machine, entry_module, module, labels, func, pc, ctx)
    };
    match outcome {
        CallOutcome::Deopt => jit_step_one(rt, func, pc),
        CallOutcome::Stored => CALL_STORED,
        CallOutcome::Fold(step) => {
            // SAFETY: `rt` is the same live, exclusively borrowed `JitRuntime` the driver
            // owns; the `&mut` derived from it above is dead by now, so this borrow does
            // not alias it.
            let runtime = unsafe { &mut *rt };
            fold_step(runtime, step, func, pc)
        }
    }
}

/// The Rust body of [`jit_call_store`] — separate from the `extern "C"` helper so the
/// exit path's borrows are provably dead before [`fold_step`] takes the runtime again
/// (the same split [`jit_step_one`]/[`jit_local_call`](super::jit_local_call) use).
fn store_continuation(
    machine: &mut Machine,
    entry_module: &Module,
    module: &Module,
    labels: &LabelMap,
    func: u32,
    pc: u32,
    ctx: &mut dyn ExecutionContext,
) -> CallOutcome {
    // The instruction this block emitted the helper for. An address outside the module
    // cannot be here (the block was compiled from this very code), and any other
    // instruction is not this helper's — both are handed to the interpreter's own step
    // rather than invented; it reports the interpreter's exact diagnostic.
    let (target, arity) = match module
        .functions
        .get(func as usize)
        .and_then(|function| function.code.get(pc as usize))
    {
        Some(Instr::Call { target, arity }) => (*target, *arity),
        _ => return CallOutcome::Deopt,
    };
    // Step 1: the module-local label, resolved through the same per-run label map the
    // interpreter uses. An unresolvable label is the interpreter's own `unknown_label`.
    let callee = match labels.resolve(target) {
        Ok((callee, _callee_pc)) => callee,
        Err(_) => return CallOutcome::Deopt,
    };
    // Step 2: the host's local-call seam, **before** anything is stored — a served
    // callee's code never runs, so it must not be left a return address.
    match machine.local_call_seam(
        entry_module,
        module,
        callee,
        arity,
        ImportFrame::Call,
        func,
        pc,
        ctx,
    ) {
        Err(err) => CallOutcome::Fold(decorate(machine, module, Err(err), func, pc)),
        Ok(Some(step)) => CallOutcome::Fold(decorate(machine, module, Ok(step), func, pc)),
        // Step 3: the continuation store, byte for byte what the interpreter's arm
        // stores — the call site (`func`, `pc + 1`) tagged with the context executing
        // now. `arity` plays no part in it.
        Ok(None) => {
            // The exact suspension/raise position an interpreted `call` records: its own
            // instruction address (`Machine::step_instruction` stores it before it runs
            // the arm). Declined, the transfer into the callee immediately re-stores the
            // callee's own block position, so this is invisible on that path; served, it
            // is what keeps a raise from inside the seam's BIF at the instruction's own
            // granularity. See [`jit_receive`](super::receive::jit_receive).
            machine.set_current_code(func, pc);
            let continuation = Continuation::Address {
                func,
                pc: pc + 1,
                context: machine.current_context(),
            };
            match machine.set_continuation(continuation) {
                Ok(()) => CallOutcome::Stored,
                Err(err) => {
                    CallOutcome::Fold(decorate(machine, module, Err(Box::new(err)), func, pc))
                }
            }
        }
    }
}

/// Apply the uniform raise funnel's **entry-context** half to the `call`'s outcome —
/// exactly what `Machine::step_instruction` does around the instruction it executes (and
/// what `super::runtime`'s `decorate` does for the probed local tail calls): in the
/// run's entry context (`0`) the active module IS the entry module, so a propagating
/// raise is decorated here; a non-zero context is left to the driver, which knows the
/// true entry module.
fn decorate(
    machine: &mut Machine,
    module: &Module,
    step: std::result::Result<Step, Box<BeamError>>,
    func: u32,
    pc: u32,
) -> std::result::Result<Step, Box<BeamError>> {
    if machine.current_context() == 0 {
        step.map_err(|err| Box::new(machine.with_uncaught_frames(*err, Some(module), func, pc)))
    } else {
        step
    }
}
