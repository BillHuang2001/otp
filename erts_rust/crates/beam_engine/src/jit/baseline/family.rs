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
//! The baseline tier's **instruction-family** helper: the in-block lowering for the
//! opcodes the interpreter's dispatch loop does not arm itself but hands to
//! [`crate::families::dispatch`] (its `other` arm).
//!
//! Left to the generic per-instruction deopt ([`super::jit_step_one`] →
//! [`Machine::step_instruction`](crate::Machine)) each such instruction paid a whole
//! interpreter step: the deopt re-matches the instruction variant in the interpreter's
//! driver, resolves it through the shared runtime funnel and only then reaches the same
//! family body. A compiled block can reach that family body directly, which is what this
//! helper does — see [`jit_family`].
//!
//! [`jit_family`]: self::jit_family
#![allow(unsafe_code)]

use super::jit_step_one;
use super::runtime::{fold_step, JitRuntime};

/// Run the family-owned instruction at `(func, pc)` natively and encode its outcome as a
/// block-ABI word.
///
/// The helper has the [`StepHelper`](super::runtime::StepHelper) signature, so the
/// emitter's family arm drives it through the shared
/// [`emit_helper_call`](super::emit::emit_helper_call) /
/// [`emit_continue_check`](super::emit::emit_continue_check) sequence: a family body
/// that reports [`Step::Next`](crate::Step::Next) (`put_list`, `put_tuple2`, a `send`,
/// a `loop_rec` with a candidate, `timeout`, `remove_message` or a zero-time
/// `wait_timeout`) returns "the next instruction" and the block **continues in-block**;
/// a family `Goto` (a `loop_rec` fail-label branch), a suspending `wait`/`wait_timeout`
/// and every raise leave the block with that word for the driver — the driver folds a
/// suspend into [`RunOutcome::Wait`](crate::RunOutcome::Wait) exactly as it folds a
/// deopted one.
///
/// The single implementation is shared by every family-owned shape the emitter claims
/// ([`super::receive::jit_receive`] is a thin wrapper over it, the same way the emitter's
/// `emit_receive` and `emit_family` share one emission sequence): the body is
/// instruction-agnostic, because everything specific to an opcode lives in the family
/// the dispatcher routes it to.
pub(crate) extern "C" fn jit_family(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    let step = {
        // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
        // `JitRuntime` and never aliases it for the duration of the call; every pointer
        // it holds points at an object the driver owns and keeps alive for the whole
        // loop (`machine`, `module`), or at a local (`labels`) that outlives the loop.
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

        // The same position store `Machine::step_instruction` makes at entry: a
        // `send`/`receive` is an exact suspension and raise position, and a term
        // constructor a move/raise position, so the machine's saved instruction pointer
        // must name this instruction, not the block.
        machine.set_current_code(func, pc);

        // The instruction this block emitted the helper for. An address outside the
        // module cannot be here (the block was compiled from this very code), so it is
        // handed to the interpreter's own step rather than invented — it reports the
        // interpreter's exact diagnostic for a bad `(func, pc)`.
        let instr = match module
            .functions
            .get(func as usize)
            .and_then(|function| function.code.get(pc as usize))
        {
            Some(instr) => instr,
            None => return jit_step_one(rt, func, pc),
        };

        // The owning family's own body (the entry the interpreter's `other` arm
        // reaches), so the mailbox seam calls, the operand reads/writes, the rooted
        // term construction and every error landing are the family's, unchanged.
        let step =
            crate::families::dispatch(machine, entry_module, module, labels, instr, func, pc, ctx)
                .map_err(Box::new);
        // The uniform raise funnel's context-`0` half, exactly as
        // `Machine::step_instruction` runs it: in context `0` the active module IS the
        // entry module, so a propagating raise is decorated here; a non-zero context is
        // left to the driver, which knows the true entry module.
        if machine.current_context() == 0 {
            step.map_err(|err| Box::new(machine.with_uncaught_frames(*err, Some(module), func, pc)))
        } else {
            step
        }
    };
    // SAFETY: `rt` is the same live, exclusively borrowed `JitRuntime` the driver owns;
    // the `&mut` derived from it above is dead by now, so this borrow does not alias it.
    let runtime = unsafe { &mut *rt };
    fold_step(runtime, step, func, pc)
}
