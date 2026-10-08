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

//! The bounds-checked big-endian read cursor: the decode side of the wire seam.
//!
//! Every network-order field in the external term format is read through
//! [`Reader`]; a short read is always reported as `BeamError::BadArg` — never a
//! panic and never a silent truncation. This module is safe Rust.

use beam_common::Result;

use crate::error::bad_arg;

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
        Reader { buf, pos: 0 }
    }

    /// The offset of the next unread byte.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// The number of unread bytes.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the cursor has consumed every byte.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Reads one byte.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u8(&mut self) -> Result<u8> {
        let byte = *self.buf.get(self.pos).ok_or_else(bad_arg)?;
        self.pos += 1;
        Ok(byte)
    }

    /// Reads a big-endian `u16`.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u16be(&mut self) -> Result<u16> {
        let bytes = self.bytes(2)?;
        let arr: [u8; 2] = bytes.try_into().map_err(|_| bad_arg())?;
        Ok(u16::from_be_bytes(arr))
    }

    /// Reads a big-endian `u32`.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u32be(&mut self) -> Result<u32> {
        let bytes = self.bytes(4)?;
        let arr: [u8; 4] = bytes.try_into().map_err(|_| bad_arg())?;
        Ok(u32::from_be_bytes(arr))
    }

    /// Reads a big-endian `u64`.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn u64be(&mut self) -> Result<u64> {
        let bytes = self.bytes(8)?;
        let arr: [u8; 8] = bytes.try_into().map_err(|_| bad_arg())?;
        Ok(u64::from_be_bytes(arr))
    }

    /// Borrows the next `n` bytes and advances the cursor past them.
    ///
    /// A short read yields `BeamError::BadArg`.
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or_else(bad_arg)?;
        // `get` is `None` both for a short read and for an out-of-range `end`,
        // so the cursor is only ever advanced once the slice is known to exist.
        let slice = self.buf.get(self.pos..end).ok_or_else(bad_arg)?;
        self.pos = end;
        Ok(slice)
    }

    /// The unread remainder of the slice, without advancing the cursor.
    pub fn rest(&self) -> &'a [u8] {
        // `pos` never exceeds `buf.len()`, so this is the tail from the cursor.
        self.buf.get(self.pos..).unwrap_or(&[])
    }

    /// The next byte without consuming it.
    ///
    /// Reading at the end of the slice yields `BeamError::BadArg`.
    pub fn peek_u8(&self) -> Result<u8> {
        self.buf.get(self.pos).copied().ok_or_else(bad_arg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beam_common::BeamError;

    /// The absolute boundary values of each supported width, in network order.
    const SAMPLE: [u8; 15] = [
        0x01, // u8
        0x12, 0x34, // u16be
        0xDE, 0xAD, 0xBE, 0xEF, // u32be
        0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, // u64be
    ];

    #[test]
    fn new_starts_at_zero() {
        let reader = Reader::new(&SAMPLE);
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.remaining(), SAMPLE.len());
        assert!(!reader.is_empty());
    }

    #[test]
    fn empty_slice_is_empty() {
        let reader = Reader::new(&[]);
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.remaining(), 0);
        assert!(reader.is_empty());
        assert_eq!(reader.rest(), &[] as &[u8]);
    }

    #[test]
    fn reads_big_endian_widths() {
        let mut reader = Reader::new(&SAMPLE);
        assert_eq!(reader.u8().unwrap(), 0x01);
        assert_eq!(reader.u16be().unwrap(), 0x1234);
        assert_eq!(reader.u32be().unwrap(), 0xDEAD_BEEF);
        assert_eq!(reader.u64be().unwrap(), 0x0123_4567_89AB_CDEF);
        assert!(reader.is_empty());
        assert_eq!(reader.position(), SAMPLE.len());
        assert_eq!(reader.rest(), &[] as &[u8]);
    }

    #[test]
    fn widths_round_trip_extreme_values() {
        let mut buf = Vec::new();
        buf.push(0xFFu8);
        buf.extend_from_slice(&0xFFFFu16.to_be_bytes());
        buf.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        buf.extend_from_slice(&u64::MAX.to_be_bytes());

        let mut reader = Reader::new(&buf);
        assert_eq!(reader.u8().unwrap(), 0xFF);
        assert_eq!(reader.u16be().unwrap(), u16::MAX);
        assert_eq!(reader.u32be().unwrap(), u32::MAX);
        assert_eq!(reader.u64be().unwrap(), u64::MAX);
        assert!(reader.is_empty());
    }

    #[test]
    fn zero_values_are_read_verbatim() {
        let buf = [0u8; 15];
        let mut reader = Reader::new(&buf);
        assert_eq!(reader.u8().unwrap(), 0);
        assert_eq!(reader.u16be().unwrap(), 0);
        assert_eq!(reader.u32be().unwrap(), 0);
        assert_eq!(reader.u64be().unwrap(), 0);
    }

    #[test]
    fn short_read_at_boundary_does_not_advance() {
        // One byte less than each width requires: every read must fail and leave
        // the cursor untouched.
        for width in [2usize, 4, 8] {
            let buf = [0u8; 8];
            let mut reader = Reader::new(&buf[..width - 1]);
            let err = match width {
                2 => reader.u16be().unwrap_err(),
                4 => reader.u32be().unwrap_err(),
                _ => reader.u64be().unwrap_err(),
            };
            assert!(matches!(err, BeamError::BadArg), "width {width}");
            assert_eq!(reader.position(), 0, "width {width}");
            assert_eq!(reader.remaining(), width - 1, "width {width}");
        }
    }

    #[test]
    fn u8_at_end_yields_bad_arg_without_advancing() {
        let mut reader = Reader::new(&[0xAB]);
        assert_eq!(reader.u8().unwrap(), 0xAB);
        let err = reader.u8().unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(reader.position(), 1);
        assert_eq!(reader.remaining(), 0);
        assert!(reader.is_empty());
    }

    #[test]
    fn partial_multi_byte_read_does_not_advance() {
        let mut reader = Reader::new(&[0x00, 0x01, 0x02]);
        // Three bytes available: a u32 read must fail *and* consume nothing.
        let err = reader.u32be().unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.remaining(), 3);
        // The u8 read afterwards must observe the very first byte.
        assert_eq!(reader.u8().unwrap(), 0x00);
    }

    #[test]
    fn bytes_borrows_and_advances() {
        let buf = [1u8, 2, 3, 4, 5];
        let mut reader = Reader::new(&buf);
        assert_eq!(reader.bytes(3).unwrap(), &[1, 2, 3]);
        assert_eq!(reader.position(), 3);
        assert_eq!(reader.remaining(), 2);
        assert_eq!(reader.bytes(0).unwrap(), &[] as &[u8]);
        assert_eq!(reader.position(), 3);
        assert_eq!(reader.bytes(2).unwrap(), &[4, 5]);
        assert!(reader.is_empty());
    }

    #[test]
    fn bytes_past_end_yields_bad_arg_without_advancing() {
        let mut reader = Reader::new(&[1u8, 2, 3]);
        let err = reader.bytes(4).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.remaining(), 3);

        // Exactly the remaining length succeeds.
        assert_eq!(reader.bytes(3).unwrap(), &[1, 2, 3]);
        // One past the end fails again, still without advancing.
        let err = reader.bytes(1).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(reader.position(), 3);
    }

    #[test]
    fn bytes_with_overflowing_length_yields_bad_arg() {
        let mut reader = Reader::new(&[1u8, 2, 3]);
        let err = reader.bytes(usize::MAX).unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(reader.position(), 0);
    }

    #[test]
    fn rest_does_not_advance() {
        let buf = [9u8, 8, 7];
        let mut reader = Reader::new(&buf);
        assert_eq!(reader.rest(), &[9, 8, 7]);
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.u8().unwrap(), 9);
        assert_eq!(reader.rest(), &[8, 7]);
        assert_eq!(reader.position(), 1);
    }

    #[test]
    fn peek_u8_does_not_advance() {
        let mut reader = Reader::new(&[0x2A]);
        assert_eq!(reader.peek_u8().unwrap(), 0x2A);
        assert_eq!(reader.peek_u8().unwrap(), 0x2A);
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.u8().unwrap(), 0x2A);
        assert_eq!(reader.remaining(), 0);
    }

    #[test]
    fn peek_u8_at_end_yields_bad_arg() {
        let reader = Reader::new(&[]);
        let err = reader.peek_u8().unwrap_err();
        assert!(matches!(err, BeamError::BadArg));
        assert_eq!(reader.position(), 0);
    }
}
