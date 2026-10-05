//! The data-level send/receive primitives built on the copy engine.
//!
//! [`send`] is the "copy into the receiver's heap, then enqueue" half of the legacy C
//! `erts_queue_message`/`copy_struct` path (the scheduler-signalling half is `beam_process`);
//! [`receive`] is the selective-receive skeleton over a [`Queue`] (waiting, `after` timeouts,
//! the save queue and signal ordering are `beam_process`'s).

use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::*;

use crate::copy::{copy_struct, CopyContext};
use crate::message::{Message, Queue};

/// Copies `term` into the receiver's heap `to_heap`, collects the off-heap objects it
/// references, builds the [`Message`] and enqueues it on `to_queue`.
///
/// `ctx`'s **destination** off-heap table *is* the message's off-heap list: `copy_struct`
/// refcounts every referenced off-heap object into it, and `send` drains it into the message (see
/// the crate `CONTEXT.md`, Design Decisions). It is expected to start empty for each send,
/// mirroring the C per-message `off_heap`; a non-empty table would attach its pre-existing handles
/// to this message.
pub fn send(
    from: Eterm,
    term: Eterm,
    to_heap: &mut Heap,
    to_queue: &mut Queue,
    ctx: &mut CopyContext<'_>,
) -> Result<()> {
    let payload = copy_struct(term, to_heap, ctx)?;
    let off_heap = ctx.dst_off_heap_mut().drain_handles();
    to_queue.enqueue(Message::new(payload, from).with_off_heap(off_heap));
    Ok(())
}

/// Removes and returns the first queued message whose payload satisfies `pred`.
pub fn receive<F: FnMut(&Message) -> bool>(queue: &mut Queue, mut pred: F) -> Option<Message> {
    let index = queue.iter().position(|message| pred(message))?;
    queue.remove_at(index)
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
