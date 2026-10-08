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

//! The port's byte buffers: [`ByteQueue`].
//!
//! Each [`crate::port::Port`] owns two of these — an **input** queue (bytes a driver read
//! and is handing up to Erlang) and an **output** queue (bytes Erlang wrote and a driver
//! must drain to the device). The queue keeps a read cursor so `front()` is always a
//! single contiguous slice, avoiding a byte copy on every `push`. It is the Rust analogue
//! of the C `ioq`/`erl_io_queue` (`erts/emulator/beam/erl_io_queue.{c,h}`).

/// A FIFO byte buffer with a read cursor.
///
/// Bytes are appended with [`ByteQueue::push`] and removed from the front with
/// [`ByteQueue::consume`] (drop without copying) or [`ByteQueue::read_into`] (copy into a
/// caller buffer). [`ByteQueue::front`] returns the unconsumed bytes as one contiguous
/// slice.
pub struct ByteQueue {
    /// The backing storage: `buf[head..]` are the unconsumed bytes.
    buf: Vec<u8>,
    /// The index of the first unconsumed byte.
    head: usize,
}

impl ByteQueue {
    /// An empty queue.
    pub fn new() -> ByteQueue {
        ByteQueue {
            buf: Vec::new(),
            head: 0,
        }
    }

    /// An empty queue with room for at least `n` bytes before reallocating.
    pub fn with_capacity(n: usize) -> ByteQueue {
        ByteQueue {
            buf: Vec::with_capacity(n),
            head: 0,
        }
    }

    /// The number of unconsumed bytes.
    pub fn len(&self) -> usize {
        self.buf.len() - self.head
    }

    /// Whether the queue holds no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Appends `data` to the back of the queue.
    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        // Reclaim the consumed prefix once it dominates the buffer, so repeated
        // push/consume cycles stay O(1) amortized instead of growing `buf` forever.
        if self.head > 0 && self.head > self.buf.len() / 2 {
            self.compact();
        }
        self.buf.extend_from_slice(data);
    }

    /// The unconsumed bytes, as one contiguous slice.
    pub fn front(&self) -> &[u8] {
        &self.buf[self.head..]
    }

    /// Consumes up to `n` bytes from the front, returning how many were dropped.
    ///
    /// The bytes are dropped without being copied out.
    pub fn consume(&mut self, n: usize) -> usize {
        let dropped = n.min(self.len());
        self.head += dropped;
        if self.head == self.buf.len() {
            // Fully drained: reset so the capacity is retained but `head` never
            // outruns `buf`.
            self.buf.clear();
            self.head = 0;
        }
        dropped
    }

    /// Copies up to `out.len()` bytes out of the front, returning how many were copied.
    pub fn read_into(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len());
        if n == 0 {
            return 0;
        }
        out[..n].copy_from_slice(&self.front()[..n]);
        self.consume(n);
        n
    }

    /// Drops every buffered byte.
    pub fn clear(&mut self) {
        self.buf.clear();
        self.head = 0;
    }

    /// Moves the unconsumed bytes to the start of the buffer, reclaiming the consumed
    /// prefix. `front()` stays contiguous as always.
    fn compact(&mut self) {
        if self.head == 0 {
            return;
        }
        if self.head >= self.buf.len() {
            self.buf.clear();
        } else {
            self.buf.drain(..self.head);
        }
        self.head = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::ByteQueue;

    #[test]
    fn new_queue_is_empty() {
        let mut queue = ByteQueue::new();
        assert_eq!(queue.len(), 0);
        assert!(queue.is_empty());
        assert_eq!(queue.front(), b"");
        assert_eq!(queue.consume(4), 0);
        assert_eq!(queue.read_into(&mut [0u8; 4]), 0);
    }

    #[test]
    fn with_capacity_holds_no_bytes() {
        let queue = ByteQueue::with_capacity(64);
        assert!(queue.is_empty());
        assert_eq!(queue.front(), b"");
    }

    #[test]
    fn push_front_and_consume() {
        let mut queue = ByteQueue::new();
        queue.push(b"hello");
        queue.push(b" world");
        assert_eq!(queue.len(), 11);
        assert_eq!(queue.front(), b"hello world");

        assert_eq!(queue.consume(6), 6);
        assert_eq!(queue.front(), b"world");
        assert_eq!(queue.len(), 5);

        // Over-consuming is clamped to the buffered length.
        assert_eq!(queue.consume(100), 5);
        assert!(queue.is_empty());
        assert_eq!(queue.front(), b"");
    }

    #[test]
    fn read_into_copies_then_consumes() {
        let mut queue = ByteQueue::new();
        queue.push(b"abcdef");

        let mut out = [0u8; 4];
        assert_eq!(queue.read_into(&mut out), 4);
        assert_eq!(&out, b"abcd");
        assert_eq!(queue.front(), b"ef");

        // A short read leaves the queue intact beyond the copied bytes.
        let mut small = [0u8; 8];
        assert_eq!(queue.read_into(&mut small), 2);
        assert_eq!(&small[..2], b"ef");
        assert!(queue.is_empty());
    }

    #[test]
    fn clear_drops_everything() {
        let mut queue = ByteQueue::new();
        queue.push(b"data");
        queue.consume(2);
        queue.clear();
        assert!(queue.is_empty());
        assert_eq!(queue.front(), b"");

        // Pushing after a clear starts from the front again.
        queue.push(b"xy");
        assert_eq!(queue.front(), b"xy");
    }

    #[test]
    fn repeated_push_consume_compacts_without_corruption() {
        let mut queue = ByteQueue::new();
        let mut expected: Vec<u8> = Vec::new();
        let mut next = 0u16;
        // Interleaved pushes and partial consumes force the compaction path on most
        // rounds; `front()` must keep returning exactly the expected bytes.
        for _ in 0..300u16 {
            let chunk: Vec<u8> = (0..4).map(|i| ((next + i) % 251) as u8).collect();
            next += 4;
            queue.push(&chunk);
            expected.extend_from_slice(&chunk);

            assert_eq!(queue.consume(3), 3);
            expected.drain(..3);
            assert_eq!(queue.front(), expected.as_slice());
        }
        assert_eq!(queue.len(), expected.len());
    }

    #[test]
    fn partial_consume_then_push_keeps_order() {
        let mut queue = ByteQueue::new();
        queue.push(b"0123456789");
        assert_eq!(queue.consume(7), 7);
        // `head` (7) > buf.len()/2 (5), so this push compacts first.
        queue.push(b"abc");
        assert_eq!(queue.front(), b"789abc");
        assert_eq!(queue.len(), 6);
    }

    #[test]
    fn empty_push_is_a_no_op() {
        let mut queue = ByteQueue::new();
        queue.push(b"");
        assert!(queue.is_empty());
        queue.push(b"x");
        queue.push(b"");
        assert_eq!(queue.front(), b"x");
    }
}
