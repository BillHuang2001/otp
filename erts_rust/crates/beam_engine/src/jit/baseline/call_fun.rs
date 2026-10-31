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
//! The baseline tier's **fun-application** helper: the in-block lowering for
//! `Instr::CallFun`/`Instr::CallFun2`, running the fun family's own
//! [`crate::families::funs::call_fun_register`] body directly.
//!
//! Left to the generic family arm ([`super::family::jit_family`]) a closure
//! application paid the `jit_family` deopt **and** the
//! [`crate::families::dispatch`] variant re-match before reaching the same body.
//! The emitter resolves the fun's register slot and the call arity at compile time,
//! so this helper can hand them straight to the family's register-source entry —
//! the same [`call_fun`](crate::families::funs::call_fun) the interpreter's arm
//! reaches — and the whole application runs in ONE emitted call.
//!
//! [`jit_call_fun`]: self::jit_call_fun
#![allow(unsafe_code)]

use super::jit_step_one;
use super::runtime::{fold_err, fold_ok_step, JitRuntime};

/// Run the fun application at `(func, pc)` natively and encode its outcome as a
/// block-ABI word.
///
/// The helper has the [`CallFunHelper`](super::runtime::CallFunHelper) signature, so
/// the emitter's `call_fun` arm drives it through the shared
/// [`emit_helper_call`](super::emit::emit_helper_call) /
/// [`emit_continue_check`](super::emit::emit_continue_check) sequence: a
/// [`Step::Next`](crate::Step::Next) (`FunOutcome::Return`'s value store) continues
/// in-block, an [`Step::Enter`](crate::Step::Enter) (a closure the host runs in this
/// machine) leaves the [`ENTER`](super::runtime::ENTER) word for the driver, a
/// [`Step::Suspend`](crate::Step::Suspend) parks on this very instruction, and every
/// raise leaves the decorated [`ERROR`](super::runtime::ERROR) word.
///
/// The single implementation is shared by `call_fun` and the x-register form of
/// `call_fun2`; the emitter declines every other `call_fun2` operand shape back to
/// the generic family arm, so this helper only ever runs the family's register
/// source with the fun register the emitter baked in.
pub(crate) extern "C" fn jit_call_fun(
    rt: *mut JitRuntime<'_>,
    func: u32,
    pc: u32,
    fun_register: u32,
    arity: u32,
) -> i64 {
    // Structural counter: this helper ran (the native arm the emitter emitted).
    #[cfg(test)]
    super::counters::record_call_fun_entered();
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer
    // it holds points at an object the driver owns and keeps alive for the whole
    // loop (`machine`, `module`), or at a local (`labels`) that outlives the loop.
    let runtime = unsafe { &mut *rt };
    // The family body's outcome, kept as the family's own `Result<Step, BeamError>` —
    // never boxed, and never converted into a `Result` over a 72-byte error union a
    // second time.
    let outcome = {
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
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
        // a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        // The same position store `Machine::step_instruction` makes at entry: a fun
        // application is an exact suspension and raise position, so the machine's
        // saved instruction pointer must name this instruction, not the block.
        machine.set_current_code(func, pc);

        // The family's own body (the register-source entry the interpreter's `CallFun`
        // arm reaches), so the classification, the seam calls, the free-variable copy
        // and every error landing are the family's, unchanged.
        match crate::families::funs::call_fun_register(
            machine,
            module,
            arity,
            fun_register,
            func,
            pc,
            ctx,
        ) {
            Ok(Some(step)) => Ok(step),
            // Impossible for `CallFun`/`CallFun2` (the register entry always produces a
            // step), but a defensive fallback that re-runs the whole instruction keeps
            // this helper honest: it is the interpreter's exact step, with the same
            // operands.
            Ok(None) => {
                #[cfg(test)]
                super::counters::record_call_fun_fallback();
                return jit_step_one(rt, func, pc);
            }
            // The uniform raise funnel, exactly as `Machine::drive` runs it (see
            // [`super::family::jit_family`]): `func`/`pc` are the raising instruction's
            // address in the **active** module, and `entry_module` is what every
            // context-`0` frame is named against.
            Err(err) => Err(machine.with_uncaught_frames(err, Some(entry_module), func, pc)),
        }
    };
    match outcome {
        Ok(step) => fold_ok_step(runtime, step, func, pc),
        Err(err) => fold_err(runtime, err),
    }
}
