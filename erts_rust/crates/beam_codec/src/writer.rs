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

//! The growable big-endian byte sink: the encode side of the wire seam.
//!
//! [`Writer`] is the only output primitive of the codec; every encoder writes
//! through it so byte order and buffer growth live in one place. This module is
//! safe Rust.

use beam_common::Result;

use crate::error;

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
        Writer { buf: Vec::new() }
    }

    /// Creates an empty writer with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Writer {
        Writer {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Appends one byte.
    pub fn u8(&mut self, value: u8) {
        self.buf.push(value);
    }

    /// Appends a big-endian `u16`.
    pub fn u16be(&mut self, value: u16) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Appends a big-endian `u32`.
    pub fn u32be(&mut self, value: u32) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Appends a big-endian `u64`.
    pub fn u64be(&mut self, value: u64) {
        self.buf.extend_from_slice(&value.to_be_bytes());
    }

    /// Appends a byte slice verbatim.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// The number of bytes written.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written yet.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the writer, returning the written bytes.
    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    /// Discards all written bytes, keeping the allocated capacity.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Overwrites the four bytes starting at `pos` with the big-endian encoding of
    /// `value`, leaving every other byte untouched.
    ///
    /// This is the codec's only backward write: a length-prefixed block whose size
    /// is only known once its body has been emitted (the `NEW_FUN_EXT` total-size
    /// field) writes a zero placeholder, emits the body, then patches the size in.
    ///
    /// **Internal invariant:** the caller has already written the four bytes at
    /// `pos` (typically the placeholder), so `pos + 4 <= len`. A `pos` that is out
    /// of range — or `pos + 4` overflowing `usize` — yields `BeamError::BadArg`
    /// rather than a panic or a silent short write.
    pub(crate) fn patch_u32be(&mut self, pos: usize, value: u32) -> Result<()> {
        let end = pos
            .checked_add(core::mem::size_of::<u32>())
            .ok_or_else(error::bad_arg)?;
        let slot = self.buf.get_mut(pos..end).ok_or_else(error::bad_arg)?;
        slot.copy_from_slice(&value.to_be_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use beam_common::BeamError;

    use super::*;

    #[test]
    fn new_is_empty() {
        let writer = Writer::new();
        assert_eq!(writer.len(), 0);
        assert!(writer.is_empty());
        assert_eq!(writer.as_slice(), &[] as &[u8]);
        assert!(writer.into_vec().is_empty());
    }

    #[test]
    fn with_capacity_starts_empty_with_room() {
        let mut writer = Writer::with_capacity(64);
        assert!(writer.is_empty());
        writer.u8(1);
        assert_eq!(writer.as_slice(), &[1]);
    }

    #[test]
    fn each_width_writes_exact_network_order() {
        let mut writer = Writer::new();
        writer.u8(0x01);
        writer.u16be(0x1234);
        writer.u32be(0xDEAD_BEEF);
        writer.u64be(0x0123_4567_89AB_CDEF);
        assert_eq!(
            writer.as_slice(),
            &[
                0x01, // u8
                0x12, 0x34, // u16be
                0xDE, 0xAD, 0xBE, 0xEF, // u32be
                0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, // u64be
            ]
        );
        assert_eq!(writer.len(), 15);
        assert!(!writer.is_empty());
    }

    #[test]
    fn extreme_values_are_big_endian() {
        let mut writer = Writer::new();
        writer.u16be(u16::MAX);
        writer.u32be(u32::MAX);
        writer.u64be(u64::MAX);
        assert_eq!(
            writer.as_slice(),
            &[
                0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
                0xFF,
            ]
        );
    }

    #[test]
    fn zero_values_are_written_verbatim() {
        let mut writer = Writer::new();
        writer.u8(0);
        writer.u16be(0);
        writer.u32be(0);
        writer.u64be(0);
        assert_eq!(writer.as_slice(), &[0u8; 15]);
    }

    #[test]
    fn bytes_appends_verbatim() {
        let mut writer = Writer::new();
        writer.bytes(&[1, 2, 3]);
        writer.bytes(&[]);
        writer.bytes(&[4]);
        assert_eq!(writer.as_slice(), &[1, 2, 3, 4]);
        assert_eq!(writer.len(), 4);
    }

    #[test]
    fn appends_accumulate_and_len_tracks_them() {
        let mut writer = Writer::new();
        for i in 0..8u8 {
            writer.u8(i);
            assert_eq!(writer.len(), usize::from(i) + 1);
        }
        assert_eq!(writer.as_slice(), &[0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn into_vec_returns_written_bytes() {
        let mut writer = Writer::new();
        writer.u16be(0x00FF);
        assert_eq!(writer.into_vec(), vec![0x00, 0xFF]);
    }

    #[test]
    fn clear_resets_len_but_keeps_capacity() {
        let mut writer = Writer::with_capacity(32);
        writer.bytes(&[1, 2, 3, 4]);
        writer.clear();
        assert_eq!(writer.len(), 0);
        assert!(writer.is_empty());
        assert_eq!(writer.as_slice(), &[] as &[u8]);
        // `clear` must not release the allocation.
        assert!(writer.into_vec().capacity() >= 32);
    }

    #[test]
    fn patch_u32be_overwrites_only_the_four_addressed_bytes() {
        let mut writer = Writer::new();
        writer.bytes(&[0xAA, 0xBB, 0, 0, 0, 0, 0xCC]);
        writer.patch_u32be(2, 0xDEAD_BEEF).unwrap();
        assert_eq!(
            writer.as_slice(),
            &[0xAA, 0xBB, 0xDE, 0xAD, 0xBE, 0xEF, 0xCC]
        );

        // The patched region may be the whole buffer, and values are network-order.
        let mut tail = Writer::new();
        tail.u32be(0);
        tail.patch_u32be(0, 1).unwrap();
        assert_eq!(tail.as_slice(), &[0, 0, 0, 1]);
        tail.patch_u32be(0, u32::MAX).unwrap();
        assert_eq!(tail.as_slice(), &[0xFF; 4]);
    }

    #[test]
    fn patch_u32be_rejects_an_out_of_range_position() {
        let mut writer = Writer::new();
        writer.u32be(0);
        // Past the end, the end itself, and a `pos` that cannot hold four bytes.
        assert!(matches!(writer.patch_u32be(1, 1), Err(BeamError::BadArg)));
        assert!(matches!(writer.patch_u32be(4, 1), Err(BeamError::BadArg)));
        assert!(matches!(
            writer.patch_u32be(usize::MAX, 1),
            Err(BeamError::BadArg)
        ));
        // The rejected calls left the buffer untouched.
        assert_eq!(writer.as_slice(), &[0, 0, 0, 0]);
    }
}
