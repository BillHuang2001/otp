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

//! The data-level send/receive primitives built on the copy engine.
//!
//! [`send`] is the "copy into the receiver's heap, then enqueue" half of the legacy C
//! `erts_queue_message`/`copy_struct` path (the scheduler-signalling half is `beam_process`);
//! [`send_fragmented`] is the same for a payload that does **not** fit the receiver's heap — it
//! copies the payload into a [`HeapFragment`] instead (the C `erts_try_alloc_message_on_heap`
//! sender); [`receive`] is the selective-receive skeleton over a [`Queue`] (waiting, `after`
//! timeouts, the save queue and signal ordering are `beam_process`'s).
//!
//! # The host-facing contract (what a runtime relies on)
//!
//! A host (such as `beam_rt`) drives the oversized-message retry through this module; the contract
//! is:
//!
//! * **`send` is atomic.** A rejected `send` performs **no partial write**: the copier preflights
//!   the destination capacity against the whole graph's flat word count *before* writing a word, so
//!   the failure leaves the receiver heap, the destination off-heap table and the queue exactly as
//!   they were.
//! * **The oversized-message retry is signalled by exactly
//!   [`BeamError::Alloc`]`(`[`AllocKind::Message`]`)`.** Any other failure is not a "too big for the
//!   heap" signal. [`send_fragmented`] encodes that rule: it tries [`send`] first and falls back to
//!   a fragment **only** on that one error kind, propagating every other failure unchanged (the
//!   receiver heap, and the destination table since the failed attempt wrote nothing, stay untouched).
//! * **One fragment holds the whole payload.** [`send_fragmented`] sizes its fragment to at least
//!   `size_object(term)` (honouring a larger `fragment_capacity_words` hint) and never splits a
//!   payload across a chain; a size-bounded split is not done here, so a caller that wants one builds
//!   the [`Fragments`] chain itself. The fragment's backing heap never reallocates, so the enqueued
//!   [`Message`]'s [`payload`](Message::payload) pointer, which points *into* the fragment, stays
//!   valid for the message's lifetime.
//! * **The fragment owns the payload's off-heap objects** until the receiver's
//!   [`Fragments::move_into`] hands them to the receiver's table; the enqueued message's own
//!   `off_heap`/`binaries`/`nodes` lists are empty on the fragment path.
//! * **[`Fragments::move_into`] is one-shot and net-refcount-preserving**: it copies each root onto
//!   the receiver's heap (fresh off-heap slots) and releases the fragment's own references only
//!   after the whole chain has moved (see its docs).

use beam_alloc::Heap;
use beam_common::{AllocKind, BeamError, Result};
use beam_term::prelude::*;

use crate::copy::{copy_struct, CopyContext};
use crate::fragment::{Fragments, HeapFragment};
use crate::message::{Message, Queue};

/// Copies `term` into the receiver's heap `to_heap`, collects the off-heap objects it
/// references, builds the [`Message`] and enqueues it on `to_queue`.
///
/// `ctx`'s **destination** off-heap table *is* the message's off-heap ownership: `copy_struct`
/// refcounts every referenced off-heap object into it (MSO handles), refcounts every shared refc
/// binary into its binary list and registers every external-thing node, and `send` drains all
/// three into the message (see the crate `CONTEXT.md`, Design Decisions). The destination table is
/// expected to start empty for each send, mirroring the C per-message `off_heap`; a non-empty
/// table would attach its pre-existing objects to this message.
///
/// A payload that does **not** fit `to_heap` fails with
/// [`BeamError::Alloc(AllocKind::Message)`](beam_common::BeamError::Alloc) and performs **no
/// partial write**: the copier preflights capacity before writing a word, so `to_heap`, the
/// destination table and `to_queue` are left exactly as they were. Use [`send_fragmented`] to fall
/// back to a heap fragment.
pub fn send(
    from: Eterm,
    term: Eterm,
    to_heap: &mut Heap,
    to_queue: &mut Queue,
    ctx: &mut CopyContext<'_>,
) -> Result<()> {
    let payload = copy_struct(term, to_heap, ctx)?;
    enqueue_copied(from, payload, ctx, to_queue);
    Ok(())
}

/// Like [`send`], but a payload that does not fit the receiver's heap is copied into a
/// [`HeapFragment`] instead of failing — the sender half of the C
/// `erts_try_alloc_message_on_heap`/`new_message_buffer` path (`erl_message.c:622-691`, `68-77`).
///
/// It first tries the ordinary receiver-heap copy; **only** a
/// [`BeamError::Alloc(AllocKind::Message)`](beam_common::BeamError::Alloc) triggers the fragment
/// path (any other failure is propagated unchanged, and `to_heap` is left untouched). The fragment
/// is **one** fragment sized to hold the whole payload (at least `fragment_capacity_words` words, at
/// least `size_object(term)`), and a size-bounded split across a chain is not done here: that is the
/// caller's concern. `term` is copied into the fragment's heap with a fresh [`CopyContext`]
/// whose source is `ctx`'s off-heap table and whose destination is the **fragment's** off-heap
/// table, so the fragment owns the payload's off-heap objects. The enqueued [`Message`]'s
/// [`payload`](Message::payload) points into the fragment and it owns the [`Fragments`].
///
/// `ctx`'s own destination off-heap table is **not** used on the fragment path (its objects would
/// belong to the fragment, not the receiver's heap), mirroring how the C fragment carries its own
/// `off_heap`.
pub fn send_fragmented(
    from: Eterm,
    term: Eterm,
    to_heap: &mut Heap,
    to_queue: &mut Queue,
    ctx: &mut CopyContext<'_>,
    fragment_capacity_words: usize,
) -> Result<()> {
    match copy_struct(term, to_heap, ctx) {
        Ok(payload) => {
            enqueue_copied(from, payload, ctx, to_queue);
            Ok(())
        }
        Err(BeamError::Alloc(failure)) if failure.kind == AllocKind::Message => {
            enqueue_fragmented(from, term, to_queue, ctx.src_off_heap(), fragment_capacity_words)
        }
        Err(other) => Err(other),
    }
}

/// Copies `term` into a fresh [`HeapFragment`] (sized to hold the whole payload) and enqueues a
/// message that points into it and owns it.
fn enqueue_fragmented(
    from: Eterm,
    term: Eterm,
    to_queue: &mut Queue,
    src_off_heap: &crate::off_heap::OffHeapTable,
    fragment_capacity_words: usize,
) -> Result<()> {
    // At least the whole payload, but honour a larger caller hint.
    let capacity = fragment_capacity_words.max(beam_term::size_object(term));
    let mut fragment = HeapFragment::new(capacity)?;
    let root = fragment.copy_root(term, src_off_heap)?;
    let mut fragments = Fragments::new();
    fragments.push(fragment);
    to_queue.enqueue(Message::new(root, from).with_fragments(fragments));
    Ok(())
}

/// Drains the copy's destination off-heap table into a [`Message`] and enqueues it.
///
/// Shared by [`send`] and the heap-resident branch of [`send_fragmented`]: the destination table
/// built up by `copy_struct` *is* the message's off-heap ownership (see the crate `CONTEXT.md`,
/// Design Decisions).
fn enqueue_copied(from: Eterm, payload: Eterm, ctx: &mut CopyContext<'_>, to_queue: &mut Queue) {
    let binaries = ctx.dst_off_heap_mut().drain_binaries();
    let nodes = ctx.dst_off_heap_mut().drain_nodes();
    let off_heap = ctx.dst_off_heap_mut().drain_handles();
    to_queue.enqueue(
        Message::new(payload, from)
            .with_off_heap(off_heap)
            .with_binaries(binaries)
            .with_nodes(nodes),
    );
}

/// Removes and returns the first queued message whose payload satisfies `pred`.
pub fn receive<F: FnMut(&Message) -> bool>(queue: &mut Queue, mut pred: F) -> Option<Message> {
    let index = queue.iter().position(|message| pred(message))?;
    queue.remove_index(index)
}

/// Borrows the first queued message whose payload satisfies `pred`, without removing it.
pub fn receive_ref<F: FnMut(&Message) -> bool>(queue: &Queue, mut pred: F) -> Option<&Message> {
    queue.iter().find(|message| pred(message))
}

#[cfg(test)]
mod tests {
    use beam_term::prelude::*;

    use super::{receive, receive_ref, Message, Queue};
    fn message(value: i64) -> Message {
        Message::new(make_small(value), make_small(0))
    }

    #[test]
    fn receive_removes_only_the_first_match() {
        let mut queue = Queue::new();
        for value in [1, 2, 3] {
            queue.enqueue(message(value));
        }

        let got = receive(&mut queue, |m| signed_value(m.payload()) == 2);
        assert_eq!(got.map(|m| signed_value(m.payload())), Some(2));
        assert_eq!(queue.len(), 2);
        assert_eq!(
            queue.iter().map(|m| signed_value(m.payload())).collect::<Vec<_>>(),
            vec![1, 3]
        );

        // No match leaves the queue untouched.
        assert!(receive(&mut queue, |m| signed_value(m.payload()) == 9).is_none());
        assert_eq!(queue.len(), 2);
    }

    #[test]
    fn receive_ref_borrows_without_removing() {
        let mut queue = Queue::new();
        queue.enqueue(message(1));
        queue.enqueue(message(2));

        let found = receive_ref(&queue, |m| signed_value(m.payload()) == 2);
        assert_eq!(found.map(|m| signed_value(m.payload())), Some(2));
        assert_eq!(queue.len(), 2);

        assert!(receive_ref(&queue, |m| signed_value(m.payload()) == 9).is_none());
    }
}
