//! The growable big-endian byte sink: the encode side of the wire seam.
//!
//! [`Writer`] is the only output primitive of the codec; every encoder writes
//! through it so byte order and buffer growth live in one place. This module is
//! safe Rust.

/// A growable sink for network-order bytes.
///
/// The writer appends; nothing is ever rewritten, so `as_slice` stays stable
/// across appends only for the bytes already written (callers re-borrow).
pub struct Writer {
    /// The bytes written so far.
    buf: Vec<u8>,
}

impl Writer {
    /// Creates an empty writer.
    pub fn new() -> Writer {
        unimplemented!()
    }

    /// Creates an empty writer with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Writer {
        unimplemented!()
    }

    /// Appends one byte.
    pub fn u8(&mut self, value: u8) {
        unimplemented!()
    }

    /// Appends a big-endian `u16`.
    pub fn u16be(&mut self, value: u16) {
        unimplemented!()
    }

    /// Appends a big-endian `u32`.
    pub fn u32be(&mut self, value: u32) {
        unimplemented!()
    }

    /// Appends a big-endian `u64`.
    pub fn u64be(&mut self, value: u64) {
        unimplemented!()
    }

    /// Appends a byte slice verbatim.
    pub fn bytes(&mut self, bytes: &[u8]) {
        unimplemented!()
    }

    /// The number of bytes written.
    pub fn len(&self) -> usize {
        unimplemented!()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        unimplemented!()
    }

    /// The bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        unimplemented!()
    }

    /// Consumes the writer, returning the written bytes.
    pub fn into_vec(self) -> Vec<u8> {
        unimplemented!()
    }

    /// Discards all written bytes, keeping the allocated capacity.
    pub fn clear(&mut self) {
        unimplemented!()
    }
}
