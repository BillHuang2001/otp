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
//! The baseline tier's **bitstring-family helper**: the in-block lowering for every `Bs*`
//! opcode — the single most frequent family in a binary/`bstring` decoder, where a match
//! and its field reads execute once per byte of the subject.
//!
//! Every family-owned instruction the emitter claims reaches its family through
//! [`crate::families::dispatch`], the router that re-matches the instruction's variant (a
//! jump table on the [`Instr`](beam_loader::Instr) discriminant) to find the **owning**
//! family. For a `Bs*` instruction that hop is pure overhead: this tier already knows the
//! variant (it emitted the arm), so [`jit_bitstr`] calls the bitstring family's own entry
//! ([`crate::families::bitstrings::exec`]) directly. The emitted arm claims with the
//! family's own [`claimed`](crate::families::bitstrings::claimed) predicate — the same
//! function the router routes on — so the bypass covers `bs_start_match*`,
//! `bs_get_position`, `bs_create_bin`, `bs_init_writable`, the
//! `bs_get_integer*`/`bs_get_binary*` reads, `bs_match_string`, the UTF reads and every
//! other member — not one hand-picked shape, and never a hand-maintained copy of the set.
//!
//! Everything else is deliberately identical to [`super::family::jit_family`]: the same
//! position store, the same instruction fetch (declining to [`jit_step_one`] on an address
//! that does not resolve), the same context-`0` raise decoration, and the same
//! [`fold_ok_step`]/[`fold_err`] encoding of the outcome. The helper therefore cannot
//! produce a result the interpreter's per-instruction step would not — only reach it one
//! dispatch hop sooner.
//!
//! **The helper runs no per-execution shape predicate.** The emit side decided the route at
//! compile time (the `emit_block` arm); re-deciding it here would hand back a large part of
//! the hop this helper exists to remove (the previous round measured a per-execution re-test
//! at about a third of the win). An address that holds a variant the family does not own — an
//! artifact/module mismatch, say — is answered by the family's own `Ok(None)` and handed to
//! [`jit_step_one`], which is the interpreter's exact diagnostic for it.

use super::jit_step_one;
use super::runtime::{fold_err, fold_ok_step, JitRuntime};
use crate::families::bitstrings;

/// How many times [`jit_bitstr`] has actually executed on this thread.
///
/// Test-only structural instrument (`#[cfg(test)]`-gated, so a production build carries no
/// counter at all): a cross-tier result agreement alone would also hold if the emitted arm had
/// kept the generic family route, so the pins assert this moved. Per-thread and monotonic, so a
/// caller takes a `before`/`after` pair around the run it is describing.
#[cfg(test)]
pub(crate) fn bitstr_calls() -> u64 {
    BITSTR_CALLS.with(std::cell::Cell::get)
}

// The `bitstr_calls` counter's per-thread storage (test builds only).
#[cfg(test)]
thread_local! {
    static BITSTR_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Run the bitstring-family instruction at `(func, pc)` through the family's own body and
/// encode its outcome as a block-ABI word.
///
/// The [`StepHelper`](super::runtime::StepHelper) signature, so the emitter drives it through
/// the very emission sequence the generic family arm uses
/// ([`emit_helper_call`](super::emit::emit_helper_call) +
/// [`emit_continue_check`](super::emit::emit_continue_check)): a completed instruction reports
/// [`Step::Next`](crate::Step::Next) and the block continues in-block, a fail-label branch
/// reports a `Goto` and a raise/`Wait` leaves the block with that word for the driver —
/// exactly what [`jit_family`](super::family::jit_family) would have produced.
pub(crate) extern "C" fn jit_bitstr(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer it holds
    // points at an object the driver owns and keeps alive for the whole loop (`machine`,
    // `module`, `entry_module`), or at a local (`labels`) that outlives the loop.
    let runtime = unsafe { &mut *rt };
    let outcome = {
        // SAFETY: `runtime.machine` is a non-null pointer to the driver's live `Machine`; no
        // other `&mut Machine` exists while the helper runs (the driver is blocked in the
        // block call that reached us).
        let machine = unsafe { &mut *runtime.machine };
        // SAFETY: `runtime.module` points at the module the driver is running, which outlives
        // the loop.
        let module = unsafe { &*runtime.module };
        // SAFETY: `runtime.entry_module` points at the run's entry module, which the driver
        // holds alive for the whole loop.
        let entry_module = unsafe { &*runtime.entry_module };
        // SAFETY: `runtime.labels` points at the driver's `labels` local, which outlives the
        // loop.
        let labels = unsafe { &*runtime.labels };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as a
        // raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        // The same position store `Machine::step_instruction` makes at entry: a `bs_match`
        // raises `{badmatch, Subject}` at NO_FAIL and branches at a fail label, and
        // `bs_create_bin`/`bs_init_writable` are allocation (move/raise) positions, so the
        // machine's saved instruction pointer must name this instruction, not the block.
        machine.set_current_code(func, pc);

        // The instruction this block emitted the helper for. An address outside the module
        // cannot be here (the block was compiled from this very code), so it is handed to the
        // interpreter's own step rather than invented.
        let instr = match module
            .functions
            .get(func as usize)
            .and_then(|function| function.code.get(pc as usize))
        {
            Some(instr) => instr,
            None => return jit_step_one(rt, func, pc),
        };

        // The bitstring family's own entry (the one `crate::families::dispatch` reaches for
        // every `Bs*` variant), so the exception-class anchor, the operand reads/writes, the
        // match-context state, the rooted term construction and every error landing are the
        // family's, unchanged.
        #[cfg(test)]
        BITSTR_CALLS.with(|calls| calls.set(calls.get() + 1));
        match bitstrings::exec(machine, module, labels, instr, func, pc, ctx) {
            Ok(Some(step)) => Ok(step),
            // Not this family's variant after all: the interpreter's own step, which reports
            // the exact diagnostic (or executes the instruction natively) for it.
            Ok(None) => return jit_step_one(rt, func, pc),
            // The uniform raise funnel, exactly as `Machine::drive` runs it: a block this
            // helper is called from never passes through `step_instruction`, so this IS the
            // one place the raise is decorated — in the entry context and inside a
            // cross-module callee alike, with `func`/`pc` naming the raising instruction in
            // the active module and `entry_module` the module every context-`0` frame
            // belongs to.
            Err(err) => Err(machine.with_uncaught_frames(err, Some(entry_module), func, pc)),
        }
    };
    match outcome {
        Ok(step) => fold_ok_step(runtime, step, func, pc),
        Err(err) => fold_err(runtime, err),
    }
}
