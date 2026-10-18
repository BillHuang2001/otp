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
//! * the **marker table** the `recv_marker_*` prologue saves positions in;
//! * the **retained payloads** — the heap fragments and the off-heap refc-binary, MSO and
//!   external-node objects of every message removed on `remove_message`, kept alive past the
//!   message's own drop so the term the running code bound stays valid
//!   ([`MailboxState::remove_at`]).
//!
//! None of this state can live in a per-call context: a `receive` that suspends on an empty
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
//! # The marker anchor (why a marker is not just the scan position)
//!
//! A receive marker is the compiler's *selective-receive skip*: `lib/compiler` emits
//! `recv_marker_reserve` just **before** the `make_ref`/`monitor` that the receive's
//! leading pattern is built from, `recv_marker_bind` binds the opaque marker to that
//! reference, and every receive that matches on it starts with
//! `recv_marker_use` (`beam_ssa_recv.erl:27-106`). The point is that a message queued
//! *before* the reference existed cannot match, so the scan may start after it.
//!
//! The C therefore does not record the *scan position* at all: `recv_marker_insert`
//! splices a marker **node** into the queue at `sig_qs.last`, i.e. immediately after
//! the last message queued at reserve time
//! (`../../erts/emulator/beam/erl_proc_sig_queue.c:4290-4348`), and
//! `erts_msgq_recv_marker_set_save__` (`erl_proc_sig_queue.h:2151`) makes
//! `recv_marker_use` resume the scan *at that node*. Two properties follow, and both
//! are load-bearing:
//!
//! * **messages enqueued after the reserve are always scanned** — the marker sits
//!   before them, so a receive that suspended at the end of the queue resumes onto
//!   whatever arrived while it waited;
//! * **the anchor survives structural change** — a queue node's identity does not
//!   move when an earlier message is unlinked (`erts_msgq_pass_recv_markers` rewrites
//!   its `prev_next` back-pointer), whereas a raw queue *index* is stale by one after
//!   every `Queue::remove_at` (`../beam_message/src/message.rs:371`).
//!
//! A [`Marker`] here is therefore an **anchor**, not a position: the number of queued
//! messages that lie *before* the marker. It starts at the queue's length at reserve
//! time and counts down whenever a message in front of it is unlinked, which is the
//! index-space equivalent of the C's node. `use_reference` clamps it into `0..=len`
//! so a queue that shrank without going through [`MailboxState::remove_at`] can never
//! park the scan past the end — a past-the-end scan is not a benign state: `loop_rec`
//! reports "no candidate", the compiled code jumps to its `wait`, and the receiver
//! misses every message already queued.
//!
//! One deliberate deviation from the C: an **empty-queue** reserve hands out a usable
//! marker (the C answers `am_undefined`, `erl_proc_sig_queue.h:2225-2236`). The C can
//! afford that because "no marker" makes `recv_marker_use` leave the save pointer
//! alone, which is the front of the queue at that instant; recording the equivalent
//! anchor (`0`) explicitly is behaviourally identical there and strictly stronger
//! afterwards, because the anchor is length-stable while an untouched save pointer is
//! whatever the previous receive left behind.
//!
//! `unsafe`-free: every queue access is a bounds-safe [`Queue`] method (a past-the-end
//! cursor or an empty queue yields `None`, never a panic).

use beam_alloc::RefcBinary;
use beam_message::{Fragments, Message, MsoHandle, NodeEntry, Queue, QueueCursor};
use beam_term::{is_small, make_small, Eterm};

/// The small-integer base of the opaque marker terms [`MailboxState::reserve`] hands out.
///
/// A marker is **opaque** to the engine — it only stores the term and hands it back
/// verbatim — so any distinct small integer serves; the base merely makes a marker
/// recognizable in a trace. It must be a *small* integer so
/// [`MailboxState::bind`] can tell it apart from a non-marker term.
const MARKER_BASE: i64 = 1 << 20;

/// How many markers one process may hold live at a time — the C's
/// `ERTS_RECV_MARKER_BLOCK_SIZE` (`../../erts/emulator/beam/erl_proc_sig_queue.h`).
///
/// The C keeps a fixed-size block and recycles the oldest entry, because a marker is
/// *not* always cleared: `beam_ssa_recv` deliberately emits no `recv_marker_clear` on
/// exception edges ("orphaned markers get recycled very quickly so there's little harm
/// in ignoring exceptions", `lib/compiler/src/beam_ssa_recv.erl:225-232`), so a process
/// that raises out of a marker'd receive between the reserve and the clear would
/// otherwise accumulate one entry per iteration for its whole life. Recycling the
/// oldest is behaviour-safe: an evicted marker is simply unknown, and
/// `bind`/`use`/`clear` treat an unknown marker as a no-op (the scan then starts where
/// it was, which only re-scans messages, never skips one).
const MAX_MARKERS: usize = 64;

/// One reserved receive marker.
#[derive(Debug)]
struct Marker {
    /// The opaque term [`MailboxState::reserve`] returned.
    term: Eterm,
    /// The reference [`MailboxState::bind`] bound the marker to, if any.
    reference: Option<Eterm>,
    /// The marker's **anchor**: how many queued messages lie *before* it.
    ///
    /// Set to the queue's length at reserve time — the C splices the marker in after the
    /// last message (`erl_proc_sig_queue.c:4290-4348`) — and decremented by
    /// [`MailboxState::remove_at`] whenever a message in front of it is unlinked, so the
    /// anchor keeps naming the same queue point after `Queue::remove_at` shifted every
    /// later message down by one (see the module docs, "The marker anchor").
    anchor: usize,
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
    /// The off-heap refc-binary handles of every removed message whose payload owned one.
    ///
    /// A payload copied **from another process** carries its off-heap payloads as
    /// [`RefcBinary`] handles on the message envelope ([`Message::binaries`]): the copy
    /// re-registers each off-heap binary it moved, and the message — not the receiver's
    /// machine — owns those handles. The running code holds only the copied term, so
    /// dropping the message on `remove_message` would release the last handle (once the
    /// sender has terminated) and leave the bound term pointing at freed memory. The handles
    /// are kept alive here instead, exactly like [`MailboxState::retained_fragments`]: each
    /// clone bumps the payload's reference count, so the payload survives the `Message`
    /// drop, and the runner can resolve the term's payload when it copies the term out of the
    /// process ([`Runtime::stabilize_term_with`](crate::runtime::Runtime::stabilize_term_with)).
    /// Released when the process's scan state is.
    retained_binaries: Vec<RefcBinary>,
    /// The off-heap MSO handles of every removed message whose payload owned one.
    ///
    /// A payload carrying an **off-heap MSO reference** (the frozen `[HEADER_BIN_REF, slot, 0]`
    /// container term) resolves its object through the copy's **source** off-heap table by the
    /// `slot` the term records ([`Message::off_heap`] holds the handles in that slot order).
    /// Dropping the message on `remove_message` would release the last handle (once the sender has
    /// terminated) and free the object under the bound term. The handles are therefore cloned
    /// (refcount++) here, exactly like [`MailboxState::retained_binaries`] — and kept **in the
    /// message's own order**, because the source table the runner rebuilds must place each handle
    /// on the same slot its container term names ([`MailboxState::retained_off_heap`]). Released
    /// when the process's scan state is.
    retained_off_heap: Vec<MsoHandle>,
    /// The external-thing node references of every removed message that held one.
    ///
    /// A payload holding an **external pid/port/reference** carries the refcounted
    /// `(node, creation)` identities the copy registered on the envelope ([`Message::nodes`]).
    /// The received payload's identity is kept alive here alongside the other off-heap families;
    /// each retained [`NodeEntry`] is a bare *handle* on the shared interior
    /// ([`NodeEntry::handle`], which does **not** register a reference), so nothing is released
    /// when this state drops and the object's own count is untouched. Released when the process's
    /// scan state is.
    retained_nodes: Vec<NodeEntry>,
}

impl MailboxState {
    /// The number of heap-fragment chains this scan is keeping alive (see
    /// [`MailboxState::retained_fragments`]).
    #[cfg(test)]
    pub(crate) fn retained(&self) -> usize {
        self.retained_fragments.len()
    }

    /// The off-heap refc-binary handles this scan is keeping alive (see
    /// [`MailboxState::retained_binaries`]).
    ///
    /// The runner feeds these into the copy's **source** off-heap table when it transports a
    /// process's return value or exit reason onto the runtime's pointer-stable arena, so a
    /// binary **received from another process** resolves like a locally built one.
    pub(crate) fn retained_binaries(&self) -> &[RefcBinary] {
        &self.retained_binaries
    }

    /// The off-heap MSO handles this scan is keeping alive (see
    /// [`MailboxState::retained_off_heap`]).
    ///
    /// The runner feeds these into the copy's **source** off-heap table — **in this order**, so
    /// each retained handle lands on the same slot the removed message's container term names —
    /// when it transports a process's return value or exit reason onto the runtime's
    /// pointer-stable arena.
    pub(crate) fn retained_off_heap(&self) -> &[MsoHandle] {
        &self.retained_off_heap
    }

    /// The external-thing node identities this scan is keeping alive (see
    /// [`MailboxState::retained_nodes`]).
    ///
    /// The runner surfaces these alongside the retained binaries and MSO handles so the whole
    /// retained family travels together. The copy engine registers a **fresh** [`NodeEntry`] for
    /// every external thing it copies and never consults a source node list, so these are the
    /// retained identity made visible rather than an input the copier reads.
    pub(crate) fn retained_nodes(&self) -> &[NodeEntry] {
        &self.retained_nodes
    }

    /// Removes the message at `cursor`, keeping a **fragment-resident** or **off-heap**
    /// payload alive.
    ///
    /// The only difference from a bare [`Queue::remove_at`] is what happens to a message that
    /// owns off-heap objects: the [`Fragments`] are taken off the message, the envelope's
    /// refc-binary and MSO handles are cloned (refcount++) onto this state, and its external-thing
    /// node identities are retained as share handles — all before the message drops — so the
    /// payload term the running code holds stays valid (see
    /// [`MailboxState::retained_fragments`], [`MailboxState::retained_binaries`],
    /// [`MailboxState::retained_off_heap`] and [`MailboxState::retained_nodes`]). A message whose
    /// payload lives entirely in the receiver's heap is dropped exactly as before.
    pub(crate) fn remove_at(&mut self, queue: &mut Queue, cursor: QueueCursor) {
        let removed = queue.position(cursor);
        if let Some(mut message) = queue.remove_at(cursor) {
            // Every marker anchored *behind* the unlinked message follows the shift:
            // one fewer message lies in front of it now. This is the index-space
            // equivalent of the C rewriting a marker node's `prev_next` back-pointer
            // (`erts_msgq_pass_recv_markers`), and it is what keeps a marker usable
            // across a receive that removed a message it had already scanned past. A
            // marker anchored *at or before* the removed message does not move.
            for marker in &mut self.markers {
                if marker.anchor > removed {
                    marker.anchor -= 1;
                }
            }
            if let Some(fragments) = message.fragments_mut() {
                self.retained_fragments.push(std::mem::take(fragments));
            }
            // Clone (refcount++) the envelope's refc-binary handles before the message drops,
            // so a payload that arrived from another process outlives it.
            self.retained_binaries
                .extend(message.binaries().iter().cloned());
            // Clone (refcount++) the envelope's MSO handles, keeping the envelope's **order** so
            // the copy source table the runner rebuilds places each handle on the same slot the
            // removed container term names.
            self.retained_off_heap
                .extend(message.off_heap().iter().cloned());
            // Retain a share handle on each of the envelope's external-thing node identities. A
            // `NodeEntry` handle shares the interior without registering a reference, so the
            // envelope's own count is left untouched.
            self.retained_nodes
                .extend(message.nodes().iter().map(NodeEntry::handle));
        }
    }

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
    /// A cursor past the end (an empty queue) removes nothing but still repositions. A
    /// removed message that owns off-heap objects — heap [`Fragments`], refc-binary or MSO
    /// handles, external-thing node identities — hands them to this state so the payload stays
    /// valid ([`MailboxState::remove_at`]).
    pub(crate) fn remove(&mut self, queue: &mut Queue) {
        self.remove_at(queue, self.cursor);
        self.cursor = queue.cursor();
    }

    /// `timeout`: reposition the scan at the first message.
    pub(crate) fn reset(&mut self, queue: &Queue) {
        self.cursor = queue.cursor();
    }

    /// `recv_marker_reserve`: an opaque marker term anchored at the **end of the queue**.
    ///
    /// The anchor is the queue's current length — the marker sits immediately after the
    /// last message queued right now, exactly like the C queue node
    /// `recv_marker_insert` splices in at `sig_qs.last` — so everything enqueued later is
    /// scanned by a receive that starts with [`MailboxState::use_reference`], and
    /// messages already queued (which cannot match the reference the caller is about to
    /// create) are skipped.
    ///
    /// An **empty** queue still hands out a usable marker, with anchor `0`: unlike the C
    /// `am_undefined` this survives later structural change (the module docs, "The
    /// marker anchor"). The `undefined` argument is the seam's term for the C's
    /// no-marker answer; it is no longer used, and no caller observes a difference
    /// because a marker term is opaque to the engine.
    ///
    /// At most [`MAX_MARKERS`] markers are kept; reserving past that recycles the oldest
    /// (the C's fixed block does the same).
    pub(crate) fn reserve(&mut self, queue: &Queue, undefined: Eterm) -> Eterm {
        let _ = undefined;
        let marker = make_small(MARKER_BASE + self.next_marker);
        // The term counter is monotone modulo a bound far inside the small-int range, so
        // two live markers can never share a term (at most `MAX_MARKERS` are live).
        self.next_marker = (self.next_marker + 1) % (1 << 40);
        if self.markers.len() == MAX_MARKERS {
            self.markers.remove(0);
        }
        self.markers.push(Marker {
            term: marker,
            reference: None,
            anchor: queue.len(),
        });
        marker
    }

    /// `recv_marker_bind`: bind the opaque `marker` to `reference`.
    ///
    /// A **non-small** marker (any non-marker term a caller may hand back) is a silent
    /// no-op, exactly like the C (`erl_proc_sig_queue.h:2238-2263`).
    pub(crate) fn bind(&mut self, marker: Eterm, reference: Eterm) {
        if !is_small(marker) {
            return;
        }
        if let Some(entry) = self.markers.iter_mut().find(|entry| entry.term == marker) {
            entry.reference = Some(reference);
        }
    }

    /// `recv_marker_use`: set the scan position to `reference`'s marker anchor.
    ///
    /// The anchor is clamped into `0..=len`, so even a queue that shrank *without* the
    /// scan state seeing the removal (a direct `Queue::dequeue`/`clear`, or a host that
    /// rewired the queue) can never park the scan past the end — which would make the
    /// next `loop_rec` report "no candidate" and lose the messages that are still
    /// queued. An unknown reference is a no-op.
    pub(crate) fn use_reference(&mut self, queue: &Queue, reference: Eterm) {
        let anchor = self
            .markers
            .iter()
            .find(|entry| entry.reference == Some(reference))
            .map(|entry| entry.anchor);
        if let Some(anchor) = anchor {
            self.cursor = queue.cursor_at(anchor.min(queue.len()));
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

    /// An **empty-queue** reserve hands out a *usable* marker anchored at the front, so a
    /// receive that suspended at the end of an empty queue re-scans from the front — the
    /// C `am_undefined` shortcut (`erl_proc_sig_queue.h:2225-2236`) is only equivalent
    /// there *at that instant*; an explicit anchor also survives a later structural
    /// change (see the module docs, "The marker anchor").
    ///
    /// The discriminator is behavioural, not merely the term's shape: the scan is
    /// advanced past the (empty) queue and then a message is enqueued, which leaves a
    /// cursor anchored on nothing. The restore must put the scan back onto it. On the
    /// pre-fix code an empty queue recorded no marker at all, `bind` and `use` were both
    /// silent no-ops, and the scan stayed past the end — the message was never seen.
    #[test]
    fn a_marker_reserved_on_an_empty_queue_still_repositions_the_scan() {
        let mut queue = Queue::new();
        let mut state = MailboxState::default();
        let marker = state.reserve(&queue, make_small(-1));
        assert!(is_small(marker), "an empty queue still yields a marker");

        state.bind(marker, make_small(7));
        assert_eq!(state.next(&queue), None, "an empty mailbox has no candidate");
        // A message arrives; the scan runs past it (the cursor ended up past the end).
        queue.enqueue(Message::new(make_small(9), NIL));
        state.skip(&queue);
        assert_eq!(
            state.next(&queue),
            None,
            "the cursor was advanced past the newly queued message"
        );

        state.use_reference(&queue, make_small(7));
        assert_eq!(
            state.next(&queue),
            Some(make_small(9)),
            "the marker re-positioned the scan onto the message that arrived later"
        );
    }

    /// A marker anchors at the **end of the queue**, not at the scan cursor, and it stays
    /// valid across a removal in front of it.
    ///
    /// This is the C's `recv_marker_insert` semantics
    /// (`../../erts/emulator/beam/erl_proc_sig_queue.c:4290-4348`: the marker node is
    /// spliced in at `sig_qs.last`) together with
    /// `erts_msgq_pass_recv_markers`'s back-pointer maintenance: `recv_marker_reserve` is
    /// emitted *before* the `make_ref` the receive matches on
    /// (`beam_ssa_recv.erl:27-106`), so a receive that starts with `recv_marker_use` must
    /// skip the messages that were already queued and scan everything that arrived
    /// afterwards.
    ///
    /// The discriminator: on the pre-fix code the marker recorded the **scan cursor**
    /// (here `0`, because nothing had been scanned yet, while the queue held two
    /// messages), so `use_reference` rewound the scan onto the first message queued
    /// *before* the reference existed and the message the receive was waiting for was
    /// reached only after two useless comparisons — and, worse, a scan that had already
    /// been exhausted kept its past-the-end position instead. Note the pre-fix code
    /// satisfied the *weaker* property this test's second half (`reset`-shaped) asserts,
    /// which is why the defect was silent.
    #[test]
    fn a_marker_anchors_at_the_queue_tail_not_at_the_scan_cursor() {
        let mut queue = queue_of(&[1, 2]);
        let mut state = MailboxState::default();
        let marker = state.reserve(&queue, make_small(-1));
        state.bind(marker, make_small(42));
        // Messages queued *after* the reserve: these are the ones the receive must see.
        queue.enqueue(Message::new(make_small(3), NIL));
        queue.enqueue(Message::new(make_small(4), NIL));

        state.use_reference(&queue, make_small(42));
        assert_eq!(
            state.next(&queue),
            Some(make_small(3)),
            "the marker skips the two messages queued before it and lands on the first \
             message queued after it"
        );

        // ... and it is length-stable: unlinking a message *in front of* the anchor moves
        // the anchor down with it (the C rewrites the marker node's `prev_next`), so the
        // restore still names the same queue point and never lands past the end.
        let mut queue = queue_of(&[1, 2]);
        let mut state = MailboxState::default();
        let marker = state.reserve(&queue, make_small(-1));
        state.bind(marker, make_small(42));
        queue.enqueue(Message::new(make_small(3), NIL));
        // Unlink both messages that were queued before the marker.
        let head = queue.cursor();
        state.remove_at(&mut queue, head);
        state.remove_at(&mut queue, head);
        assert_eq!(payloads(&queue), vec![3], "only the post-reserve message is left");
        state.use_reference(&queue, make_small(42));
        assert_eq!(
            state.next(&queue),
            Some(make_small(3)),
            "the anchor followed the removals instead of pointing past the end"
        );
    }

    /// A restore **clamps** the anchor into `0..=len`, so a queue that shrank behind the
    /// scan state's back cannot leave the scan stranded past the end for ever.
    ///
    /// [`MailboxState::remove_at`] maintains every anchor, so the clamp only matters for a
    /// queue emptied or shortened **without** the scan state seeing it (a direct
    /// [`Queue::dequeue`], [`Queue::clear`], or a host that rewired the queue). The failure
    /// it prevents is not the restore itself — a cursor past the end peeks `None` either
    /// way — but everything that happens *afterwards*: an unclamped `anchor > len` stays
    /// past the end as the queue refills, so `loop_rec` answers "no candidate" and the
    /// receiver misses every message queued after the restore.
    #[test]
    fn a_restored_anchor_is_clamped_into_the_queue() {
        let mut queue = queue_of(&[1, 2, 3]);
        let mut state = MailboxState::default();
        // The marker sits *after* all three messages, so its anchor is `3`.
        let marker = state.reserve(&queue, make_small(-1));
        state.bind(marker, make_small(8));
        assert_eq!(state.next(&queue), Some(make_small(1)));

        // The queue shrinks behind the scan state's back down to one message.
        assert!(queue.dequeue().is_some());
        assert!(queue.dequeue().is_some());
        state.use_reference(&queue, make_small(8));
        assert_eq!(
            state.next(&queue),
            None,
            "the one message left was queued before the marker, so it is skipped"
        );

        // Two messages arrive afterwards: a clamped scan still finds them.
        queue.enqueue(Message::new(make_small(4), NIL));
        queue.enqueue(Message::new(make_small(5), NIL));
        assert_eq!(
            state.next(&queue),
            Some(make_small(4)),
            "an unclamped anchor (3) would still be past the end of a 3-message queue"
        );
        state.skip(&queue);
        assert_eq!(state.next(&queue), Some(make_small(5)));

        // A fully drained queue leaves a sane scan position too.
        queue.clear();
        state.use_reference(&queue, make_small(8));
        assert_eq!(state.next(&queue), None);
        queue.enqueue(Message::new(make_small(6), NIL));
        assert_eq!(
            state.next(&queue),
            Some(make_small(6)),
            "a message queued after a restore over a drained queue is still scanned"
        );
    }

    /// The marker table is bounded (the C's `ERTS_RECV_MARKER_BLOCK_SIZE` recycling) and
    /// keeps handing out usable, distinct markers.
    ///
    /// `beam_ssa_recv` deliberately emits no `recv_marker_clear` on exception edges
    /// (`lib/compiler/src/beam_ssa_recv.erl:225-232`), so an unbounded table would leak
    /// one entry per raise out of a marker'd receive for the life of the process. An
    /// evicted marker is simply unknown, which `bind`/`use` treat as a no-op.
    #[test]
    fn the_marker_table_is_bounded_and_recycles_the_oldest() {
        let queue = queue_of(&[1]);
        let mut state = MailboxState::default();
        let first = state.reserve(&queue, make_small(-1));
        state.bind(first, make_small(1));
        for _ in 1..MAX_MARKERS {
            let marker = state.reserve(&queue, make_small(-1));
            state.bind(marker, make_small(-2));
        }
        assert_eq!(state.markers.len(), MAX_MARKERS);
        let newest = state.reserve(&queue, make_small(-1));
        assert_eq!(
            state.markers.len(),
            MAX_MARKERS,
            "reserving past the bound recycles instead of growing"
        );
        assert!(
            state.markers.iter().all(|marker| marker.term != first),
            "the oldest marker was the one recycled"
        );
        // The recycled marker's reference is simply unknown now: using it is a no-op,
        // never a wrong scan position, and the newest marker still works.
        state.bind(newest, make_small(99));
        state.reset(&queue);
        state.use_reference(&queue, make_small(1));
        assert_eq!(
            state.next(&queue),
            Some(make_small(1)),
            "an evicted marker leaves the scan alone"
        );
        let queue = queue_of(&[1, 2]);
        state.use_reference(&queue, make_small(99));
        assert_eq!(
            state.next(&queue),
            Some(make_small(2)),
            "the newest marker anchors at the tail of the queue it was reserved on"
        );
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

    /// The scan keeps only **off-heap-owning** payloads alive: an ordinary message whose
    /// payload lives in the receiver's own heap is dropped exactly as before, so the retention
    /// path adds nothing to the common case.
    #[test]
    fn removing_a_heap_resident_message_retains_nothing() {
        let mut queue = queue_of(&[1, 2, 3]);
        let mut state = MailboxState::default();
        state.skip(&queue);
        state.remove(&mut queue);
        assert_eq!(payloads(&queue), vec![1, 3]);
        assert_eq!(state.retained(), 0, "a heap-resident payload needs no keeping");
        assert!(state.retained_binaries().is_empty());
        assert!(state.retained_off_heap().is_empty());
        assert!(state.retained_nodes().is_empty());
    }

    /// **The scan retains the removed message's MSO handles.**
    ///
    /// A payload carrying an off-heap MSO reference resolves its object through the copy's source
    /// off-heap table by the slot the container term records, so the envelope's handles must
    /// outlive the message exactly like a refc binary's. The discriminator is the MSO's reference
    /// count: an `observer` clone plus the message's own handle make `2`, and `remove_at` must
    /// **clone** the envelope's handle onto the scan state for the count to still be `2` after the
    /// message drops. On the pre-fix code the message's handle is the last one, dropped with the
    /// message, so the count falls to `1` and the first assertion below fails.
    #[test]
    fn removing_a_message_retains_its_off_heap_handles() {
        let handle = beam_gc::Mso::new(0u32, 16);
        let observer = handle.clone();
        assert_eq!(observer.refcount(), 2, "the observer plus the message's handle");

        let mut queue = Queue::new();
        queue.enqueue(Message::new(make_small(1), NIL).with_off_heap(vec![handle]));
        let mut state = MailboxState::default();
        let cursor = queue.cursor();
        state.remove_at(&mut queue, cursor);

        assert_eq!(
            observer.refcount(),
            2,
            "the scan retained a clone of the MSO handle"
        );
        assert_eq!(state.retained_off_heap().len(), 1);
        assert_eq!(
            state.retained_off_heap()[0].refcount(),
            2,
            "the retained handle names the message's own object"
        );
    }

    /// **The scan retains the removed message's external-thing node identities**, symmetric with
    /// the refc-binary and MSO families.
    ///
    /// The retained handle names the same `(node, creation)` identity the removed message held. On
    /// the pre-fix code this test cannot even be *expressed* — there is no `retained_nodes`
    /// accessor — so its discriminator is the accessor's existence rather than a runtime count: a
    /// [`NodeEntry`] handle shares its interior **without** registering a reference, so dropping
    /// the message's entry moves no observable count.
    #[test]
    fn removing_a_message_retains_its_node_entries() {
        use beam_term::prelude::*;

        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));
        let entry = NodeEntry::new(node, 3);
        let observer = entry.handle();

        let mut queue = Queue::new();
        queue.enqueue(Message::new(make_small(1), NIL).with_nodes(vec![entry]));
        let mut state = MailboxState::default();
        let cursor = queue.cursor();
        state.remove_at(&mut queue, cursor);

        assert_eq!(
            state.retained_nodes().len(),
            1,
            "the scan retained the removed message's node entry"
        );
        let retained = &state.retained_nodes()[0];
        assert!(
            retained.identity_eq(node, 3),
            "the retained entry names the same node identity"
        );
        assert!(
            retained.same_node(&observer),
            "the retained entry is the same node the message held"
        );
    }

    /// **The runtime's transport-copy source builder surfaces the retained off-heap handles.**
    ///
    /// The `remove_at` retention above is only observable to a transport copy if the builder the
    /// runner calls ([`Runtime::source_payloads`]) carries it. This drives the real data flow: the
    /// MSO-carrying message goes into a **live runtime's** pid mailbox, is removed through the same
    /// `remove_at`, its scan state is installed on the runtime, and the builder must then carry the
    /// handle. On the pre-fix code the retained list is empty, so the builder's `off_heap` is empty
    /// and the first assertion fails.
    ///
    /// A full `stabilize_term_with` resolution of a term carrying an actual off-heap container is
    /// **not** driven here: no safe public API can synthesize the bare container term
    /// (`beam_message::layout::make_off_heap_container` is `pub(crate)`, and
    /// `beam_term::refc::write_bin_ref` is `unsafe`), `beam_rt` is `#![deny(unsafe_code)]`, and no
    /// production path builds one — so this data-flow assertion is the honest route.
    #[test]
    fn the_copy_source_builder_includes_the_retained_off_heap_handles() {
        use crate::runtime::Runtime;

        let mut rt = Runtime::with_defaults().expect("the default runtime");
        let pid = rt.self_pid();
        let handle = beam_gc::Mso::new(0u32, 16);
        let observer = handle.clone();

        // Queue the carrying message in the pid's own mailbox...
        {
            let process = rt.processes.get_mut(pid).expect("the runtime's own process");
            let mailbox = process.mailbox_mut();
            mailbox.enqueue(Message::new(make_small(1), NIL).with_off_heap(vec![handle]));
        }
        // ...and remove it through the scan state the receive seam drives.
        let mut state = MailboxState::default();
        {
            let process = rt.processes.get_mut(pid).expect("the runtime's own process");
            let mailbox = process.mailbox_mut();
            let cursor = mailbox.cursor();
            state.remove_at(mailbox, cursor);
        }
        assert_eq!(observer.refcount(), 2, "the removal retained the handle");
        rt.mailboxes.insert(pid, state);

        // The builder the runner calls must now carry the retained handle.
        let payloads = rt.source_payloads(pid, &[]);
        assert_eq!(
            payloads.off_heap.len(),
            1,
            "the copy-source builder surfaces the retained MSO handle"
        );
        assert!(
            payloads.binaries.is_empty(),
            "no refc binary was involved"
        );
        assert_eq!(
            observer.refcount(),
            3,
            "the builder owns its own clone of the handle"
        );
    }

    /// The runtime's per-pid **mailbox scan state** map — `Runtime::mailboxes`, the map
    /// [`RuntimeContext::mailbox_parts`](crate::context::RuntimeContext) resolves every receive
    /// against — is a `ProcessId`-keyed map over the crate's own deterministic
    /// [`crate::hasher`], not `RandomState`'s SipHash (the same substitution the GC descent latch
    /// pins in `runtime::gc::tests`).
    ///
    /// The soundness argument is what is pinned here, never a clock: the map is only ever
    /// `get`/`entry`/`insert`ed by the pid of a process the table already holds (never iterated for
    /// an observable order), so a cheaper hasher may change the probe cost and the (unobserved)
    /// order — and nothing else. The `assert_fast_hashed` pin stops compiling if the map ever
    /// reverts to `RandomState`.
    #[test]
    fn the_mailbox_scan_state_map_answers_like_a_default_hasher_map() {
        use std::collections::HashMap;

        use beam_process::ProcessId;

        use crate::hasher::FastHashMap;
        use crate::runtime::Runtime;

        /// The type pin: the field really is the crate's fast-hashed map.
        fn assert_fast_hashed(map: &FastHashMap<ProcessId, MailboxState>) -> usize {
            map.len()
        }

        let mut rt = Runtime::with_defaults().expect("the default runtime");
        let own = rt.self_pid();
        // A mixed corpus: the running pid, one differing from it in the **serial** alone (so the
        // whole pid, not just the table slot, is the key) and two boundary-shaped pids.
        let corpus = [
            own,
            ProcessId::new(own.number(), own.serial().wrapping_add(1)),
            ProcessId::new(0, 0),
            ProcessId::new(u64::MAX, u64::MAX),
        ];
        // A mixed pattern: every other pid's scan state is created, so both verdicts are exercised.
        let mut reference: HashMap<ProcessId, ()> = HashMap::new();
        for (index, pid) in corpus.iter().enumerate() {
            if index % 2 == 0 {
                rt.mailboxes.entry(*pid).or_default();
                reference.insert(*pid, ());
            }
        }
        assert_eq!(
            assert_fast_hashed(&rt.mailboxes),
            reference.len(),
            "the map holds exactly the states that were created"
        );
        for pid in &corpus {
            assert_eq!(
                rt.mailboxes.contains_key(pid),
                reference.contains_key(pid),
                "the membership verdict agrees with a default-hasher map for {pid:?}"
            );
        }
        // The scan state is the *process's*: the running pid has one, an unknown pid has none.
        assert!(
            rt.mailboxes.contains_key(&own),
            "the running pid's scan state exists"
        );
        assert!(
            !rt.mailboxes.contains_key(&ProcessId::new(7, 7)),
            "a pid nobody created a scan state for has none"
        );
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
        use beam_gc::HeapPolicy;
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
        // A **policy-bounded** receiver, because the fragment fallback is reached only when
        // the receiver's own nursery genuinely refuses the payload. The runtime's default
        // policy is unbounded (`+hmax 0`: `max_heap_words == 0`), and `Process::deliver`
        // retries a refused copy after growing the nursery **on demand** — so an unbounded
        // receiver simply absorbs the ~8000-word payload into its young heap and never takes
        // the fragment path. Capping the nursery at 1024 words (comfortably above
        // `recv:wait/0`'s live set, far below the payload) makes that growth impossible, so
        // the retry reproduces the raw `AllocKind::Message` refusal the send path keys its
        // fragment fallback on.
        let policy = HeapPolicy::from_min_max(0, 1024);
        let receiver = rt
            .spawn_process_with_policy("recv", "wait", vec![], policy, None)
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
