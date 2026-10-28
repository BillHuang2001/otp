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
//! The emitter arm for `put_list` — the cons-cell **constructor**, and the only
//! family-owned instruction besides `put_tuple2` whose body allocates.
//!
//! [`emit_put_list`] emits the shape `emit_list_read` ([`super::aggregate`]) established
//! for the cons **readers**, inverted: a guard phase that can only ever reject, then one
//! positioned helper call ([`super::family::emit_step_helper`] with
//! [`jit_put_list`](super::super::cons::jit_put_list)), then the shared continuation check,
//! and a local `fallback` that runs exactly one interpreter step.
//!
//! **Nothing before the helper mutates machine state.** The guard phase is
//! [`Loc::guard_store`] / [`Loc::guard_load`] only — a `Y` window bound check that branches
//! to `fallback`, never a write — and the inline position store inside
//! [`emit_positioned_helper_call`](super::emit_positioned_helper_call) writes only
//! `JitRuntime::current_code`. So a deopt from any guard re-executes the whole instruction
//! against an untouched machine, and `dirty.clear()` at the pre-call spill is exactly right.
//!
//! ## Why a helper, and why it re-derives the instruction
//!
//! The cons cell must come from the **rooted** allocation door
//! (`terms::make_cons_rooted` → `Machine::alloc_rooted` → `Context::alloc_words_rooted`),
//! which no emitted code can reach — the root set (`RootSet { x, stack, fr, live }`) is
//! Rust state, and a collecting host may relocate every term it names. The helper is
//! therefore one positioned call per cons, not an inlined store pair.
//!
//! The helper re-derives the instruction from `(func, pc)` deliberately: the two operand
//! slots are the only things the emitted code could hand over, and a packed block-ABI word
//! has no lossless channel for an **immediate constant** operand (`[H | []]`'s tail is
//! `NIL`, and a non-small `Integer` needs a heap word). Re-implementing the family's
//! operand decode here would duplicate `read_operand`/`write_operand` — the one authority
//! on what a slot means — so the arm hands over the instruction's address instead.
//!
//! ## The shape (see [`put_list_shape`] for every refusal)
//!
//! Accepted only when `head`, `tail` and `dst` are each an in-bound `X` register, an
//! addressable `Y` slot, or an immediate constant, and no operand needs *slack* — a
//! non-small `Operand::Integer` would make the family's `make_cons_rooted` reserve
//! `2 + extra` words, i.e. hand an `extra > 0` to the seam, and the emitted arm proves
//! nothing about that. Such a shape declines at compile time and emits **zero bytes**, so
//! `emit_block` falls through to the family arm (`emit_family`), never to a deopt: `put_list`
//! has no deopt route of its own, and the family body is the same code the interpreter's
//! step runs.

#[cfg(test)]
use std::cell::Cell;

use beam_common::Result;
use beam_loader::Operand;

use crate::jit::emitter::{CodeBuffer, Label as CodeLabel};

use super::super::cons::jit_put_list;
use super::family::emit_step_helper;
use super::operands::{constant_word, Loc, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2};
use super::{emit_step_and_continue, Allocation};

/// The operand slots a native `put_list` arm was accepted for.
///
/// Each field is `Some(loc)` for a slot the emitted code must frame-guard (an `X`
/// register guards trivially, a `Y` slot emits the window check) and `None` for an
/// **immediate constant**, which no guard can reject (there is no address to prove).
/// The helper re-reads every operand from the machine, so the shape carries no value.
#[derive(Clone, Copy, Debug)]
pub(super) struct PutListShape {
    /// The cell's CAR operand.
    head: Option<Loc>,
    /// The cell's CDR operand.
    tail: Option<Loc>,
    /// The destination the built cell is written to.
    dst: Option<Loc>,
}

impl PutListShape {
    /// Whether any slot is a `Y` slot — the block must therefore publish the frame window
    /// in its prologue before this arm runs.
    pub(super) fn uses_y(self) -> bool {
        [self.head, self.tail, self.dst]
            .into_iter()
            .flatten()
            .any(Loc::is_y)
    }

    /// The x-registers this arm caches in machine registers: **none**.
    ///
    /// The helper reads every operand out of `machine.x`/`machine.y` and writes `dst`
    /// there too, so the arm never touches a cache register (see
    /// [`super::regalloc::cacheable_touches`]).
    pub(super) const fn registers(self) -> [u32; 0] {
        []
    }
}

/// The **purely syntactic** shape test for a native `put_list` arm — no label map, no
/// module, no register allocation, so [`super::regalloc::allocate_registers`] and
/// [`emit_put_list`] derive the same answer.
///
/// Accepted: each of `head`/`tail`/`dst` is an in-bound `X` register
/// ([`Loc::of`]), an addressable `Y` slot ([`Loc::of`]), or an immediate constant
/// ([`constant_word`]: a small `Integer`, a `Character`, an `Atom`, or `Nil`).
///
/// Refused (the arm emits zero bytes and `emit_block` keeps the family arm):
///
/// * `Operand::FloatReg` — a float register, which no cons operand can name;
/// * `Operand::Literal` — a literal-area word the helper would have to resolve through the
///   module's area; the family does that, and the arm proves nothing about it;
/// * an out-of-bound `X` register, or a `Y` slot the emitted `disp32` cannot address
///   (`Loc::of` answers `None` for both);
/// * a **non-small** `Operand::Integer`/`Character` — `constant_word` is `None` and the
///   operand is not a slot either. This is the refusal that keeps the helper's
///   `extra == 0` claim true: the family reserves `2 + operand_slack_words(head) +
///   operand_slack_words(tail)` words, and `operand_slack_words` is non-zero exactly when a
///   big-integer immediate needs slack.
/// * `Operand::Label` / `Operand::List` / `Operand::Allocation`, which are never a value.
pub(super) fn put_list_shape(head: &Operand, tail: &Operand, dst: &Operand) -> Option<PutListShape> {
    Some(PutListShape {
        head: slot(head)?,
        tail: slot(tail)?,
        dst: slot(dst)?,
    })
}

/// One operand as a guardable slot, or `None` for an immediate constant.
///
/// The `Option<Option<Loc>>` flattening is the point: `None` (refused) and
/// `Some(None)` (an immediate, nothing to guard) are different answers, and the caller
/// must not confuse them.
fn slot(operand: &Operand) -> Option<Option<Loc>> {
    match Loc::of(operand) {
        Some(loc) => Some(Some(loc)),
        // No address to guard, but only an immediate constant is acceptable here: any other
        // operand kind would need the family's own read.
        None if constant_word(operand).is_some() => Some(None),
        None => None,
    }
}

/// Emit the native `put_list` arm, or `Ok(false)` when [`put_list_shape`] declines.
///
/// The emitted sequence is `emit_list_read`'s, inverted:
///
/// 1. **guard phase** — `dst`'s store guard, then `head`'s and `tail`'s load guards, each
///    branching to `fallback`. All three precede any mutation, and the one
///    address/end scratch pair is reused because a guard must merely *reject*;
/// 2. the positioned helper call
///    ([`emit_step_helper`] with `position = true`), which publishes this instruction's
///    own `(func, pc)` inline before the call so a pre-emption or suspension parks here;
/// 3. `jmp fast_done` — the helper `Ok(_)` arm continues in-block via the shared
///    continuation check, anything else leaves the block with the word in `rax`;
/// 4. `fallback`: exactly one interpreter step, then the continuation check.
///
/// `Ok(false)` means "not this arm": the caller must emit the **family** arm, not a deopt.
pub(super) fn emit_put_list(
    buffer: &mut CodeBuffer,
    allocation: &Allocation,
    done: CodeLabel,
    func: u32,
    pc: u32,
    head: &Operand,
    tail: &Operand,
    dst: &Operand,
) -> Result<bool> {
    let Some(shape) = put_list_shape(head, tail, dst) else {
        record_declined();
        return Ok(false);
    };
    record_lowered();

    let fallback: CodeLabel = buffer.label();
    let fast_done: CodeLabel = buffer.label();

    // Guard phase. Every guard may only reject, and none of them consumes the address it
    // computes: the helper re-derives the slot from `machine.y`, so the `Y` address in
    // `Y_ADDR_SCRATCH` is dead on the accepted path.
    if let Some(loc) = shape.dst {
        loc.guard_store(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;
    }
    if let Some(loc) = shape.head {
        loc.guard_load(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;
    }
    if let Some(loc) = shape.tail {
        loc.guard_load(buffer, Y_ADDR_SCRATCH, Y_ADDR_SCRATCH_2, fallback)?;
    }

    emit_step_helper(buffer, allocation, done, func, pc, jit_put_list, true)?;
    buffer.jmp(fast_done)?;
    buffer.bind(fallback)?;
    emit_step_and_continue(buffer, done, allocation, func, pc)?;
    buffer.bind(fast_done)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// The test-build emission instrument
// ---------------------------------------------------------------------------
//
// Tier agreement cannot prove an inline arm native: a deopted instruction is executed by
// the interpreter on **both** sides, so an arm that silently stopped lowering would still
// agree. The emission decision is the only observable, so it is counted here — the same
// contract as `emit/bitstr_inline/mod.rs`'s per-shape `inlined` counter.

/// How often a `put_list` was accepted for the native arm, on this thread.
///
/// A **compile-time** count: it moves once per emitted arm, never per execution. Pair it
/// with a zero static-deopt count for `PutList` over the same compile-free emission to
/// prove the whole instruction stayed native.
#[cfg(test)]
pub(crate) fn lowered() -> u64 {
    PUT_LIST_ARM.with(Cell::get).0
}

/// How often a `put_list` was **refused** by [`put_list_shape`], on this thread.
///
/// A refusal emits zero bytes, so the counter is the proof that a refused shape reached the
/// family arm rather than this one — the counterpart of [`lowered`].
#[cfg(test)]
pub(crate) fn declined() -> u64 {
    PUT_LIST_ARM.with(Cell::get).1
}

/// Reset both counters (a test that compiles more than one module asserts per-compile
/// deltas rather than a running total).
#[cfg(test)]
pub(crate) fn reset_counters() {
    PUT_LIST_ARM.with(|counts| counts.set((0, 0)));
}

#[cfg(test)]
thread_local! {
    /// `(lowered, declined)` for the `put_list` arm — see [`lowered`]/[`declined`].
    static PUT_LIST_ARM: Cell<(u64, u64)> = const { Cell::new((0, 0)) };
}

#[cfg(test)]
fn record_lowered() {
    PUT_LIST_ARM.with(|counts| {
        let (low, dec) = counts.get();
        counts.set((low + 1, dec));
    });
}

#[cfg(test)]
fn record_declined() {
    PUT_LIST_ARM.with(|counts| {
        let (low, dec) = counts.get();
        counts.set((low, dec + 1));
    });
}

/// Production builds carry no instrument: the two record hooks are no-ops, so the
/// emitter's decision costs nothing at run time and the emitted bytes do not change.
#[cfg(not(test))]
#[inline(always)]
const fn record_lowered() {}

/// The production no-op twin of the refusal counter (see [`record_lowered`]).
#[cfg(not(test))]
#[inline(always)]
const fn record_declined() {}
