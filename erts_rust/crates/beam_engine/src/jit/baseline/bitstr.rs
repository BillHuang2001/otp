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
//! The baseline tier's **specialised `bs_match` helper**: the in-block lowering for the one
//! `bs_match` shape a binary comprehension / fixed-field decoder executes once per element.
//!
//! Every family-owned instruction the emitter claims reaches its family through
//! [`crate::families::dispatch`], the router that re-matches the instruction's variant (a
//! jump table on the [`Instr`] discriminant) to find the **owning** family. For a `BsMatch`
//! that router hop is pure overhead: this tier already knows the variant (it emitted the
//! arm), so [`jit_bs_match`] calls the bitstring family's own body
//! ([`crate::families::bitstrings::exec`]) directly — and only for the shape
//! [`supported`] names, so every other `bs_match` keeps the generic family helper's route.
//!
//! Everything else is deliberately identical to [`super::family::jit_family`]: the same
//! position store, the same instruction fetch (declining to [`jit_step_one`] on an address
//! that does not resolve), the same `class_atoms_live` latch the router would have run, the
//! same context-`0` raise decoration, and the same [`fold_ok_step`]/[`fold_err`] encoding of
//! the outcome. The helper therefore cannot produce a result the interpreter's
//! per-instruction step would not — only reach it one dispatch hop sooner.

use beam_loader::{Instr, Operand};

use super::jit_step_one;
use super::runtime::{fold_err, fold_ok_step, JitRuntime};
use crate::families::bitstrings;

/// The shape [`jit_bs_match`] lowers: the fixed-width field read a binary comprehension (and
/// a `<<Len, Rest/bits>>` style decoder) runs once per element.
///
/// A `bs_match`'s `commands` operand is the raw flattened command list, so the decoded shape
/// is `[ensure_at_least, Bits, Unit, integer, Live, Flags, Size, Unit, Dst]` — nine operands:
/// a compile-time constant `ensure_at_least` skip, then an `integer` read of a compile-time
/// constant size/unit into a register. That is exactly the pair the OTP compiler emits for
/// `<< C >> <= B` (`bits/i`'s own `{ensure_at_least,8,1}` + `{integer,_,{literal,[]},8,1,Dst}`),
/// and the only nine-operand command list the vocabulary has (the other commands are 2
/// (`ensure_exactly`, `skip`), 4 (`get_tail`, `=:=`) or 6 (`integer`, `binary`) operands long).
///
/// The predicate is **purely syntactic** — the instruction's own operands, never the module
/// (so it cannot resolve the two command atoms): it only decides *which* of two routes reaches
/// the **same** family body. A shape it wrongly accepts would still execute correctly through
/// [`crate::families::bitstrings::exec`]; it could only fail to be the fast path.
pub(crate) fn supported(instr: &Instr) -> bool {
    let Instr::BsMatch { commands, .. } = instr else {
        return false;
    };
    // `ensure_at_least Bits Unit` followed by `integer Live Flags Size Unit Dst`. The two
    // command atoms are checked only as "an atom" (the predicate has no module to name them
    // through); the constant fields and the register destination are the shape's real
    // signature, and the operand count is what makes the pair unambiguous.
    match commands.as_slice() {
        [
            Operand::Atom(_),
            Operand::Integer(_),
            Operand::Integer(_),
            Operand::Atom(_),
            Operand::Integer(_),
            _flags,
            Operand::Integer(_),
            Operand::Integer(_),
            dst,
        ] => is_register(dst),
        _ => false,
    }
}

/// `true` for the destination forms a `bs_match` `integer` command can bind: an X register,
/// a Y slot, or the `tr` typed wrapper around either.
fn is_register(operand: &Operand) -> bool {
    matches!(
        operand,
        Operand::X(_) | Operand::Y(_) | Operand::Typed { .. }
    )
}

/// How many times [`jit_bs_match`] has actually executed on this thread.
///
/// Test-only structural instrument (`#[cfg(test)]`-gated, so a production build carries no
/// counter at all): a cross-tier result agreement alone would also hold if the emitted arm had
/// kept the generic family route, so the pins assert this moved. Per-thread and monotonic, so a
/// caller takes a `before`/`after` pair around the run it is describing.
#[cfg(test)]
pub(crate) fn specialised_calls() -> u64 {
    SPECIALISED_CALLS.with(std::cell::Cell::get)
}

// The `specialised_calls` counter's per-thread storage (test builds only).
#[cfg(test)]
thread_local! {
    static SPECIALISED_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
/// Run the `bs_match` at `(func, pc)` through the bitstring family's own body and encode its
/// outcome as a block-ABI word.
///
/// The [`StepHelper`](super::runtime::StepHelper) signature, so the emitter drives it through
/// the very emission sequence the generic family arm uses
/// ([`emit_helper_call`](super::emit::emit_helper_call) +
/// [`emit_continue_check`](super::emit::emit_continue_check)): a completed match reports
/// [`Step::Next`](crate::Step::Next) and the block continues in-block, a fail-label branch
/// reports a `Goto` and a raise/`Wait` leaves the block with that word for the driver —
/// exactly what [`jit_family`](super::family::jit_family) would have produced.
pub(crate) extern "C" fn jit_bs_match(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
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
        // SAFETY: `runtime.labels` points at the driver's `labels` local, which outlives the
        // loop.
        let labels = unsafe { &*runtime.labels };
        // SAFETY: `runtime.ctx` is the driver's `&mut dyn ExecutionContext` reborrowed as a
        // raw pointer; the driver holds no `&mut` to it while the helper runs.
        let ctx = unsafe { &mut *runtime.ctx };

        // The same position store `Machine::step_instruction` makes at entry: a `bs_match`
        // raises `{badmatch, Subject}` at NO_FAIL and branches at a fail label, so the
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
        // The emitted arm decided the **shape** at compile time; the helper's own guard is
        // only that this address really holds a `BsMatch` (a mismatch means the artifact is
        // not the one the driver published). It deliberately does NOT re-run [`supported`]:
        // the family body executes *every* `BsMatch` shape, so the shape predicate only ever
        // chose between two routes to the same body, and re-deciding it per execution would
        // hand back a large part of the hop this helper exists to remove.
        if !matches!(instr, Instr::BsMatch { .. }) {
            return jit_step_one(rt, func, pc);
        }

        // The router's own `class_atoms_live` latch, reproduced because this helper bypasses
        // the router: without it a family raise whose class atom the *module's* table cannot
        // name would select its catch frame and then abandon it with a typed `unsupported`.
        // One `bool` load and a not-taken branch in the steady state.
        if !machine.class_atoms_live {
            machine.ensure_class_atoms(ctx, module);
        }

        // The bitstring family's own body (the entry [`crate::families::dispatch`] reaches
        // for every `Bs*` variant), so the operand reads/writes, the match-context state, the
        // rooted term construction and every error landing are the family's, unchanged.
        #[cfg(test)]
        SPECIALISED_CALLS.with(|calls| calls.set(calls.get() + 1));
        match bitstrings::exec(machine, module, labels, instr, func, pc, ctx) {
            Ok(Some(step)) => Ok(step),
            // Not this family's variant after all: the interpreter's own step, which reports
            // the documented `unsupported` for an unclaimed variant.
            Ok(None) => return jit_step_one(rt, func, pc),
            // The uniform raise funnel's context-`0` half, exactly as
            // `Machine::step_instruction` runs it: in context `0` the active module IS the
            // entry module, so a propagating raise is decorated here; a non-zero context is
            // left to the driver, which knows the true entry module.
            Err(err) if machine.current_context() == 0 => {
                Err(machine.with_uncaught_frames(err, Some(module), func, pc))
            }
            Err(err) => Err(err),
        }
    };
    match outcome {
        Ok(step) => fold_ok_step(runtime, step, func, pc),
        Err(err) => fold_err(runtime, err),
    }
}
