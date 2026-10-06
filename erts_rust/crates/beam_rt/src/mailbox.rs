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

use beam_message::{Message, Queue, QueueCursor};
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
#[derive(Debug, Default)]
pub(crate) struct MailboxState {
    /// The scan position `loop_rec` peeks at and `loop_rec_end` advances.
    cursor: QueueCursor,
    /// The reserved markers, in reservation order.
    markers: Vec<Marker>,
    /// Distinguishes consecutive reserved marker terms.
    next_marker: i64,
}

impl MailboxState {
    /// `loop_rec`: the payload of the message at the scan position, `None` when the
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
    /// A cursor past the end (an empty queue) removes nothing but still repositions.
    pub(crate) fn remove(&mut self, queue: &mut Queue) {
        queue.remove_at(self.cursor);
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
}
