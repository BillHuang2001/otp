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

//! Per-process mailbox scan state: the receive cursor and the marker table.
//!
//! The engine's receive family never keeps mailbox state — it only **positions** the
//! scan through the [`ExecutionContext`](beam_engine::ExecutionContext) seam and lets
//! the compiled match tests decide which candidate fits (see
//! `../beam_engine/src/context.rs`). The state that position lives in is this module's
//! [`MailboxState`]:
//!
//! * the **scan cursor** ([`QueueCursor`]) that `loop_rec` peeks at and `loop_rec_end`
//!   advances;
//! * the **marker table** the `recv_marker_*` prologue saves positions in.
//!
//! Neither can live in a per-call context: a `receive` that suspends on an empty
//! mailbox is resumed later — possibly only after another process has delivered a
//! message — and the scan must continue exactly where it stopped. The state is
//! therefore stored **per process**, keyed by [`ProcessId`](beam_process::ProcessId) in
//! [`Runtime::mailboxes`](crate::runtime::Runtime), so it outlives any single
//! [`RuntimeContext`](crate::context::RuntimeContext) and is reused by the later
//! scheduler pass that runs a process across many contexts.
//!
//! The operations mirror the C message-queue contract the receive family drives
//! (`../../erts/emulator/beam/erl_proc_sig_queue.h:2225-2394`: peek at the save
//! position, `set_save_next` advances it, `unlink_msg_set_save_first` removes the
//! peeked message and resets the scan, `set_save_first` resets it). They are the real
//! counterpart of the reference `TestMailbox` in `../beam_engine/tests/receive.rs`,
//! with identical semantics — that test's cases are the ones these must satisfy.
//!
//! `unsafe`-free: every queue access is a bounds-safe [`Queue`] method (a past-the-end
//! cursor or an empty queue yields `None`, never a panic).

use beam_message::{Fragments, Message, Queue, QueueCursor};
use beam_term::{is_small, make_small, Eterm};

/// The small-integer base of the opaque marker terms [`MailboxState::reserve`] hands out.
///
/// A marker is **opaque** to the engine — it only stores the term and hands it back
/// verbatim — so any distinct small integer serves; the base merely makes a marker
/// recognizable in a trace. It must be a *small* integer so
/// [`MailboxState::bind`] can tell it apart from the empty-queue `undefined`.
const MARKER_BASE: i64 = 1 << 20;

/// One reserved receive marker.
#[derive(Debug)]
struct Marker {
    /// The opaque term [`MailboxState::reserve`] returned.
    term: Eterm,
    /// The reference [`MailboxState::bind`] bound the marker to, if any.
    reference: Option<Eterm>,
    /// The scan position recorded when the marker was reserved.
    position: usize,
}

/// The receive scan state of one process's mailbox.
///
/// See the module docs: it outlives any single context, because a suspended receive
/// resumes against exactly this position. [`MailboxState::default`] is the state of a
/// fresh process — cursor at the first message, no markers.
#[derive(Default)]
pub(crate) struct MailboxState {
    /// The scan position `loop_rec` peeks at and `loop_rec_end` advances.
    cursor: QueueCursor,
    /// The reserved markers, in reservation order.
    markers: Vec<Marker>,
    /// Distinguishes consecutive reserved marker terms.
    next_marker: i64,
    /// The fragments of every removed message whose payload lived in one.
    ///
    /// A message payload that did not fit the receiver's heap lives in a **heap fragment**
    /// the message owns; the running code holds only the copied *term*. Dropping the
    /// message on `remove_message` would therefore free the fragment under the code that
    /// just bound it. The legacy C *moves* the fragment onto the process heap
    /// (`erts_move_multi_frags`, `copy.c:2049-2111`); this runtime keeps the fragment alive
    /// here instead, because the receiver's heap cannot be borrowed at the same time as its
    /// mailbox from outside `beam_process` (see the crate `CONTEXT.md` handoff). The
    /// fragments are released when the process's scan state is.
    retained_fragments: Vec<Fragments>,
}

impl MailboxState {
    /// The number of heap-fragment chains this scan is keeping alive (see
    /// [`MailboxState::retained_fragments`]).
    #[cfg(test)]
    pub(crate) fn retained(&self) -> usize {
        self.retained_fragments.len()
    }

    /// Removes the message at `cursor`, keeping a **fragment-resident** payload alive.
    ///
    /// The only difference from a bare [`Queue::remove_at`] is what happens to a message
    /// that owns [`Fragments`]: they are taken off the message (which is then dropped) and
    /// retained on this state, so the payload term the running code holds stays valid. A
    /// message whose payload lives in the receiver's heap is dropped exactly as before.
    pub(crate) fn remove_at(&mut self, queue: &mut Queue, cursor: QueueCursor) {
        if let Some(mut message) = queue.remove_at(cursor) {
            if let Some(fragments) = message.fragments_mut() {
                self.retained_fragments.push(std::mem::take(fragments));
            }
        }
    }    /// `loop_rec`: the payload of the message at the scan position, `None` when the
    /// scan is exhausted (an empty queue, or every candidate already scanned past).
    ///
    /// **Peeks** — the cursor does not move; only [`MailboxState::skip`] advances it.
    pub(crate) fn next(&self, queue: &Queue) -> Option<Eterm> {
        queue.message_at(self.cursor).map(Message::payload)
    }

    /// `loop_rec_end`: advance the scan past the current candidate.
    ///
    /// Saturating: once past the last message the cursor stays past the end.
    pub(crate) fn skip(&mut self, queue: &Queue) {
        queue.advance(&mut self.cursor);
    }

    /// `remove_message`: unlink the matched candidate and reposition the scan at the
    /// first message.
    ///
    /// A cursor past the end (an empty queue) removes nothing but still repositions. A
    /// removed message that owns heap [`Fragments`] hands them to this state so the payload
    /// stays valid ([`MailboxState::remove_at`]).
    pub(crate) fn remove(&mut self, queue: &mut Queue) {
        self.remove_at(queue, self.cursor);
        self.cursor = queue.cursor();
    }

    /// `timeout`: reposition the scan at the first message.
    pub(crate) fn reset(&mut self, queue: &Queue) {
        self.cursor = queue.cursor();
    }

    /// `recv_marker_reserve`: an opaque marker term recording the current scan position.
    ///
    /// An **empty** queue has no position to record, so — exactly like the C
    /// `am_undefined` — the caller's `undefined` term is returned instead;
    /// [`MailboxState::bind`] then treats it as a no-op.
    pub(crate) fn reserve(&mut self, queue: &Queue, undefined: Eterm) -> Eterm {
        if queue.is_empty() {
            return undefined;
        }
        let marker = make_small(MARKER_BASE + self.next_marker);
        self.next_marker += 1;
        self.markers.push(Marker {
            term: marker,
            reference: None,
            position: queue.position(self.cursor),
        });
        marker
    }

    /// `recv_marker_bind`: bind the opaque `marker` to `reference`.
    ///
    /// A **non-small** marker (the empty-queue `undefined`) is a silent no-op, exactly
    /// like the C (`erl_proc_sig_queue.h:2238-2263`).
    pub(crate) fn bind(&mut self, marker: Eterm, reference: Eterm) {
        if !is_small(marker) {
            return;
        }
        if let Some(entry) = self.markers.iter_mut().find(|entry| entry.term == marker) {
            entry.reference = Some(reference);
        }
    }

    /// `recv_marker_use`: set the scan position to the one `reference`'s marker recorded.
    ///
    /// An unknown reference is a no-op.
    pub(crate) fn use_reference(&mut self, queue: &Queue, reference: Eterm) {
        let position = self
            .markers
            .iter()
            .find(|entry| entry.reference == Some(reference))
            .map(|entry| entry.position);
        if let Some(position) = position {
            self.cursor = queue.cursor_at(position);
        }
    }

    /// `recv_marker_clear`: release `reference`'s marker.
    ///
    /// An unknown reference is a no-op.
    pub(crate) fn clear(&mut self, reference: Eterm) {
        if let Some(index) = self
            .markers
            .iter()
            .position(|entry| entry.reference == Some(reference))
        {
            self.markers.remove(index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_term::NIL;

    /// A queue holding the small integers `values`, in order.
    fn queue_of(values: &[i64]) -> Queue {
        let mut queue = Queue::new();
        for value in values {
            queue.enqueue(Message::new(make_small(*value), NIL));
        }
        queue
    }

    /// The payloads queued, in order (the queue's own payload accessors).
    fn payloads(queue: &Queue) -> Vec<i64> {
        queue
            .iter()
            .map(|message| beam_term::signed_value(message.payload()))
            .collect()
    }

    #[test]
    fn next_peeks_and_skip_advances() {
        let queue = queue_of(&[1, 2, 3]);
        let mut state = MailboxState::default();

        // Peeking twice yields the same candidate — `loop_rec` does not advance.
        assert_eq!(state.next(&queue), Some(make_small(1)));
        assert_eq!(state.next(&queue), Some(make_small(1)));

        state.skip(&queue);
        assert_eq!(state.next(&queue), Some(make_small(2)));
        state.skip(&queue);
        state.skip(&queue);
        assert_eq!(state.next(&queue), None, "the scan is exhausted");

        // Saturating: advancing past the end stays past the end.
        state.skip(&queue);
        assert_eq!(state.next(&queue), None);
    }

    #[test]
    fn an_empty_queue_yields_no_candidate() {
        let queue = Queue::new();
        let mut state = MailboxState::default();
        assert_eq!(state.next(&queue), None);
        // Every positioning operation on an empty queue is safe and stays exhausted.
        state.skip(&queue);
        state.reset(&queue);
        state.remove(&mut Queue::new());
        assert_eq!(state.next(&queue), None);
    }

    #[test]
    fn remove_unlinks_the_candidate_and_resets_the_scan() {
        let mut queue = queue_of(&[1, 2, 3]);
        let mut state = MailboxState::default();
        state.skip(&queue); // cursor at `2`

        state.remove(&mut queue);

        assert_eq!(payloads(&queue), vec![1, 3], "only the candidate was removed");
        assert_eq!(
            state.next(&queue),
            Some(make_small(1)),
            "remove_message repositions the scan at the first message"
        );
    }

    #[test]
    fn reset_repositions_at_the_first_message() {
        let queue = queue_of(&[1, 2]);
        let mut state = MailboxState::default();
        state.skip(&queue);
        assert_eq!(state.next(&queue), Some(make_small(2)));
        state.reset(&queue);
        assert_eq!(state.next(&queue), Some(make_small(1)));
    }

    #[test]
    fn a_marker_reserved_on_an_empty_queue_is_the_undefined_term() {
        let queue = Queue::new();
        let mut state = MailboxState::default();
        let undefined = Eterm::from_raw(0xAB);
        assert_eq!(state.reserve(&queue, undefined), undefined);
        // Binding the `undefined` marker is a no-op, and using it cannot move the scan.
        state.bind(undefined, make_small(7));
        state.use_reference(&queue, make_small(7));
        assert_eq!(state.next(&queue), None);
    }

    #[test]
    fn a_marker_moves_the_scan_back_to_its_recorded_position() {
        // The seam's contract, exercised directly (the reference
        // `the_mailbox_repositions_the_scan_at_a_marker`): a marker reserved with a
        // non-empty queue records the position, and `use_reference` jumps the scan to it.
        let mut queue = queue_of(&[1]);
        let mut state = MailboxState::default();
        state.skip(&queue); // past `1`
        let marker = state.reserve(&queue, make_small(-1));
        assert!(is_small(marker), "a recorded marker is a small integer");
        state.bind(marker, make_small(999));
        queue.enqueue(Message::new(make_small(2), NIL));

        state.reset(&queue);
        assert_eq!(state.next(&queue), Some(make_small(1)));
        state.use_reference(&queue, make_small(999));
        assert_eq!(
            state.next(&queue),
            Some(make_small(2)),
            "the marker moved the scan past the first message"
        );

        // A cleared marker no longer repositions the scan.
        state.clear(make_small(999));
        state.reset(&queue);
        state.use_reference(&queue, make_small(999));
        assert_eq!(state.next(&queue), Some(make_small(1)));
    }

    #[test]
    fn an_unknown_reference_is_a_silent_no_op() {
        let queue = queue_of(&[1, 2]);
        let mut state = MailboxState::default();
        state.skip(&queue);
        state.use_reference(&queue, make_small(12345));
        assert_eq!(
            state.next(&queue),
            Some(make_small(2)),
            "an unknown marker reference leaves the scan where it was"
        );
        state.clear(make_small(12345));
        assert_eq!(state.next(&queue), Some(make_small(2)));
    }

    #[test]
    fn consecutive_markers_get_distinct_terms() {
        let queue = queue_of(&[1, 2]);
        let mut state = MailboxState::default();
        let first = state.reserve(&queue, make_small(-1));
        let second = state.reserve(&queue, make_small(-1));
        assert_ne!(first, second);
    }

    /// The scan keeps only **fragment-resident** payloads alive: an ordinary message whose
    /// payload lives in the receiver's own heap is dropped exactly as before, so the
    /// fragment path adds no retention to the common case.
    #[test]
    fn removing_a_heap_resident_message_retains_nothing() {
        let mut queue = queue_of(&[1, 2, 3]);
        let mut state = MailboxState::default();
        state.skip(&queue);
        state.remove(&mut queue);
        assert_eq!(payloads(&queue), vec![1, 3]);
        assert_eq!(state.retained(), 0, "a heap-resident payload needs no keeping");
    }

    /// A payload **larger than the receiver's whole heap** is delivered intact — through the
    /// heap-fragment model — and still wakes the blocked receiver.
    ///
    /// `recv:wait/0` is `receive X -> X end` (the committed oracle fixture
    /// `../beam_engine/tests/fixtures/recv.beam`): it returns the message it took. The whole
    /// delivery runs the way an `-eval` send does (from outside a scheduler slice), so the
    /// wake has to survive to a later pass as well.
    #[test]
    fn an_oversized_payload_reaches_a_blocked_receiver_through_a_fragment() {
        use beam_engine::{terms, ExecutionContext};
        use beam_process::ProcessState;

        use crate::context::RuntimeContext;
        use crate::runtime::Runtime;

        /// The committed receive fixture (reused by relative path — never copied).
        const RECV_BEAM: &str = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../beam_engine/tests/fixtures/recv.beam"
        );

        let mut rt = Runtime::with_defaults().expect("the default runtime");
        rt.load_module(RECV_BEAM).expect("recv.beam loads");
        let receiver = rt
            .spawn_process("recv", "wait", vec![])
            .expect("spawn recv:wait/0");

        // 1. The receiver blocks on its empty mailbox *before* anything is sent, so the
        //    delivery must wake it as well as hand it the payload.
        assert_eq!(rt.run_one_slice().expect("one slice"), 1);
        assert_eq!(
            rt.processes().get(receiver).expect("the receiver is alive").state(),
            ProcessState::Waiting,
            "the receiver is blocked before anything is sent"
        );

        // 2. A payload far larger than that receiver's heap. It is built on the runtime's
        //    pointer-stable arena; the assertion is the precondition the fragment path
        //    exists for — the payload genuinely cannot fit.
        let elems: Vec<Eterm> = (1..=4_000i64).map(make_small).collect();
        let payload = {
            let mut ctx = RuntimeContext::new(&mut rt);
            terms::make_list(&mut ctx, &elems).expect("the payload")
        };
        let heap_available = rt
            .processes()
            .get(receiver)
            .expect("the receiver is alive")
            .heap()
            .available();
        let needed = beam_term::size_object(payload);
        assert!(
            needed > heap_available,
            "the payload ({needed} words) must exceed the receiver's {heap_available}-word heap"
        );

        // 3. Deliver from outside a scheduler slice. The receiver is blocked (so this wakes
        //    it) and its heap cannot hold the payload (so `deliver_simple` reports
        //    `Alloc(Message)` and the send path must fall back to a heap fragment rather
        //    than fail the `!`).
        {
            let mut ctx = RuntimeContext::new(&mut rt);
            ctx.send_message(receiver.to_eterm(), payload)
                .expect("the oversized delivery does not fail");
        }

        // 4. What is queued really is a fragment-resident payload.
        {
            let process = rt.processes().get(receiver).expect("the receiver is alive");
            let message = process
                .mailbox()
                .iter()
                .next()
                .expect("one message is queued");
            let fragments = message
                .fragments()
                .expect("an oversized payload lives in a heap fragment");
            assert_eq!(fragments.len(), 1, "one fragment holds the whole payload");
            assert!(
                fragments
                    .iter()
                    .next()
                    .expect("one fragment")
                    .contains(message.payload()),
                "the message payload points into the fragment"
            );
        }

        // 5. The wake the fragment delivery produced is still pending (it is recorded for
        //    the scheduler pass, not enqueued on the spot), and the pass absorbs it and runs
        //    the receiver — which returns the payload intact.
        assert_eq!(rt.run_queue_len(), 0, "a fragment wake is absorbed by a pass");
        assert!(
            rt.run_scheduler().expect("the pass runs the woken receiver") >= 1,
            "the wake turned into a slice"
        );
        let result = rt.process_result(receiver).expect("recv:wait/0 returned");
        assert!(
            beam_term::compare::eq(payload, result, rt.atoms()),
            "the oversized payload arrived intact"
        );
        assert_eq!(
            rt.mailboxes.get(&receiver).expect("the scan state").retained(),
            1,
            "the receive kept the fragment (and so the bound payload) alive"
        );
    }
}
