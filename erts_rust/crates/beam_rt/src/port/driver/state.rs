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

//! The per-port driver-private state a [`DriverPort`](super::DriverPort) host owns that the frozen
//! [`beam_abi::DriverHost`] seam has no term/home for: the driver's **private byte queue** — the C's
//! `prt->ioq` (`erts/emulator/beam/io.c:6790-6919`, `erl_io_queue.c`).
//!
//! It is per port and reached only through the `driver_enq`/`driver_pushq`/`driver_deq`/
//! `driver_sizeq`/`driver_peekq` host functions, which the host itself answers, so it lives here
//! rather than in the (already oversized) sibling `driver.rs`.
//!
//! # The private queue is not the Erlang→driver output queue
//!
//! The C keeps two distinct queues per port: the *Erlang→driver* output queue `prt->queue` (what
//! `port_command` appends and the driver's `output`/`outputv` callback consumes — `beam_io` owns
//! that one) and the driver's **own** `prt->ioq`, which only `driver_enq`/`driver_pushq`/… touch
//! (`beam_io`'s own host documents the same split, `beam_io/src/abi/host.rs`). This module models
//! the second one: an ordered list of **chunks**, one per successful enqueue, so a `driver_peekq`
//! iovec entry maps to one `driver_enq` call (`io.c:6790-6919`) and a partial `driver_deq` never
//! moves an earlier chunk's bytes.

use std::os::raw::c_void;

use libc::c_int;

use beam_abi::SysIOVec;

/// The C's driver-private port queue (`prt->ioq`) as a list of enqueue chunks.
///
/// Every method answers the C's own code: the `driver_*` wrappers in `beam_abi` translate between
/// those codes and the [`beam_abi::DriverHost`] hook contract (a hook's `-1` becomes the C's
/// `NOT_A_SIZE` sentinel, `queues.rs`).
#[derive(Debug, Default)]
pub struct DriverQueue {
    /// The queued bytes, in queue order, one entry per successful enqueue (`driver_enqv` explains
    /// the C's own chunking: one iovec entry per enqueue, `io.c:6827-6834`).
    chunks: Vec<Vec<u8>>,
}

impl DriverQueue {
    /// An empty queue — a **fresh** port's `driver_sizeq` is `0` (`io.c:6827-6834`).
    pub fn new() -> DriverQueue {
        DriverQueue::default()
    }

    /// `driver_enq` / `driver_enq_bin`: append `data` at the queue's **tail**.
    ///
    /// The C's success code is `0`; an empty `data` enqueues nothing and is still `0` (the wrapper
    /// answers `0` before reaching this hook for a zero length, `queues.rs::driver_enq`).
    pub fn enq(&mut self, data: &[u8]) -> c_int {
        if data.is_empty() {
            return 0;
        }
        self.chunks.push(data.to_vec());
        0
    }

    /// `driver_pushq` / `driver_pushq_bin`: put `data` at the queue's **head** (the C's
    /// `driver_pushq*` is the `enq_front` counterpart).
    pub fn pushq(&mut self, data: &[u8]) -> c_int {
        if data.is_empty() {
            return 0;
        }
        self.chunks.insert(0, data.to_vec());
        0
    }

    /// The queue's byte count (`driver_sizeq`) — never negative for a live queue.
    pub fn sizeq(&self) -> c_int {
        let total: usize = self.chunks.iter().map(Vec::len).sum();
        c_int::try_from(total).unwrap_or(c_int::MAX)
    }

    /// `driver_deq`: drop `len` bytes from the **head**, answering the number dropped, or `-1` (the
    /// C's failure code) when `len` exceeds the queue's size. The C's `driver_deq` then reports the
    /// *remaining* size (`io.c:6880-6890`), which the wrapper re-reads through
    /// [`DriverQueue::sizeq`].
    pub fn deq(&mut self, len: c_int) -> c_int {
        if len < 0 {
            return -1;
        }
        let wanted = len as usize;
        if wanted > self.chunks.iter().map(Vec::len).sum::<usize>() {
            return -1;
        }
        let mut left = wanted;
        while left > 0 {
            let first = self.chunks[0].len();
            if first <= left {
                left -= first;
                self.chunks.remove(0);
            } else {
                self.chunks[0].drain(..left);
                left = 0;
            }
        }
        len
    }

    /// `driver_peekq`: one iovec entry per queued chunk, or `None` for an **empty** queue (the C's
    /// `NULL` with `*vlenp = 0`, `io.c:6861-6880`).
    ///
    /// Each `iov_base` points at that chunk's own heap allocation, which never moves while the chunk
    /// is queued (a `Vec<u8>`'s buffer is stable across the `DriverQueue`'s own growth), so
    /// `beam_abi`'s published iovec view really describes the queue's bytes.
    pub fn peekq(&self) -> Option<Vec<SysIOVec>> {
        if self.chunks.is_empty() {
            return None;
        }
        Some(
            self.chunks
                .iter()
                .map(|chunk| SysIOVec {
                    iov_base: chunk.as_ptr() as *mut c_void,
                    iov_len: chunk.len(),
                })
                .collect(),
        )
    }

    /// Drops everything queued: the port (and so this queue) is going away.
    pub fn clear(&mut self) {
        self.chunks.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh queue is empty: the oracle's `driver_sizeq` on a **fresh** driver port is `0`.
    #[test]
    fn a_fresh_queue_is_empty() {
        let queue = DriverQueue::new();
        assert_eq!(queue.sizeq(), 0);
        assert!(queue.peekq().is_none());
    }

    /// `driver_enq` appends at the tail, `driver_pushq` at the head, and both answer the C's `0`.
    #[test]
    fn enq_appends_and_pushq_prepends() {
        let mut queue = DriverQueue::new();
        assert_eq!(queue.enq(b"tail"), 0);
        assert_eq!(queue.pushq(b"head"), 0);
        assert_eq!(queue.sizeq(), 8);
        assert_eq!(queue.enq(b""), 0, "an empty enqueue is a no-op, still 0");
        assert_eq!(queue.sizeq(), 8);
        let view = queue.peekq().expect("a non-empty queue peeks");
        assert_eq!(view.len(), 2, "one iovec entry per enqueue");
        assert_eq!(view[0].iov_len, 4);
        assert_eq!(view[1].iov_len, 4);
    }

    /// `driver_deq` drops from the head and answers the number of bytes dropped; a request larger
    /// than the queue is the C's failure code and leaves the queue untouched.
    #[test]
    fn deq_drops_from_the_head_or_refuses() {
        let mut queue = DriverQueue::new();
        queue.enq(b"abcd");
        queue.enq(b"ef");
        assert_eq!(queue.deq(3), 3);
        assert_eq!(queue.sizeq(), 3);
        // The remainder is the tail of the first chunk plus the whole second one.
        let view = queue.peekq().expect("a non-empty queue peeks");
        assert_eq!(view[0].iov_len, 1);
        assert_eq!(view[1].iov_len, 2);
        assert_eq!(queue.deq(4), -1, "more than the queue holds");
        assert_eq!(queue.sizeq(), 3, "a refused deq leaves the queue untouched");
        assert_eq!(queue.deq(3), 3);
        assert_eq!(queue.sizeq(), 0);
        assert!(queue.peekq().is_none());
        queue.enq(b"x");
        queue.clear();
        assert_eq!(queue.sizeq(), 0);
    }
}
