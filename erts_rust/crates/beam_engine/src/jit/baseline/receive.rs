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

//! The baseline tier's **`send` / receive-control** helper.
//!
//! [`jit_receive`] is emitted for the seven opcodes that drive the copy-bound
//! message-passing path — `send`, `loop_rec`, `loop_rec_end`, `wait`,
//! `wait_timeout`, `timeout` and `remove_message` (see `../emit/mod.rs`). Left to the
//! generic per-instruction deopt they each paid a whole interpreter step
//! ([`jit_step_one`] → [`Machine::step_instruction`](crate::Machine)) on every mailbox
//! operation, which is the dominant instruction class of a `send`/`receive` loop.
//!
//! The helper runs the instruction **without** the interpreter's generic dispatch: it
//! resolves the instruction at `(func, pc)` in the active module and hands it to the
//! receive family's own body ([`crate::families::dispatch`], the same entry the
//! interpreter's `other` arm reaches), so the semantics are the family's **by
//! construction** — `send`'s `x(0) = Message` and its `raise_bif_error` landing, a
//! `loop_rec` candidate's `write_operand`/fail-label branch, the mailbox seam calls,
//! and the suspend a `wait`/`wait_timeout` reports.
//!
//! Everything else is [`jit_step_one`]'s, exactly:
//!
//! * the position store ([`Machine::set_current_code`]) the interpreter's step makes
//!   at entry, so `Machine::current_code` keeps the instruction granularity a raise
//!   and a parked `receive` rely on;
//! * the context-`0` half of the uniform raise funnel
//!   ([`Machine::with_uncaught_frames`]) — in the run's entry context the active module
//!   *is* the entry module, so the frames are attached here, while a raise inside a
//!   cross-module callee is left for the driver, which knows the true entry module;
//! * the [`fold_step`] encoding of the resulting [`Step`](crate::Step) into the block
//!   ABI's word, so a `Step::Wait` reaches the driver as [`encode_wait`]'s word and a
//!   `Step::Next` as "the next instruction" — which the emitted continuation check
//!   turns back into an in-block continue.
//!
//! [`encode_wait`]: super::encode_wait

use super::jit_step_one;
use super::runtime::{fold_step, JitRuntime};

/// Run the `send`/receive-control instruction at `(func, pc)` natively and encode its
/// outcome as a block-ABI word.
///
/// The helper has the [`StepHelper`](super::runtime::StepHelper) signature, so the
/// emitter's `emit_receive` arm drives it through the shared
/// [`emit_helper_call`](super::emit::emit_helper_call) /
/// [`emit_continue_check`](super::emit::emit_continue_check) sequence: a `send`,
/// `loop_rec` (with a candidate), `timeout`, `remove_message` or a zero-time
/// `wait_timeout` returns "the next instruction" and the block **continues in-block**;
/// a `loop_rec` fail-label branch, a suspending `wait`/`wait_timeout` and every raise
/// leave the block with that word for the driver — the driver folds a suspend into
/// [`RunOutcome::Wait`](crate::RunOutcome::Wait) exactly as it folds a deopted one.
pub(crate) extern "C" fn jit_receive(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
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
        // SAFETY: `runtime.labels` points at the driver's `labels` local, which outlives
        // the loop.
        let labels = unsafe { &*runtime.labels };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as
        // a raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        // The same position store `Machine::step_instruction` makes at entry: a
        // `send`/`receive` is an exact suspension and raise position, so the machine's
        // saved instruction pointer must name this instruction, not the block.
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

        // The receive family's own body (the entry the interpreter's `other` arm
        // reaches), so the mailbox seam calls, the operand reads/writes and the
        // `send` error landing are the family's, unchanged.
        let step = crate::families::dispatch(machine, module, labels, instr, func, pc, ctx)
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