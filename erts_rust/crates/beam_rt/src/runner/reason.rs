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

//! The **exit-reason translation** and the **finite-`receive` deadline recovery**.
//!
//! When a slice ends, the runner has to name two things the engine does not: the *reason
//! term* a failing process exits with ([`exit_reason`], plus the `normal` atom a returning
//! process exits with via [`normal`]) and the deadline of a `receive … after T` a process
//! just parked at ([`parked_receive_timeout`]), which
//! [`RunOutcome::Wait`](beam_engine::RunOutcome::Wait) does not carry.

use std::time::Duration;

use beam_common::BeamError;
use beam_engine::Machine;
use beam_loader::{Instr, Module, Operand};
use beam_term::{is_small, signed_value, Eterm};

use crate::runtime::Runtime;

/// The `normal` exit-reason term (the reason a process that returns exits with).
pub(super) fn normal(runtime: &mut Runtime) -> Eterm {
    runtime.intern("normal")
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
    // A structured reason travels as a live term (the `beam_common::Raise` convention;
    // the payload is the reason as an `Eterm`, built on a BIF heap that M1 never
    // resets or moves). Return it directly — no reshaping, no loss.
    if let Some(term) = crate::error::live_reason_term(error) {
        return term;
    }
    runtime.intern(&error.to_string())
}

/// The finite timeout of the `receive` a process has just parked at `(func, pc)`.
///
/// The engine reports a suspension at the receive's **scan** address — the `loop_rec` label
/// the `wait`/`wait_timeout` jumps back to once a message arrives — and
/// [`EngineRunOutcome::Wait`](beam_engine::RunOutcome::Wait) carries no timeout, so the
/// *timed-out* continuation is recovered from the module, whose compiled shape is fixed:
///
/// ```text
/// ScanLabel:  loop_rec Fail, Dst
///             … match tests, loop_rec_end ScanLabel …
/// Fail:       wait_timeout ScanLabel, Time
///             timeout            ; <-- the timed-out continuation
/// ```
///
/// `Fail` is exactly the jump target of the `loop_rec` at `pc`, and — per the C
/// (`emu/msg_instrs.tab:356-363`: "In case of timeout, control will be transferred to the
/// `timeout` instruction following the `wait_timeout` instruction", armed as
/// `*pi = $NEXT_INSTRUCTION` in `i_wait.int`) — a timeout resumes at the instruction
/// **right after** the `wait_timeout`, i.e. the `timeout` opcode.
///
/// Returns `None` when the process parked on a bare `wait` (the `after infinity` the compiler
/// emits as `wait`), and for a timeout whose delay is not statically readable (see
/// [`timeout_delay`]) — the process then parks indefinitely, exactly as before, rather than
/// being given an invented deadline.
pub(super) fn parked_receive_timeout(
    machine: &Machine,
    module: &Module,
    func: u32,
    pc: u32,
) -> Option<(Duration, (u32, u32))> {
    let function = module.functions.get(func as usize)?;
    for (index, instr) in function.code.iter().enumerate() {
        let Instr::WaitTimeout { target, time } = instr else {
            continue;
        };
        // The `wait_timeout` that owns this receive is the one whose jump-back target is the
        // address the engine parked at (its `loop_rec`).
        if function.labels.get(target) != Some(&(pc as usize)) {
            continue;
        }
        let resume = (func, (index as u32) + 1);
        return timeout_delay(machine, module, time).map(|delay| (delay, resume));
    }
    None
}

/// The positive delay of a `wait_timeout`'s `Time` operand, when it is statically readable.
///
/// The operand is a **decoded IR operand**, not a runtime term: a literal timeout (`after
/// 20`) is [`Operand::Integer`] or (for a value the loader materialized) [`Operand::Literal`],
/// and an X- **or Y-register** timeout is read from the machine — no register and no Y slot of
/// the parked frame is touched by the suspension, so the value the `wait_timeout` tested is
/// still there. The Y case is read through the public [`Machine::y`] accessor, which is exactly
/// the slot the parked frame owns (the `wait_timeout` and the frame that carries its `Time` are
/// the same function), so a `receive … after T` with `T` a runtime variable recovers the same
/// deadline a literal one does.
///
/// `None` for everything else — an `after infinity` atom, a Y slot with no frame behind it (the
/// machine is not parked, or `k` is outside the frame's Y slots), and any non-positive or
/// non-integer value. `None` means "no deadline": the process parks until a message arrives,
/// which is the honest answer when the delay cannot be known rather than a guessed one.
pub(super) fn timeout_delay(machine: &Machine, module: &Module, time: &Operand) -> Option<Duration> {
    match time {
        Operand::Integer(millis) => positive_millis(*millis),
        Operand::Literal(index) => module.literal(*index).and_then(millis_of),
        Operand::X(register) => machine.x(*register).and_then(millis_of),
        Operand::Y(register) => machine.y(*register).and_then(millis_of),
        _ => None,
    }
}

/// `millis` as a [`Duration`], or `None` when it is not a positive delay.
fn positive_millis(millis: i64) -> Option<Duration> {
    (millis > 0).then(|| Duration::from_millis(millis as u64))
}

/// The positive millisecond delay a timeout **term** denotes, or `None`.
///
/// A small integer greater than zero is a delay; the atom `infinity`, a zero/negative
/// integer, and anything else are not (`infinity` means "never", which is the absence of a
/// deadline — the same `None`).
fn millis_of(term: Eterm) -> Option<Duration> {
    if is_small(term) {
        return positive_millis(signed_value(term));
    }
    None
}
