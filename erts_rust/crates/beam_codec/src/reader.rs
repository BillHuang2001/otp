//! The bounds-checked big-endian read cursor: the decode side of the wire seam.
//!
//! Every network-order field in the external term format is read through
//! [`Reader`]; a short read is always reported as `BeamError::BadArg` — never a
//! panic and never a silent truncation. This module is safe Rust.

use beam_common::Result;

/// A forward-only cursor over a byte slice with bounds-checked reads.
///
/// The reader never panics on malformed input: reading past the end of the slice
/// returns `BeamError::BadArg` and leaves the cursor position unchanged.
pub struct Reader<'a> {
    /// The bytes being read.
    buf: &'a [u8],
    /// The offset of the next unread byte.
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Creates a reader positioned at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Reader<'a> {
        unimplemented!()
    }

    /// The offset of the next unread byte.
    pub fn position(&self) -> usize {
        unimplemented!()
    }

    /// The number of unread bytes.
    pub fn remaining(&self) -> usize {
        unimplemented!()
    }

    /// Whether the cursor has consumed every byte.
    pub fn is_empty(&self) -> bool {
        unimplemented!()
    }

    /// Reads one byte.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u8(&mut self) -> Result<u8> {
        unimplemented!()
    }

    /// Reads a big-endian `u16`.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u16be(&mut self) -> Result<u16> {
        unimplemented!()
    }

    /// Reads a big-endian `u32`.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u32be(&mut self) -> Result<u32> {
        unimplemented!()
    }

    /// Reads a big-endian `u64`.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u64be(&mut self) -> Result<u64> {
        unimplemented!()
    }

    /// Borrows the next `n` bytes and advances the cursor past them.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        unimplemented!()
    }

    /// The unread remainder of the slice, without advancing the cursor.
    pub fn rest(&self) -> &'a [u8] {
        unimplemented!()
    }

    /// The next byte without consuming it.
    ///
    /// Reading at the end of the slice yields `BeamError::BadArg`.
    pub fn peek_u8(&self) -> Result<u8> {
        unimplemented!()
    }
}
