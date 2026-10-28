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
//! The baseline tier's **`put_list`** helper: the cons-construction arm's one Rust
//! call.
//!
//! Left to the generic family helper ([`super::family::jit_family`]) a `put_list` cost a
//! whole family-helper entry: the helper's own position store re-derived the packed
//! `(func, pc)` word from two run-time arguments on every execution, where the emitted
//! arm holds both as compile-time constants and can store the position inline in three
//! instructions ([`super::emit::emit_position_store`], the `jit_bitstr` precedent).
//! This helper is that arm's callee — see [`jit_put_list`].
//!
//! **Why a helper at all, and why it re-reads the instruction.** A cons cell must be
//! built on the process heap, and the only door to that heap is the rooted allocation
//! seam `terms::reserve` → `Machine::alloc_rooted` → `ExecutionContext::alloc_words_rooted`,
//! i.e. Rust. Nothing in the emitted sequence may allocate (a moving collection would
//! relocate terms a raw `*mut Eterm` still names), so the allocation stays here and the
//! block pays exactly one positioned helper call.
//!
//! **Re-execution idempotency.** The emitted arm's `fallback` is a deopt: it runs
//! *one* interpreter step, which re-executes the whole instruction. Nothing reachable
//! from this helper mutates machine state before the allocation succeeds:
//!
//! * the shape predicate only admits slack-free operands ([`super::emit::cons`]), so
//!   `families::lists::put_list`'s `extra` is `0` and the reservation is its first act;
//! * the two operand reads happen *inside* `make_cons_rooted`'s closure, after the
//!   reservation, so a collecting host's relocation is observed rather than read
//!   around;
//! * once the cons exists nothing can fail — the destination is an x-register or an
//!   already-guarded in-window `Y` slot — so there is no half-applied state for a
//!   re-execution to double-observe.
//!
//! [`jit_put_list`]: self::jit_put_list
#![allow(unsafe_code)]

use beam_loader::{Instr, Operand};

use crate::context::{ExecutionContext, RootSet};
use crate::machine::Machine;
use crate::operand::{operand_slack_words, read_operand_slack, write_operand};
use crate::terms;
use crate::Step;

use super::jit_step_one;
use super::runtime::{fold_err, fold_ok_step, JitRuntime};

/// Run the `put_list` at `(func, pc)` natively and encode its outcome as a block-ABI
/// word.
///
/// The helper has the [`StepHelper`](super::runtime::StepHelper) signature, so the
/// emitter's cons arm drives it through the same
/// [`emit_positioned_helper_call`](super::emit::emit_positioned_helper_call) /
/// [`emit_continue_check`](super::emit::emit_continue_check) sequence the bitstring arm
/// uses: a successful construction reports [`Step::Next`](crate::Step::Next) and the
/// block **continues in-block**, while a raise leaves the block with that word for the
/// driver.
///
/// It stores **no** position of its own: the emitted arm publishes this instruction's
/// own `(func, pc)` inline *before* the call (three instructions, both operands being
/// compile-time constants there), so the position still names the instruction rather
/// than the block and a raise parks exactly on it.
///
/// The instruction is re-derived from `(func, pc)` deliberately. A packed operand spec
/// has no lossless 64-bit channel for an immediate constant (a small int, an atom, `[]`
/// and a character all live in one word, and an out-of-band marker would have to
/// distinguish them from real words), and re-implementing the family's operand decode
/// here would duplicate `read_operand`/`write_operand` — the exact drift this tier
/// avoids everywhere else. The re-read costs two bounds-checked loads.
pub(crate) extern "C" fn jit_put_list(rt: *mut JitRuntime<'_>, func: u32, pc: u32) -> i64 {
    // SAFETY: the driver passes `rt` as the address of a live, exclusively borrowed
    // `JitRuntime` and never aliases it for the duration of the call; every pointer
    // it holds points at an object the driver owns and keeps alive for the whole
    // loop (`machine`, `module`, `entry_module`), or at a local (`labels`) that
    // outlives the loop.
    let runtime = unsafe { &mut *rt };
    // The family body's outcome, kept as the family's own `Result<Step, BeamError>` so
    // the success arm carries a plain [`Step`](crate::Step) and pays one ordinary
    // `Result` return — never a second box.
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

        // Only a `put_list` reaches this helper (the emitter's arm claims exactly that
        // variant). Anything else is the interpreter's, rather than guessed at here.
        let (head, tail, dst) = match instr {
            Instr::PutList { head, tail, dst } => (head, tail, dst),
            _ => return jit_step_one(rt, func, pc),
        };

        // The construction itself, the family's own body inlined: the rooted
        // reserve -> read -> write discipline, with the two operand reads *inside* the
        // reservation's closure so a collecting host's relocation is observed rather
        // than read around. The shape proof already refused every slack-carrying
        // operand, so the `extra` figure is 0 on every shape that reaches here.
        match construct(machine, ctx, head, tail, dst) {
            Ok(step) => Ok(step),
            // The uniform raise funnel, exactly as `Machine::drive` runs it: the blocks
            // this helper is called from never pass through `step_instruction`, so this
            // IS the one place a raise is decorated — with the run's true
            // `entry_module`, for a raise in the entry context and inside a
            // cross-module callee alike.
            Err(err) => Err(machine.with_uncaught_frames(err, Some(entry_module), func, pc)),
        }
    };
    match outcome {
        Ok(step) => fold_ok_step(runtime, step, func, pc),
        Err(err) => fold_err(runtime, err),
    }
}

/// The construction body, the owning family's own `put_list` with the dispatcher's
/// variant match removed.
///
/// The order is the frozen **reserve -> read -> write** discipline: the cons block is
/// reserved first (through [`terms::make_cons_rooted`], hence one rooted
/// `alloc_words_rooted` carrying the whole [`RootSet`]), the two operands are read
/// **inside** the reservation's closure (so a collecting host's relocation is observed
/// rather than read around), and only then is the destination written.
///
/// Every accepted shape carries no slack (`operand_slack_words` is 0 for a small int,
/// an atom, `[]`, and an x-register or in-window `Y` slot), so `extra` is 0 on every
/// path the emitter's `put_list_shape` admits; the sum is kept because this body must
/// stay identical to the family's if the shape proof later widens.
fn construct(
    machine: &mut Machine,
    ctx: &mut dyn ExecutionContext,
    head: &Operand,
    tail: &Operand,
    dst: &Operand,
) -> beam_common::Result<Step> {
    let extra = operand_slack_words(head) + operand_slack_words(tail);
    let cons = terms::make_cons_rooted(machine, ctx, RootSet::ALL_LIVE, extra, |m, s| {
        Ok((
            read_operand_slack(m, head, s)?,
            read_operand_slack(m, tail, s)?,
        ))
    })?;
    write_operand(machine, dst, cons)?;
    Ok(Step::Next)
}
