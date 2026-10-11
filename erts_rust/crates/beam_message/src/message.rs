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

//! The message envelope ([`Message`]) and the per-process FIFO message queue ([`Queue`]).
//!
//! `Message` mirrors the legacy C `ErtsMessage` fields `m[0]`/`m[1]`/`m[2]`
//! (`ERL_MESSAGE_TERM`/`_TOKEN`/`_FROM`) plus the off-heap objects the payload owns: the MSO
//! handles of its off-heap references, the refc binaries whose payloads it shares, and the
//! external-thing node references it holds. `Queue` is a **pure data structure**: waiting, the
//! save queue, signal ordering and priority handling are `beam_process`'s, not this crate's.

use std::collections::VecDeque;

use beam_alloc::RefcBinary;
use beam_term::prelude::*;

use crate::fragment::Fragments;
use crate::node::NodeEntry;
use crate::off_heap::MsoHandle;

/// A copied message: the payload term, the sender identity and the off-heap objects it owns.
///
/// A payload that fits the receiver's heap lives *in that heap* and the message holds no
/// fragments. A payload that does not fit instead lives in one or more [`Fragments`] (the
/// `ErlHeapFragment` chain), and [`payload`](Message::payload) points into them; the fragments own
/// the payload's off-heap objects for its lifetime, so `off_heap`/`binaries`/`nodes` are then empty.
pub struct Message {
    payload: Eterm,
    from: Eterm,
    token: Eterm,
    off_heap: Vec<MsoHandle>,
    binaries: Vec<RefcBinary>,
    nodes: Vec<NodeEntry>,
    fragments: Option<Fragments>,
}

impl Message {
    /// A new message carrying `payload` from `from`, with no token, no off-heap objects and no
    /// fragments.
    pub fn new(payload: Eterm, from: Eterm) -> Message {
        Message {
            payload,
            from,
            token: NIL,
            off_heap: Vec::new(),
            binaries: Vec::new(),
            nodes: Vec::new(),
            fragments: None,
        }
    }

    /// Sets the message's sequence/distribution token.
    pub fn with_token(mut self, token: Eterm) -> Message {
        self.token = token;
        self
    }

    /// Re-targets the message's sender identity, keeping **everything else** — `payload`, `token`,
    /// `off_heap`, `binaries`, `nodes` and `fragments`.
    ///
    /// Builder parity with [`with_token`](Message::with_token), for a message whose sender is not
    /// yet known at [`Message::new`] time. To write a *relocated* `from` back into an envelope that
    /// already exists, use the in-place [`set_from`](Message::set_from) instead.
    ///
    /// `from` is a term. A host that stores a heap word here must gather it as a GC root and scatter
    /// it back with [`set_from`](Message::set_from) on every collection of the owning process — see
    /// that method's documentation.
    pub fn with_from(mut self, from: Eterm) -> Message {
        self.from = from;
        self
    }

    /// Sets the off-heap objects the payload owns (they were refcounted during the copy).
    pub fn with_off_heap(mut self, off_heap: Vec<MsoHandle>) -> Message {
        self.off_heap = off_heap;
        self
    }

    /// Sets the refc binaries whose off-heap payloads the message shares (they were refcounted
    /// during the copy).
    pub fn with_binaries(mut self, binaries: Vec<RefcBinary>) -> Message {
        self.binaries = binaries;
        self
    }

    /// Sets the external-thing node references the payload holds (they were refcounted during the
    /// copy).
    pub fn with_nodes(mut self, nodes: Vec<NodeEntry>) -> Message {
        self.nodes = nodes;
        self
    }

    /// Sets the heap fragments holding a payload that did not fit the receiver's heap.
    pub fn with_fragments(mut self, fragments: Fragments) -> Message {
        self.fragments = Some(fragments);
        self
    }

    /// Replaces the message's payload term, keeping **everything else** — `from`, `token`,
    /// `off_heap`, `binaries`, `nodes` and `fragments`.
    ///
    /// This is the **relocation-safe rebuild**: a collector (or any other host step) that must move
    /// a *queued* message's payload to a new address uses this instead of `Message::new(...)` +
    /// re-applying the builders, so the fragment chain and the off-heap / refc-binary / node lists
    /// the message owns are carried across. Rebuilding from scratch silently **drops** those lists,
    /// which for a fragment-resident payload is a use-after-free (the new payload would point into
    /// fragments nobody owns any more) and for a heap-resident one leaks the refcounts the message
    /// held (see the sibling `beam_process` handoff in the crate `CONTEXT.md`).
    ///
    /// No validation is performed: the caller asserts `payload` is a term that belongs with this
    /// envelope (in particular, for a fragmented message, a term pointing into the **same**
    /// [`Fragments`], since the fragments are not re-derived from the new payload).
    pub fn with_payload(mut self, payload: Eterm) -> Message {
        self.payload = payload;
        self
    }

    /// The copied payload term.
    pub fn payload(&self) -> Eterm {
        self.payload
    }

    /// The sender identity.
    pub fn from(&self) -> Eterm {
        self.from
    }

    /// Writes a **relocated** sender identity back into the envelope — the mutating seam a host's
    /// rooted-collection walk calls after [`from`](Message::from) has been gathered as a GC root.
    ///
    /// `from` is a **term**. A host that stores a heap word here (an on-heap `{Name,node}` pair, an
    /// external pid, an Info tuple — the rare cases the legacy C also allows,
    /// `erl_proc_sig_queue.c:2272`) must, for every collection of the owning process:
    ///
    /// 1. push [`from`](Message::from) into that collection's root set (alongside the message's
    ///    payload), then
    /// 2. write the relocated word back through this setter.
    ///
    /// A heap word stored here with **no** such gather survives as a **stale pointer into the
    /// evacuated from-space** once the owning process collects. The common case — the immediate
    /// sender pid / node atom / sysname the C asserts (`is_immed(ERL_MESSAGE_FROM(msg))`,
    /// `erl_proc_sig_queue.c:8441`) — needs no gather and can be written at any time.
    pub fn set_from(&mut self, from: Eterm) {
        self.from = from;
    }

    /// The sequence/distribution token ([`NIL`] when absent).
    pub fn token(&self) -> Eterm {
        self.token
    }

    /// Writes a **relocated** sequence/distribution token back into the envelope — the mutating seam
    /// a host's rooted-collection walk calls after [`token`](Message::token) has been gathered as a
    /// GC root.
    ///
    /// `token` is a **term** and, unlike `from`, the legacy C *expects* it to be a heap term on the
    /// distribution path (its DBG assert is `token == am_undefined || token == NIL ||
    /// is_tuple(token)`, `erl_message.c:388-390`, and it is copied with `copy_struct`). A host that
    /// stores such a heap tuple here must, for every collection of the owning process:
    ///
    /// 1. push [`token`](Message::token) into that collection's root set, then
    /// 2. write the relocated word back through this setter.
    ///
    /// A heap word stored here with **no** such gather survives as a **stale pointer into the
    /// evacuated from-space** once the owning process collects. [`NIL`] (the absent-token case) is
    /// an immediate and needs no gather.
    pub fn set_token(&mut self, token: Eterm) {
        self.token = token;
    }

    /// The off-heap objects the payload owns.
    pub fn off_heap(&self) -> &[MsoHandle] {
        &self.off_heap
    }

    /// The refc binaries whose off-heap payloads the payload owns.
    pub fn binaries(&self) -> &[RefcBinary] {
        &self.binaries
    }

    /// The external-thing node references the payload holds.
    pub fn nodes(&self) -> &[NodeEntry] {
        &self.nodes
    }

    /// The heap fragments holding the payload, or `None` when the payload lives in the receiver's
    /// heap.
    pub fn fragments(&self) -> Option<&Fragments> {
        self.fragments.as_ref()
    }

    /// The heap fragments, mutably — for the receiver to [`move_into`](Fragments::move_into) its
    /// own heap.
    pub fn fragments_mut(&mut self) -> Option<&mut Fragments> {
        self.fragments.as_mut()
    }

    /// The **logical word size** the payload occupies — the copy-charge input, the Rust analogue of
    /// the legacy C `size_object_litopt(message)` (`bif.c`), which the sender charge divides by
    /// `ERTS_MSG_COPY_WORDS_PER_REDUCTION` (64):
    ///
    /// * **Heap-resident message** (no fragments): the payload lives in the receiver's heap, so the
    ///   size is the payload graph's flat word count, [`beam_term::size_object`] of the payload.
    /// * **Fragmented message**: the payload points *into* the message's [`Fragments`] (it never
    ///   touches the receiver's heap), so the size is the chain's used words,
    ///   [`Fragments::total_used_words`] — the summed `used_size` of every fragment, i.e. exactly
    ///   the words the payload occupies there.
    ///
    /// Either way this counts **heap words**, so an off-heap refc binary contributes only its 8-word
    /// on-heap `[BinRef][ErlSubBits]` handle and a huge binary yields (almost) no charge — the
    /// payload's *logical* byte size is accounted separately, to the holder, via
    /// [`logical_binary_bytes`](Message::logical_binary_bytes).
    ///
    /// Note this is the *flat* count: [`beam_term::size_object`] is the C `size_object_x` port and,
    /// like the C, is unbounded on a cyclic/forged graph (see the crate `CONTEXT.md`, Known Issues).
    pub fn heap_words(&self) -> usize {
        match &self.fragments {
            Some(fragments) => fragments.total_used_words(),
            None => beam_term::size_object(self.payload),
        }
    }

    /// The **logical byte size** of the refc binaries this message shares — the input to the
    /// *holder's* virtual binary heap (`bin_vheap += refc_binary->orig_size / sizeof(Eterm)`, the C
    /// `erl_gc.c` rule surfaced to the user as `virtual_reds`).
    ///
    /// It is the sum of [`RefcBinary::len`] over [`binaries`](Message::binaries), the binaries whose
    /// off-heap payloads the message holds. **0** when the message shares no refc binaries.
    ///
    /// Independent of [`heap_words`](Message::heap_words): the payload's on-heap cost counts only
    /// the 8-word handles, so a 200-byte binary contributes `8` heap words here and `200` logical
    /// bytes there. A **fragmented** message reports `0` from this accessor: on that path the
    /// payload's binaries live in the fragment's own off-heap table and are owned by the
    /// [`Fragments`], not by the envelope's `binaries` list.
    pub fn logical_binary_bytes(&self) -> usize {
        self.binaries.iter().map(RefcBinary::len).sum()
    }
}

/// A positional **scan cursor** over a [`Queue`].
///
/// The engine drives a receive **candidate by candidate** (peek the message at the scan position,
/// advance the position on a failed match, remove the matched message, reset to the first message).
/// A cursor is a **monotonic position**: it starts at the first message ([`Default`] = position `0`)
/// and only ever moves forward ([`Queue::advance`]).
///
/// A cursor is deliberately **not** a stable handle. [`Queue::remove_at`] shifts the positions of
/// every subsequent message down by one, so a cursor that is outstanding across a removal is
/// **stale** (it now points one past the message it pointed at). Callers re-derive a fresh cursor
/// with [`Queue::cursor`] after the queue changes structurally — which is exactly what the engine
/// seam does: remove the matched message, then reset to the first message.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueCursor {
    position: usize,
}

/// A per-process FIFO message queue.
#[derive(Default)]
pub struct Queue {
    messages: VecDeque<Message>,
}

impl Queue {
    /// An empty queue.
    pub fn new() -> Queue {
        Queue {
            messages: VecDeque::new(),
        }
    }

    /// Appends `message` at the tail (the queue is a FIFO).
    ///
    /// Under `debug_assertions` this validates the envelope's two host-relocatable words: `from` and
    /// `token` must be **immediates** (see `debug_assert_enqueue_terms`), so a host that enqueues an
    /// un-relocated young-heap word fails loudly here instead of leaving it dangling after the owning
    /// process's next collection. The check is compiled out under `--release`.
    pub fn enqueue(&mut self, message: Message) {
        debug_assert_enqueue_terms(message.from(), message.token());
        self.messages.push_back(message);
    }

    /// Removes and returns the head message.
    pub fn dequeue(&mut self) -> Option<Message> {
        self.messages.pop_front()
    }

    /// The head message, without removing it.
    pub fn peek(&self) -> Option<&Message> {
        self.messages.front()
    }

    /// The number of queued messages.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Whether the queue holds no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Drops every queued message.
    pub fn clear(&mut self) {
        self.messages.clear();
    }

    /// Iterates the queued messages in FIFO order.
    pub fn iter(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter()
    }

    /// A fresh cursor at the first message (position `0`).
    ///
    /// On an empty queue that position is already past the end.
    pub fn cursor(&self) -> QueueCursor {
        QueueCursor::default()
    }

    /// A fresh cursor at `position`.
    pub fn cursor_at(&self, position: usize) -> QueueCursor {
        QueueCursor { position }
    }

    /// The cursor's position (index) in the queue.
    ///
    /// Never fails: a cursor past the end simply reports its (past-the-end) position.
    pub fn position(&self, cursor: QueueCursor) -> usize {
        cursor.position
    }

    /// The message at `cursor`, or `None` if the cursor is past the end (which is always the case
    /// for a cursor over an empty queue).
    ///
    /// Never fails and never panics — the access is bounds-safe.
    pub fn message_at(&self, cursor: QueueCursor) -> Option<&Message> {
        self.messages.get(cursor.position)
    }

    /// Moves `cursor` past the current message.
    ///
    /// Saturating: once the cursor is at or beyond the last message it stays past the end (its
    /// position does not grow beyond the queue length). Never panics and never overflows.
    pub fn advance(&self, cursor: &mut QueueCursor) {
        if cursor.position < self.messages.len() {
            cursor.position += 1;
        }
    }

    /// Removes and returns the message at `cursor`, or `None` if the cursor is past the end.
    ///
    /// **Every message after the removed one shifts down by one**, so a cursor at or after the
    /// removal position is stale afterwards (see [`QueueCursor`]); re-derive one with
    /// [`Queue::cursor`] before scanning again. Never fails and never panics.
    pub fn remove_at(&mut self, cursor: QueueCursor) -> Option<Message> {
        self.messages.remove(cursor.position)
    }

    /// Removes and returns the message at `index` (used by selective receive).
    pub(crate) fn remove_index(&mut self, index: usize) -> Option<Message> {
        self.messages.remove(index)
    }
}

/// Debug-only validation at the enqueue boundary of the two **term-carrying** envelope words a host
/// must relocate — `from` and `token`.
///
/// Both must be genuine values (a bare header word such as `THE_NON_VALUE` is not a term) and must be
/// **immediates**: the host (`beam_process::Process::run_rooted_collection`) gathers only each queued
/// message's payload as a GC root, so a non-immediate word left in `from`/`token` would not be
/// relocated and would dangle into the evacuated from-space after the owning process collects.
///
/// Legacy-C nuance: the C also *permits* a heap `token` tuple (`token == am_undefined || token == NIL
/// || is_tuple(token)`, `erl_message.c:388-390`) and a heap Info-tuple `from`
/// (`erl_proc_sig_queue.c:2272`), so this is a deliberate narrowing, not a C invariant. A future host
/// opting into a heap `from`/`token` must (a) add a root-gather for these two words to its collection
/// walk — pushing them from [`Message::from`]/[`Message::token`] and scattering them back with
/// [`Message::set_from`]/[`Message::set_token`] — and (b) deliberately relax this tripwire.
///
/// Cheap and always compiled out under `--release` (`debug_assert!`).
fn debug_assert_enqueue_terms(from: Eterm, token: Eterm) {
    debug_assert!(
        is_value(from),
        "Message::from is a bare non-value header word (THE_NON_VALUE), not a term — the envelope \
         stores a genuine term word here"
    );
    debug_assert!(
        is_immediate(from),
        "Message::from is not an immediate: a heap `from` must be gathered as a GC root by the \
         owning process's collection walk and written back with Message::set_from before the \
         envelope is enqueued, otherwise the word dangles into the evacuated from-space"
    );
    debug_assert!(
        is_value(token),
        "Message::token is a bare non-value header word (THE_NON_VALUE), not a term — the envelope \
         stores a genuine term word here"
    );
    debug_assert!(
        is_immediate(token),
        "Message::token is not an immediate: a heap `token` must be gathered as a GC root by the \
         owning process's collection walk and written back with Message::set_token before the \
         envelope is enqueued, otherwise the word dangles into the evacuated from-space"
    );
}

#[cfg(test)]
mod tests {
    use beam_term::prelude::*;

    use super::{Message, Queue, QueueCursor};
    fn message(value: i64) -> Message {
        Message::new(make_small(value), make_small(0))
    }

    #[test]
    fn message_builder_and_accessors() {
        let payload = make_small(42);
        let from = make_small(7);
        let message = Message::new(payload, from);
        assert_eq!(message.payload(), payload);
        assert_eq!(message.from(), from);
        assert_eq!(message.token(), NIL);
        assert!(message.off_heap().is_empty());
        assert!(message.binaries().is_empty());
        assert!(message.nodes().is_empty());
        assert!(message.fragments().is_none(), "a heap-resident payload has no fragments");

        let message = message.with_token(make_small(1));
        assert_eq!(message.token(), make_small(1));
        assert_eq!(message.payload(), payload);
    }

    #[test]
    fn a_fragmented_message_reports_its_fragments() {
        use crate::fragment::{Fragments, HeapFragment};

        let mut fragments = Fragments::new();
        fragments.push(HeapFragment::new(8).expect("the fragment allocates"));
        let mut message = Message::new(make_small(1), make_small(0)).with_fragments(fragments);

        let reported = message.fragments().expect("the fragments travel with the message");
        assert_eq!(reported.len(), 1);
        assert!(message.fragments_mut().is_some());
    }

    #[test]
    fn the_binary_and_node_lists_travel_with_the_message() {
        use crate::node::NodeEntry;
        use beam_alloc::RefcBinary;

        let mut atoms = AtomTable::new();
        let node = make_atom(atoms.intern("nonode@nohost"));

        let message = Message::new(make_small(1), make_small(0))
            .with_binaries(vec![RefcBinary::new(&[9, 9, 9]).expect("the payload allocates")])
            .with_nodes(vec![NodeEntry::new(node, 2)]);
        assert_eq!(message.binaries().len(), 1);
        assert_eq!(message.binaries()[0].as_bytes(), &[9, 9, 9]);
        assert_eq!(message.nodes().len(), 1);
        assert_eq!(message.nodes()[0].node().raw(), node.raw());
        assert_eq!(message.nodes()[0].creation(), 2);
    }

    #[test]
    fn queue_is_fifo() {
        let mut queue = Queue::new();
        assert!(queue.is_empty());
        assert_eq!(queue.len(), 0);
        assert!(queue.peek().is_none());
        assert!(queue.dequeue().is_none());

        queue.enqueue(message(1));
        queue.enqueue(message(2));
        queue.enqueue(message(3));
        assert_eq!(queue.len(), 3);
        assert_eq!(queue.peek().map(|m| m.payload()), Some(make_small(1)));
        assert_eq!(
            queue.iter().map(|m| signed_value(m.payload())).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        assert_eq!(queue.dequeue().map(|m| signed_value(m.payload())), Some(1));
        assert_eq!(queue.remove_index(1).map(|m| signed_value(m.payload())), Some(3));
        assert_eq!(queue.len(), 1);
        assert!(queue.remove_index(5).is_none());

        queue.clear();
        assert!(queue.is_empty());
    }

    #[test]
    fn default_is_the_empty_queue() {
        let queue = Queue::default();
        assert!(queue.is_empty());
    }

    #[test]
    fn cursor_starts_at_the_first_message() {
        let mut queue = Queue::new();
        for value in [1, 2, 3] {
            queue.enqueue(message(value));
        }

        let cursor = queue.cursor();
        assert_eq!(queue.position(cursor), 0);
        assert_eq!(queue.position(QueueCursor::default()), 0);
        assert_eq!(
            queue.message_at(cursor).map(|m| signed_value(m.payload())),
            Some(1)
        );
    }

    #[test]
    fn cursor_over_an_empty_queue_is_past_the_end() {
        let mut queue = Queue::new();

        let cursor = queue.cursor();
        assert_eq!(queue.position(cursor), 0);
        assert!(queue.message_at(cursor).is_none());
        assert!(queue.remove_at(cursor).is_none());
        assert!(queue.is_empty());
    }

    #[test]
    fn advance_walks_fifo_order_and_saturates_past_the_end() {
        let mut queue = Queue::new();
        for value in [10, 20, 30] {
            queue.enqueue(message(value));
        }

        let mut cursor = queue.cursor();
        let mut scanned = Vec::new();
        for _ in 0..queue.len() {
            scanned.push(signed_value(queue.message_at(cursor).expect("in range").payload()));
            queue.advance(&mut cursor);
        }
        assert_eq!(scanned, vec![10, 20, 30], "messages are scanned in FIFO order");
        assert_eq!(queue.position(cursor), 3, "one past the last message");
        assert!(queue.message_at(cursor).is_none());

        // Saturating: advancing further stays past the end instead of growing.
        queue.advance(&mut cursor);
        queue.advance(&mut cursor);
        assert_eq!(queue.position(cursor), 3);
        assert!(queue.message_at(cursor).is_none());
    }

    #[test]
    fn cursor_at_positions_correctly() {
        let mut queue = Queue::new();
        for value in [1, 2, 3] {
            queue.enqueue(message(value));
        }

        assert_eq!(queue.position(queue.cursor_at(0)), 0);
        let cursor = queue.cursor_at(1);
        assert_eq!(queue.position(cursor), 1);
        assert_eq!(
            queue.message_at(cursor).map(|m| signed_value(m.payload())),
            Some(2)
        );
        assert_eq!(
            queue.message_at(queue.cursor_at(2)).map(|m| signed_value(m.payload())),
            Some(3)
        );
        assert!(queue.message_at(queue.cursor_at(3)).is_none());
        assert!(queue.message_at(queue.cursor_at(99)).is_none());
    }

    #[test]
    fn remove_at_removes_and_shifts_the_rest() {
        let mut queue = Queue::new();
        for value in [1, 2, 3, 4] {
            queue.enqueue(message(value));
        }

        let removed = queue.remove_at(queue.cursor_at(1));
        assert_eq!(removed.map(|m| signed_value(m.payload())), Some(2));
        assert_eq!(
            queue.iter().map(|m| signed_value(m.payload())).collect::<Vec<_>>(),
            vec![1, 3, 4],
            "the messages after the removed one shift down"
        );
        assert_eq!(queue.len(), 3);

        // A past-the-end cursor removes nothing and leaves the queue unchanged.
        assert!(queue.remove_at(queue.cursor_at(9)).is_none());
        assert_eq!(queue.len(), 3);
        assert_eq!(
            queue.iter().map(|m| signed_value(m.payload())).collect::<Vec<_>>(),
            vec![1, 3, 4]
        );

        // The head is removable through a fresh cursor.
        let head = queue.cursor();
        assert_eq!(
            queue.remove_at(head).map(|m| signed_value(m.payload())),
            Some(1)
        );
        assert_eq!(
            queue.iter().map(|m| signed_value(m.payload())).collect::<Vec<_>>(),
            vec![3, 4]
        );
    }

    #[test]
    fn draining_with_remove_at_empties_the_queue() {
        let mut queue = Queue::new();
        for value in [1, 2, 3, 4] {
            queue.enqueue(message(value));
        }

        let mut drained = Vec::new();
        while !queue.is_empty() {
            // Re-derive a fresh cursor for every removal — the engine's reset-to-first step.
            let cursor = queue.cursor();
            let removed = queue.remove_at(cursor).expect("a message while non-empty");
            drained.push(signed_value(removed.payload()));
        }
        assert_eq!(drained, vec![1, 2, 3, 4]);
        assert!(queue.is_empty());
        assert!(queue.remove_at(queue.cursor()).is_none());
    }
}
