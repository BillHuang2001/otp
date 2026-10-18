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
//! The **receive / message-queue** group.
//!
//! Owns the 10 receive variants:
//!
//! `Send, LoopRec, LoopRecEnd, Wait, Timeout, RemoveMessage, RecvMarkerReserve,
//! RecvMarkerBind, RecvMarkerClear, RecvMarkerUse`.
//!
//! Mechanic: install [`ProbeMailbox`](super::super::ProbeMailbox) (pre-seeded
//! through `push`, or via `resume_message` for the wakeup path) so the seam has
//! something to scan; a row that legitimately blocks declares
//! `Expect::Suspends(..)` and the harness resumes it at the reported
//! `(func, pc)` — never a typed error.
//!
//! The engine keeps **no** mailbox state of its own: every one of these opcodes is
//! a call into the [`Mailbox`] seam, whose `ExecutionContext` twins default to the
//! typed `unsupported_opcode: <method>` answer. So an installed mailbox is the whole
//! fix for a row that used to be counted as "claimed but blocked" — what is left is
//! the *valid operand*: a destination register, a fail/target label the module
//! defines, and a **host-produced** marker.
//!
//! Three consequences shape the rows below:
//!
//! * **Message selection is the compiled code's job** (the match instructions between
//!   `loop_rec` and `loop_rec_end`), never the engine's, so a row can pin the
//!   candidate a scan position yields without any matching machinery. Messages are
//!   small integers: an atom would have to be interned into the run module's table,
//!   and a compound term would have to be built in the run's own context.
//! * `receive_skip`/`receive_remove`/`receive_reset` act on the host's **own**
//!   persistent scan position, so a row that needs a non-head scan **pre-seeds** it
//!   by calling the [`Mailbox`] method on its own mailbox handle before the probe is
//!   built (`ProbeMailbox`'s state is shared with the installed copy).
//! * A marker is **opaque**: it must come from `receive_marker_reserve`, so the
//!   bind/use/clear rows reserve it in their `setup` prologue instead of fabricating
//!   one. `ProbeMailbox::receive_marker_reserve` answers a **fresh small-int token**
//!   recording the current scan position (and the `undefined` atom on an **empty**
//!   queue, which is why those rows queue a message first).
//!
//! Ground truth (read-only): `emu/msg_instrs.tab` (`i_loop_rec`, `loop_rec_end`,
//! `remove_message`, `timeout`, `wait.*`, `recv_marker_*`) and
//! `emu/bif_instrs.tab:425-455` (`send`) — the family that owns these variants is
//! `src/families/receive.rs`.

use beam_engine::{Mailbox, MinimalContext};
use beam_loader::{Instr, Operand};
use beam_term::{make_small, Eterm};

use super::super::{
    aux_label, Expect, Probe, ProbeAtoms, ProbeMailbox, FAIL_SINK_LABEL, SUCCESS_LABEL,
};

/// The register argument every non-`send` row seeds: a `send`/`loop_rec` destination
/// stands in for a pid a real host would resolve, and a scan-only opcode has to leave
/// *some* distinguishable term in `x(0)` for the success sink to return.
const PLACEHOLDER: i64 = 777;

/// The message the `send` row delivers (and therefore also the value it returns,
/// since `Pid ! Msg` evaluates to `Msg`).
const MESSAGE: i64 = 42;

/// The first queued message of a two-message queue.
const FIRST: i64 = 1;

/// The second queued message of a two-message queue.
const SECOND: i64 = 2;

/// The value the `wait` row's aux block writes into `x(0)` after the resume.
const WAKE_VALUE: i64 = 77;

/// The reference the marker rows bind (a small integer, so it needs no interning and
/// no heap).
const REFERENCE: i64 = 1001;

/// Every converted row of the receive group.
pub fn rows(atoms: &ProbeAtoms) -> Vec<Probe> {
    vec![
        send_row(atoms),
        loop_rec_row(atoms),
        loop_rec_end_row(atoms),
        wait_row(atoms),
        timeout_row(atoms),
        remove_message_row(atoms),
        recv_marker_reserve_row(atoms),
        recv_marker_bind_row(atoms),
        recv_marker_use_row(atoms),
        recv_marker_clear_row(atoms),
    ]
}

// ---------------------------------------------------------------------------
// Argument builders
// ---------------------------------------------------------------------------

/// `x[0..2)` = the destination (`x(0)`) and the message (`x(1)`) of a `send`.
fn send_args(_ctx: &mut MinimalContext) -> Vec<Eterm> {
    vec![make_small(FIRST), make_small(MESSAGE)]
}

/// `x[0..1)` = the [`PLACEHOLDER`] a scan-only opcode leaves untouched.
fn placeholder_arg(_ctx: &mut MinimalContext) -> Vec<Eterm> {
    vec![make_small(PLACEHOLDER)]
}

// ---------------------------------------------------------------------------
// The rows
// ---------------------------------------------------------------------------

/// `send` — deliver `x(1)` to `x(0)`.
///
/// C stores `erl_send`'s result, which is the **message itself**, in `x(0)`
/// (`emu/bif_instrs.tab:436-441`), which is why `Pid ! Msg` evaluates to `Msg` — so
/// the row pins `42`, not the destination. `x(0) = 1` is only a placeholder: a real
/// host resolves the destination to a process, while [`ProbeMailbox`] delivers into
/// its own queue.
fn send_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(Instr::Send, Expect::Returned(Some(make_small(MESSAGE))))
        .args(send_args)
        .mailbox(ProbeMailbox::new(atoms.undefined()))
        .note("send stores the message x(1)=42 in x(0); x(0)=1 is a placeholder destination a real host resolves to a pid")
}

/// `loop_rec Fail Dst` — the candidate at the scan position, or the `fail` branch.
///
/// One queued message at scan 0 is the **candidate** case: the fetch writes it into
/// `Dst` (`x(0)`) and falls through, so the success sink returns the message. The
/// **exhausted** case — `receive_next` answers `None` and the instruction jumps to
/// its `fail` label — is the documented `fail` branch of the same row (the other
/// nine family tests drive it); a probe row drives exactly one of the two sinks.
fn loop_rec_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::LoopRec {
            fail: FAIL_SINK_LABEL,
            dst: Operand::X(0),
        },
        Expect::Returned(Some(make_small(MESSAGE))),
    )
    .args(placeholder_arg)
    .mailbox(ProbeMailbox::with_messages(
        atoms.undefined(),
        vec![make_small(MESSAGE)],
    ))
    .note("the candidate at scan 0 is fetched into x(0)=42; an exhausted scan would take this row's fail branch instead")
}

/// `loop_rec_end Target` — the candidate did not match: advance the scan and jump.
///
/// The target is the success sink, so the row proves both halves: the scan really
/// advanced (`receive_skip`) and the jump landed — `x(0)` is still the untouched
/// [`PLACEHOLDER`], which no message of the queue (1, 2) could have produced.
fn loop_rec_end_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::LoopRecEnd {
            target: SUCCESS_LABEL,
        },
        Expect::Returned(Some(make_small(PLACEHOLDER))),
    )
    .args(placeholder_arg)
    .mailbox(ProbeMailbox::with_messages(
        atoms.undefined(),
        vec![make_small(FIRST), make_small(SECOND)],
    ))
    .note("skips the candidate at scan 0 and jumps back to its target (the success sink), which returns the untouched x(0)=777")
}

/// `wait Target` — suspend, resuming at `Target`.
///
/// An **empty** mailbox is what makes the suspend the *legitimate* outcome rather
/// than a probe artifact: the compiled receive has already exhausted its scan and
/// parks on the wakeup. The row declares `Expect::Suspends`, so the harness resumes
/// the machine at the reported `(func, pc)` — the target label, whose aux block
/// writes [`WAKE_VALUE`] into `x(0)` and returns it. (The engine has no host timer at
/// Milestone 1, so a real wakeup is a delivered message; a resume needs neither.)
fn wait_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::Wait {
            target: aux_label(0),
        },
        Expect::Suspends(Some(make_small(WAKE_VALUE))),
    )
    .args(placeholder_arg)
    .mailbox(ProbeMailbox::new(atoms.undefined()))
    .block(vec![
        Instr::Label(aux_label(0)),
        Instr::Move {
            src: Operand::Integer(WAKE_VALUE),
            dst: Operand::X(0),
        },
        Instr::Return,
    ])
    .note("an empty mailbox suspends at the target label; the harness resumes there and the aux block returns 77")
}

/// `timeout` — the receive's timeout fired: reset the scan to the head.
///
/// The scan is pre-advanced to 1 (`receive_skip` on the row's own mailbox handle), so
/// the row drives the *reset* rather than a no-op at position 0.
fn timeout_row(atoms: &ProbeAtoms) -> Probe {
    let mut mailbox = two_message_mailbox(atoms);
    mailbox
        .receive_skip()
        .expect("a two-message queue can advance its scan to 1");
    Probe::new(Instr::Timeout, Expect::Returned(Some(make_small(PLACEHOLDER))))
        .args(placeholder_arg)
        .mailbox(mailbox)
        .note("resets a scan pre-advanced to 1 back to the head (receive_reset), then falls through to the success sink")
}

/// `remove_message` — unlink the matched candidate and reposition at the head.
///
/// The scan sits at the head, so the row unlinks the first message and repositions;
/// the run then falls through, returning the untouched [`PLACEHOLDER`].
fn remove_message_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::RemoveMessage,
        Expect::Returned(Some(make_small(PLACEHOLDER))),
    )
    .args(placeholder_arg)
    .mailbox(two_message_mailbox(atoms))
    .note("unlinks the candidate at scan 0 and repositions the scan at the head (receive_remove), then falls through")
}

/// `recv_marker_reserve Dst` — stash an opaque marker for the current scan position.
///
/// The queue is **non-empty** on purpose: a correct host answers the `undefined` atom
/// when there is no position to record, so an empty queue would pin nothing about the
/// reservation. `x(0)` receives the host's small-int token (recording position 0); the
/// engine itself never looks at it again, so the row pins the shape, not the token.
fn recv_marker_reserve_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::RecvMarkerReserve {
            marker: Operand::X(0),
        },
        Expect::Returned(None),
    )
    .mailbox(ProbeMailbox::with_messages(
        atoms.undefined(),
        vec![make_small(FIRST)],
    ))
    .note("a non-empty queue yields a small-int marker recording scan position 0 (an empty queue answers the `undefined` atom)")
}

/// `recv_marker_bind Marker Reference` — bind the marker to the reference.
///
/// The marker is **never fabricated**: the prologue reserves it from the host (the
/// only producer of a valid one) into `x(0)`, and `x(1)` holds the reference.
fn recv_marker_bind_row(atoms: &ProbeAtoms) -> Probe {
    Probe::new(
        Instr::RecvMarkerBind {
            marker: Operand::X(0),
            reference: Operand::X(1),
        },
        Expect::Returned(None),
    )
    .setup(vec![
        Instr::Move {
            src: Operand::Integer(REFERENCE),
            dst: Operand::X(1),
        },
        Instr::RecvMarkerReserve {
            marker: Operand::X(0),
        },
    ])
    .mailbox(ProbeMailbox::with_messages(
        atoms.undefined(),
        vec![make_small(FIRST)],
    ))
    .note("the prologue reserves the marker (an opaque host term) and the row binds it to the reference in x(1)")
}

/// `recv_marker_use Reference` — restore the scan position the reference recorded.
///
/// The row's scan starts at 1, so `recv_marker_reserve` records position **1**; the
/// prologue's trailing `timeout` then resets the scan to 0, which is what makes the
/// reposition back to 1 a real move rather than a no-op.
fn recv_marker_use_row(atoms: &ProbeAtoms) -> Probe {
    let mut mailbox = two_message_mailbox(atoms);
    mailbox
        .receive_skip()
        .expect("a two-message queue can advance its scan to 1");
    Probe::new(
        Instr::RecvMarkerUse {
            reference: Operand::X(1),
        },
        Expect::Returned(None),
    )
    .setup(vec![
        Instr::Move {
            src: Operand::Integer(REFERENCE),
            dst: Operand::X(1),
        },
        Instr::RecvMarkerReserve {
            marker: Operand::X(0),
        },
        Instr::RecvMarkerBind {
            marker: Operand::X(0),
            reference: Operand::X(1),
        },
        Instr::Timeout,
    ])
    .mailbox(mailbox)
    .note("reserve records scan position 1 and the trailing `timeout` resets it to 0, so the row's reposition back to 1 is a real move")
}

/// `recv_marker_clear Reference` — release the reference's marker.
///
/// The same prologue as the `use` row minus the trailing `timeout`: the marker is
/// reserved at position 1, bound, and then dropped by the instruction under test (an
/// unknown/fabricated reference would be a silent no-op, which is exactly what the
/// host-produced marker rules out).
fn recv_marker_clear_row(atoms: &ProbeAtoms) -> Probe {
    let mut mailbox = two_message_mailbox(atoms);
    mailbox
        .receive_skip()
        .expect("a two-message queue can advance its scan to 1");
    Probe::new(
        Instr::RecvMarkerClear {
            reference: Operand::X(1),
        },
        Expect::Returned(None),
    )
    .setup(vec![
        Instr::Move {
            src: Operand::Integer(REFERENCE),
            dst: Operand::X(1),
        },
        Instr::RecvMarkerReserve {
            marker: Operand::X(0),
        },
        Instr::RecvMarkerBind {
            marker: Operand::X(0),
            reference: Operand::X(1),
        },
    ])
    .mailbox(mailbox)
    .note("the prologue reserves the marker at position 1 and binds it; the instruction under test drops that marker entry")
}

/// A mailbox holding [`FIRST`] then [`SECOND`], scan at the head.
fn two_message_mailbox(atoms: &ProbeAtoms) -> ProbeMailbox {
    ProbeMailbox::with_messages(
        atoms.undefined(),
        vec![make_small(FIRST), make_small(SECOND)],
    )
}
