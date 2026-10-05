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
        let _ = data;
        todo!("compact the buffer if the consumed prefix is large, then extend with `data`")
    }

    /// The unconsumed bytes, as one contiguous slice.
    pub fn front(&self) -> &[u8] {
        &self.buf[self.head..]
    }

    /// Consumes up to `n` bytes from the front, returning how many were dropped.
    ///
    /// The bytes are dropped without being copied out.
    pub fn consume(&mut self, n: usize) -> usize {
        let _ = n;
        todo!("advance `head` by min(n, len) and return the number of bytes consumed")
    }

    /// Copies up to `out.len()` bytes out of the front, returning how many were copied.
    pub fn read_into(&mut self, out: &mut [u8]) -> usize {
        let _ = out;
        todo!("copy from `front()` into `out`, then consume the copied count")
    }

    /// Drops every buffered byte.
    pub fn clear(&mut self) {
        todo!("reset `buf` and `head` to empty")
    }
}
