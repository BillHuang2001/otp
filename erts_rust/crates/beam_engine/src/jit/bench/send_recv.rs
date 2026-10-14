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

//! The **message-passing loopback** workload — the one shape in the tier table whose
//! cost is `send` + `receive` rather than arithmetic, a term build or a cross-module
//! call.
//!
//! # Why this shape
//!
//! `../../../../../../tools/bench/RESULTS.md` §8 item 2 records the runtime-level
//! `send_recv` regression: on the **default** baseline-JIT tier a two-process
//! send/receive copy loop is **2.15×–3.31× dearer** than on the interpreter tier. That
//! figure comes from the *external* driver (`tools/bench`), which needs two live
//! processes; this in-crate harness runs **one** process on a
//! [`MinimalContext`](crate::MinimalContext), so it
//! reproduces the same compiled instruction shape in-process and measures it with the
//! tier table's own method.
//!
//! Measured here, the same shape does **not** show that deficit: the baseline tier is
//! net *faster* (min-of-3 ≈ 9268.9 ns/op vs the interpreter's ≈ 13337.7 ns/op, ≈ 1.44×).
//! The two readings do not conflict — this harness's mailbox delivers synchronously, so
//! the `receive` never blocks, there is no second process, no scheduler resume and no
//! ETF cop in the loop, which is exactly the runtime glue the external driver measures.
//! The in-process result therefore localises the runtime regression to the **runtime
//! glue**, not to the tier's lowering of this compiled shape. See
//! [`super`](crate::jit::bench)'s `send_recv/2` measurement note.
//!
//! # The shape
//!
//! The module is the compiled lowering of
//!
//! ```text
//!   send_recv(0, Acc) -> Acc;
//!   send_recv(N, Acc) ->
//!       Self = <the process's own identity>,
//!       Self ! {Self, N},
//!       receive
//!           {Self, T} -> send_recv(N - 1, Acc + T)
//!       end.
//! ```
//!
//! with one substitution: a single [`MinimalContext`](crate::MinimalContext) has no
//! `erlang:self/0`, so the
//! process identity is a small-int constant the body keeps in a Y slot — it is both
//! the `send` destination and the first element of every message, so the receive's
//! **bound-variable** match is a real payload comparison, not a fabricated one. The
//! rest is byte-for-byte the instruction sequence the installed `erlc` emits for the
//! Erlang above (only `{bif,self,…}` becomes a `move` of the constant):
//!
//! ```text
//!  1 label 1
//!    test is_eq_exact Fail=2 [x0, 0]      ; the `send_recv(0, Acc)` clause — falls
//!    move x1 x0                           ; through (and returns Acc) at zero
//!    return
//!  2 label 2                              ; the step clause
//!    allocate_heap 3 3 2
//!    move x1 y1                           ; Acc survives the receive in y1
//!    move x0 y2                           ; N   survives the receive in y2
//!    move Self y0                         ; the process identity (a constant)
//!    put_tuple2 x1 [y0, x0]               ; the message {Self, N}      (DEOPTS)
//!    move y0 x0                           ; the destination             (inlined)
//!    send                                 ; deliver x1 to x0            (DEOPTS)
//!  3 label 3                              ; the receive's loop head
//!    loop_rec Fail=5 x0                   ; the candidate, or suspend    (DEOPTS)
//!    test is_tuple Fail=4 [x0]                                            (inlined)
//!    test test_arity Fail=4 [x0, 2]                                       (inlined)
//!    get_tuple_element x0 0 x1                                            (inlined)
//!    test is_eq_exact Fail=4 [x1, y0]     ; element 0 == Self (bound var) (deopts:
//!                                         ;                              a Y operand)
//!    remove_message                                                       (DEOPTS)
//!    gc_bif '-' 1 [y2, 1] x1              ; x1 = N - 1  (Y operand ⇒ deopts)
//!    get_tuple_element x0 1 x0            ; x0 = T                               (inlined)
//!    gc_bif '+' 2 [y1, x0] x0             ; x0 = Acc + T (Y operand ⇒ deopts)
//!    swap x0 x1                                                           (inlined)
//!    call_last 2 1 3                      ; the tail call to the loop head (inlined)
//!  4 label 4                              ; a non-matching candidate: skip it
//!    loop_rec_end 3                                                       (DEOPTS)
//!  5 label 5                              ; the mailbox is exhausted: suspend
//!    wait 3
//! ```
//!
//! Every step of the loop counts itself once in the accumulator, so a run makes
//! exactly [`SEND_RECV_ROUNDS`] `send` + `receive` round-trips and the exported result
//! is the triangular sum `SEND_RECV_ROUNDS * (SEND_RECV_ROUNDS + 1) / 2` — a figure a
//! loop that skipped, repeated or corrupted a round-trip cannot produce.
//!
//! # The loopback mailbox
//!
//! The engine keeps no mailbox state; the host supplies it through the
//! [`Mailbox`](crate::context::Mailbox) seam. [`LoopbackMailbox`] is that host here: a
//! single-process FIFO whose [`send_message`](Mailbox::send_message) enqueues the
//! message on the **same** queue the following `receive` scans, so the receive always
//! finds exactly the message it just sent and the `wait` (the mailbox-exhausted arm of
//! this compiled shape) is never reached. The scan position is a **saturating** cursor
//! with `beam_message::Queue`'s `QueueCursor` semantics (`remove_message` unlinks the
//! candidate and repositions at the first message), and the `recv_marker_*` opcodes are
//! served over the same cursor rather than rejected. The compiled shape above emits no
//! marker opcode, so the marker support is the harness's completeness, not a measured
//! path.

use std::collections::{HashMap, VecDeque};

use beam_common::Result;
use beam_loader::{HeapNeed, ImportRef, Instr, Label, Operand, TestKind, NO_FAIL};
use beam_term::{make_small, Eterm};

use crate::context::Mailbox;

use super::checks::verify_send_recv;
use super::support::Fixture;
use super::workloads::{Args, Workload};

// ---------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------

/// The module the message-passing workload is entered through.
const SEND_RECV_MODULE: &str = "jit_bench_send_recv";

/// The number of `send` + `receive` round-trips one run performs.
///
/// Every round-trip sends the current counter and adds it to the accumulator, so the
/// exported result is the triangular number `SEND_RECV_ROUNDS * (SEND_RECV_ROUNDS + 1)
/// / 2`. It matches [`SUM_LENGTH`](super::workloads::SUM_LENGTH) and
/// [`WALK_LENGTH`](super::workloads::WALK_LENGTH), so the message-passing loop and the
/// two list walks are comparable.
pub(super) const SEND_RECV_ROUNDS: usize = 64;

/// The small-int process identity the loopback mailbox exchanges messages with.
///
/// A real Erlang process would use `erlang:self/0`; one process on a
/// [`MinimalContext`] has no `self/0` BIF, so the identity is a constant the module
/// carries in `y0` (as `self()` would be). It is both the `send` destination and the
/// first element of every message, so the receive's bound-variable match compares a
/// real payload value.
const SELF_IDENTITY: i64 = 0;

// ---------------------------------------------------------------------------
// The workload
// ---------------------------------------------------------------------------

/// `send_recv/2` — [`SEND_RECV_ROUNDS`] `send` + `receive` loopback round-trips.
///
/// The module is the compiled shape documented at the top of this file: a
/// tail-recursive loop that sends `{Self, N}` to itself, receives it back through its
/// own mailbox, and folds the payload into an accumulator. See the module docs for the
/// instruction-by-instruction lowering and its deopt story.
pub(super) fn send_recv_workload() -> Workload {
    let mut fixture = Fixture::with_plus(SEND_RECV_MODULE);
    // `erlang:'+'/2` is index 0 (`Fixture::with_plus`); `erlang:'-'/2` is index 1.
    let minus = fixture.import("erlang", "-", 2);
    let plus = ImportRef(0);
    let entry = Label(1);
    let step = Label(2);
    let recv_loop = Label(3);
    let skip = Label(4);
    let wait = Label(5);
    let code = vec![
        Instr::Label(entry),
        // The `send_recv(0, Acc) -> Acc` clause: `is_eq_exact` continues at zero (the
        // base case) and jumps to the step clause for every other counter. A BEAM
        // `test` jumps on the *negation* of its predicate, so `is_eq_exact` is
        // exactly the compiled clause test.
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: step,
            args: vec![Operand::X(0), Operand::Integer(0)],
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::X(0),
        },
        Instr::Return,
        Instr::Label(step),
        Instr::AllocateHeap {
            stack_need: HeapNeed::Words(3),
            heap_need: HeapNeed::Words(3),
            live: 2,
        },
        Instr::Move {
            src: Operand::X(1),
            dst: Operand::Y(1),
        },
        Instr::Move {
            src: Operand::X(0),
            dst: Operand::Y(2),
        },
        // `Self = self()` — the constant process identity the mailbox loopback
        // exchanges with (see [`SELF_IDENTITY`]).
        Instr::Move {
            src: Operand::Integer(SELF_IDENTITY),
            dst: Operand::Y(0),
        },
        // `Self ! {Self, N}` — build the message into x(1), then send x(1) to x(0).
        Instr::PutTuple2 {
            dst: Operand::X(1),
            elements: vec![Operand::Y(0), Operand::X(0)],
        },
        Instr::Move {
            src: Operand::Y(0),
            dst: Operand::X(0),
        },
        Instr::Send,
        Instr::Label(recv_loop),
        Instr::LoopRec {
            fail: wait,
            dst: Operand::X(0),
        },
        Instr::Test {
            kind: TestKind::IsTuple,
            fail: skip,
            args: vec![Operand::X(0)],
        },
        Instr::Test {
            kind: TestKind::TestArity,
            fail: skip,
            args: vec![Operand::X(0), Operand::Integer(2)],
        },
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 0,
            dst: Operand::X(1),
        },
        // The bound-variable match: `{Self, T}` compares the message's first element
        // against the `Self` the body carries in its Y slot.
        Instr::Test {
            kind: TestKind::IsEqExact,
            fail: skip,
            args: vec![Operand::X(1), Operand::Y(0)],
        },
        Instr::RemoveMessage,
        // `x1 = N - 1` — the decremented counter (the `erlang:'-'/2` import at 1).
        Instr::GcBif {
            arity: 2,
            bif: minus,
            fail: NO_FAIL,
            live: 1,
            args: vec![Operand::Y(2), Operand::Integer(1)],
            dst: Operand::X(1),
        },
        Instr::GetTupleElement {
            source: Operand::X(0),
            element: 1,
            dst: Operand::X(0),
        },
        // `x0 = Acc + T` — the payload folded into the accumulator.
        Instr::GcBif {
            arity: 2,
            bif: plus,
            fail: NO_FAIL,
            live: 2,
            args: vec![Operand::Y(1), Operand::X(0)],
            dst: Operand::X(0),
        },
        // `x0`/`x1` are the tail call's argument registers (`counter`, `acc`).
        Instr::Swap {
            a: Operand::X(0),
            b: Operand::X(1),
        },
        Instr::CallLast {
            arity: 2,
            target: entry,
            deallocate: 3,
        },
        // A candidate that does not match: advance the scan and try the next one.
        Instr::Label(skip),
        Instr::LoopRecEnd { target: recv_loop },
        // The mailbox is exhausted: suspend, resuming at the loop head. The loopback
        // mailbox always enqueues before the receive, so this is unreachable here —
        // it is present because it is part of the compiled shape.
        Instr::Label(wait),
        Instr::Wait { target: recv_loop },
    ];
    let (module, function) = fixture.finish("send_recv", 2, code);
    Workload::new(
        format!(
            "send_recv/2 ({SEND_RECV_ROUNDS} send + receive loopback round-trips \
             through the mailbox)"
        ),
        module,
        function,
        2,
        Args::Fixed(vec![make_small(SEND_RECV_ROUNDS as i64), make_small(0)]),
        verify_send_recv,
    )
    .with_mailbox(new_mailbox)
}

// ---------------------------------------------------------------------------
// The loopback mailbox
// ---------------------------------------------------------------------------

/// A fresh single-process loopback mailbox (the factory a [`Workload`] hands the
/// harness).
pub(super) fn new_mailbox() -> Box<dyn Mailbox> {
    Box::new(LoopbackMailbox::new())
}

/// A one-process **loopback** mailbox: `send_message` enqueues the message on the same
/// queue the following `receive` scans, so a `send`/`receive` pair inside one process
/// completes without a second process.
///
/// The scan position is a **saturating** cursor over the FIFO, with the semantics
/// `beam_message::Queue`'s `QueueCursor` gives the production queue: `loop_rec` reads
/// the candidate at the cursor, `loop_rec_end` advances it, `remove_message` unlinks
/// the candidate and repositions at the first message, and `timeout` resets to the
/// first message. The `recv_marker_*` opcodes are served over the same cursor: the
/// marker returned by `recv_marker_reserve` records the current position, a
/// `recv_marker_bind` associates it with the reference the engine hands back, a
/// `recv_marker_use` restores the recorded position and a `recv_marker_clear` releases
/// it.
///
/// The messages are terms the **context's** arena owns (`send` hands over an `Eterm`
/// the engine built through `ExecutionContext::alloc_words`), and the mailbox lives
/// inside that same context, so a queued term stays valid for the whole measurement.
struct LoopbackMailbox {
    /// The FIFO of delivered messages, in delivery order.
    queue: VecDeque<Eterm>,
    /// The current scan position (a saturating index into `queue`).
    cursor: usize,
    /// The scan position each opaque marker recorded, keyed by the marker term.
    reserved: HashMap<Eterm, usize>,
    /// The marker each reference (from `recv_marker_bind`) names.
    bound: HashMap<Eterm, Eterm>,
    /// The next opaque marker token (never reused, so two reservations never alias).
    next_marker: i64,
}

impl LoopbackMailbox {
    /// An empty loopback mailbox, positioned at its first (not yet delivered) message.
    fn new() -> LoopbackMailbox {
        LoopbackMailbox {
            queue: VecDeque::new(),
            cursor: 0,
            reserved: HashMap::new(),
            bound: HashMap::new(),
            next_marker: 0,
        }
    }
}

impl Mailbox for LoopbackMailbox {
    fn send_message(&mut self, _to: Eterm, message: Eterm) -> Result<()> {
        // The loopback: the message is delivered to this very process's queue. The
        // destination is deliberately ignored — one process has no peers, so the only
        // faithful delivery is to itself.
        self.queue.push_back(message);
        Ok(())
    }

    fn receive_next(&mut self) -> Result<Option<Eterm>> {
        Ok(self.queue.get(self.cursor).copied())
    }

    fn receive_skip(&mut self) -> Result<()> {
        // A **saturating** advance: skipping past the last message leaves the cursor at
        // the end (the next `receive_next` reports exhaustion) rather than wrapping.
        if self.cursor < self.queue.len() {
            self.cursor += 1;
        }
        Ok(())
    }

    fn receive_remove(&mut self) -> Result<()> {
        // `remove_message`: unlink the matched candidate and reposition the scan at the
        // first message (C's `erts_msgq_unlink_msg_set_save_first`).
        if self.cursor < self.queue.len() {
            self.queue.remove(self.cursor);
        }
        self.cursor = 0;
        Ok(())
    }

    fn receive_reset(&mut self) -> Result<()> {
        // `timeout`: restart the scan at the first message.
        self.cursor = 0;
        Ok(())
    }

    fn receive_marker_reserve(&mut self) -> Result<Eterm> {
        // The marker is opaque to the engine: any distinct term will do, so it is a
        // fresh small int that records the position it stood for.
        let marker = make_small(self.next_marker);
        self.next_marker += 1;
        self.reserved.insert(marker, self.cursor);
        Ok(marker)
    }

    fn receive_marker_bind(&mut self, marker: Eterm, reference: Eterm) -> Result<()> {
        // A marker a later `recv_marker_use` could not resolve would silently leave the
        // scan in the wrong place, so an unknown marker is a typed error, never a
        // no-op.
        match self.reserved.get(&marker) {
            Some(_) => {
                self.bound.insert(reference, marker);
                Ok(())
            }
            None => Err(crate::error::unsupported(
                "receive_marker_bind: unknown receive marker",
            )),
        }
    }

    fn receive_marker_use(&mut self, reference: Eterm) -> Result<()> {
        match self
            .bound
            .get(&reference)
            .and_then(|marker| self.reserved.get(marker))
        {
            Some(&position) => {
                self.cursor = position.min(self.queue.len());
                Ok(())
            }
            None => Err(crate::error::unsupported(
                "receive_marker_use: unknown receive reference",
            )),
        }
    }

    fn receive_marker_clear(&mut self, reference: Eterm) -> Result<()> {
        if let Some(marker) = self.bound.remove(&reference) {
            self.reserved.remove(&marker);
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Guards
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use beam_term::signed_value;

    use crate::context::MinimalContext;
    use crate::machine::Machine;

    /// Walk every instruction of the workload's one function.
    fn instructions(workload: &Workload) -> impl Iterator<Item = &Instr> {
        workload
            .module
            .functions
            .iter()
            .flat_map(|function| function.code.iter())
    }

    /// The workload exists to measure exactly one shape: the compiled `send` + `receive`
    /// loop. Drop the `send`, the `loop_rec`, the `wait` or the bound-variable match and
    /// it would keep passing the tier-agreement check while silently measuring
    /// arithmetic — so the shape is pinned here. The `wait` is the mailbox-exhausted arm
    /// of the same compiled shape (unreachable with a loopback mailbox, but it is what
    /// makes the `loop_rec` a real receive rather than a one-shot fetch).
    #[test]
    fn the_send_recv_workload_keeps_its_send_and_receive_shape() {
        let workload = send_recv_workload();
        let mut sends = 0;
        let mut loop_recs = 0;
        let mut loop_rec_ends = 0;
        let mut waits = 0;
        let mut removes = 0;
        let mut tuple_builds = 0;
        let mut bound_matches = 0;
        for instr in instructions(&workload) {
            match instr {
                Instr::Send => sends += 1,
                Instr::LoopRec { .. } => loop_recs += 1,
                Instr::LoopRecEnd { .. } => loop_rec_ends += 1,
                Instr::Wait { .. } => waits += 1,
                Instr::RemoveMessage => removes += 1,
                Instr::PutTuple2 { .. } => tuple_builds += 1,
                // The bound-variable match is an `is_eq_exact` whose right operand is
                // the `Self` the body carries in a Y slot.
                Instr::Test {
                    kind: TestKind::IsEqExact,
                    args,
                    ..
                } if args.iter().any(|operand| matches!(operand, Operand::Y(_))) => {
                    bound_matches += 1;
                }
                _ => {}
            }
        }
        assert_eq!(sends, 1, "the loop must keep exactly one `send`");
        assert_eq!(loop_recs, 1, "the loop must keep exactly one `loop_rec`");
        assert_eq!(loop_rec_ends, 1, "the loop must keep its `loop_rec_end` arm");
        assert_eq!(waits, 1, "the loop must keep its `wait` arm");
        assert_eq!(removes, 1, "the loop must keep its `remove_message`");
        assert_eq!(tuple_builds, 1, "the loop must keep exactly one message build");
        assert_eq!(
            bound_matches, 1,
            "the receive must keep its bound-variable equality match"
        );
    }

    /// The loopback mailbox makes the phase terminate: the interpreter must run the
    /// whole `send` + `receive` loop to completion — never suspend (`wait`) — and
    /// return the triangular sum. This is the mailbox-semantics pin the tier table
    /// depends on, kept next to the mailbox it validates.
    #[test]
    fn the_loopback_mailbox_completes_the_send_receive_loop() {
        let workload = send_recv_workload();
        let mut ctx = MinimalContext::with_bif_hook(workload.bif_hook());
        workload.install_mailbox(&mut ctx);
        let args = workload
            .arguments(&mut ctx)
            .expect("the workload's arguments must build");
        let mut machine = Machine::new();
        let result = machine
            .run_export(
                &workload.module,
                workload.function,
                workload.arity,
                &args,
                &mut ctx,
            )
            .expect("the loopback receive must never suspend");
        assert_eq!(
            signed_value(result),
            (SEND_RECV_ROUNDS * (SEND_RECV_ROUNDS + 1) / 2) as i64,
            "the loop must fold every round-trip's payload into the accumulator"
        );
    }
}
