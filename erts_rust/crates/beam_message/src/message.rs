//! The message envelope ([`Message`]) and the per-process FIFO message queue ([`Queue`]).
//!
//! `Message` mirrors the legacy C `ErtsMessage` fields `m[0]`/`m[1]`/`m[2]`
//! (`ERL_MESSAGE_TERM`/`_TOKEN`/`_FROM`) plus the off-heap objects the payload owns. `Queue` is a
//! **pure data structure**: waiting, the save queue, signal ordering and priority handling are
//! `beam_process`'s, not this crate's.

use std::collections::VecDeque;

use beam_term::prelude::*;

use crate::off_heap::MsoHandle;

/// A copied message: the payload term, the sender identity and the off-heap objects it owns.
pub struct Message {
    payload: Eterm,
    from: Eterm,
    token: Eterm,
    off_heap: Vec<MsoHandle>,
}

impl Message {
    /// A new message carrying `payload` from `from`, with no token and no off-heap objects.
    pub fn new(payload: Eterm, from: Eterm) -> Message {
        Message {
            payload,
            from,
            token: NIL,
            off_heap: Vec::new(),
        }
    }

    /// Sets the message's sequence/distribution token.
    pub fn with_token(mut self, token: Eterm) -> Message {
        self.token = token;
        self
    }

    /// Sets the off-heap objects the payload owns (they were refcounted during the copy).
    pub fn with_off_heap(mut self, off_heap: Vec<MsoHandle>) -> Message {
        self.off_heap = off_heap;
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

    /// The sequence/distribution token ([`NIL`] when absent).
    pub fn token(&self) -> Eterm {
        self.token
    }

    /// The off-heap objects the payload owns.
    pub fn off_heap(&self) -> &[MsoHandle] {
        &self.off_heap
    }
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
    pub fn enqueue(&mut self, message: Message) {
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

    /// Removes and returns the message at `index` (used by selective receive).
    pub(crate) fn remove_at(&mut self, index: usize) -> Option<Message> {
        self.messages.remove(index)
    }
}

#[cfg(test)]
mod tests {
    use beam_term::prelude::*;

    use super::{Message, Queue};
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

        let message = message.with_token(make_small(1));
        assert_eq!(message.token(), make_small(1));
        assert_eq!(message.payload(), payload);
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
        assert_eq!(queue.remove_at(1).map(|m| signed_value(m.payload())), Some(3));
        assert_eq!(queue.len(), 1);
        assert!(queue.remove_at(5).is_none());

        queue.clear();
        assert!(queue.is_empty());
    }

    #[test]
    fn default_is_the_empty_queue() {
        let queue = Queue::default();
        assert!(queue.is_empty());
    }
}
