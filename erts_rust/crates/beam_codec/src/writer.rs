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
/// The writer appends; the only in-place writes are the backward ones a
/// length-prefixed block needs when its size is known only after its body has
/// been emitted ([`Writer::patch_u16be`], [`Writer::patch_u32be`],
/// [`Writer::mask_last_byte`]) plus the [`Writer::truncate`] undo. `as_slice`
/// therefore stays stable across appends only for the bytes already written
/// (callers re-borrow).
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
    /// Wraps a caller-owned byte buffer, continuing at its end.
    ///
    /// The reusable-buffer constructor: an empty `buf` behaves exactly like
    /// [`Writer::new`], while a non-empty one is appended to (its existing bytes are
    /// never rewritten). Paired with [`Writer::into_vec`]/[`Writer::clear`], this lets
    /// a host keep one allocation across encodes instead of one per call.
    pub fn from_vec(buf: Vec<u8>) -> Writer {
        Writer { buf }
    }
    /// The number of bytes the buffer can hold without reallocating.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }
    /// Ensures room for at least `additional` more bytes, reallocating at most once.
    ///
    /// Equivalent to `Vec::reserve`; the appends already reserve as they grow, so this
    /// exists for a caller that knows the final size up front.
    pub fn reserve(&mut self, additional: usize) {
        self.buf.reserve(additional);
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

    /// Appends the two-byte header `[tag, value]` with **one** capacity check.
    ///
    /// The coalesced form of [`Writer::u8`] twice: a tag byte plus one payload byte
    /// (`SMALL_INTEGER_EXT`, `SMALL_TUPLE_EXT`, `ATOM_CACHE_REF_EXT`, the size-and-
    /// length pair of `SMALL_BIG_EXT`, …) is emitted as a single two-byte append, so
    /// the sink is reserved/checked once instead of once per byte. The bytes written
    /// are exactly `u8(tag); u8(value)`.
    #[inline]
    pub(crate) fn head_u8(&mut self, tag: u8, value: u8) {
        self.buf.extend_from_slice(&[tag, value]);
    }

    /// Appends the three-byte header `[tag, hi, lo]` (big-endian `value`) with
    /// **one** capacity check.
    ///
    /// The coalesced form of [`Writer::u8`] + [`Writer::u16be`]: a tag byte plus a
    /// 16-bit length/count field (`ATOM_UTF8_EXT`, the `STRING_EXT`/`NEWER_REFERENCE_EXT`
    /// element count, …). The bytes written are exactly `u8(tag); u16be(value)`.
    #[inline]
    pub(crate) fn head_u16(&mut self, tag: u8, value: u16) {
        let [hi, lo] = value.to_be_bytes();
        self.buf.extend_from_slice(&[tag, hi, lo]);
    }

    /// Appends the five-byte header `[tag, b3, b2, b1, b0]` (big-endian `value`)
    /// with **one** capacity check.
    ///
    /// The coalesced form of [`Writer::u8`] + [`Writer::u32be`]: a tag byte plus a
    /// 32-bit length/count field (`BINARY_EXT`/`BIT_BINARY_EXT`'s byte count,
    /// `MAP_EXT`'s pair count, `LARGE_TUPLE_EXT`'s arity, `LIST_EXT`'s element count,
    /// …). The bytes written are exactly `u8(tag); u32be(value)`.
    #[inline]
    pub(crate) fn head_u32(&mut self, tag: u8, value: u32) {
        let [b3, b2, b1, b0] = value.to_be_bytes();
        self.buf.extend_from_slice(&[tag, b3, b2, b1, b0]);
    }

    /// Appends a one-byte `tag` followed by `bytes` verbatim, reserving the whole
    /// `1 + bytes.len()` run **once**.
    ///
    /// There is deliberately **no length field** between the two: a wire form that
    /// carries one (`SMALL_ATOM_UTF8_EXT`'s name length, `BINARY_EXT`'s byte count)
    /// writes the tag and its length through [`Writer::head_u8`] /
    /// [`Writer::head_u16`] / [`Writer::head_u32`] and then the payload through
    /// [`Writer::bytes`]. This call is for a tag whose payload *is* everything that
    /// follows — `NEW_FLOAT_EXT`'s eight IEEE bytes, the 31-byte `FLOAT_EXT` field,
    /// `ATOM_UTF8_EXT`'s already-length-prefixed remainder — and, as a plain
    /// one-byte-prefix-plus-run coalescer, for a layout whose first byte merely
    /// prefixes a block (`NEW_FUN_EXT`'s `[arity][uniq]`).
    #[inline]
    pub(crate) fn head_bytes(&mut self, tag: u8, bytes: &[u8]) {
        self.buf.reserve(bytes.len() + 1);
        self.buf.push(tag);
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

    /// Rolls the sink back to `len` bytes, discarding everything written after it
    /// while keeping the allocated capacity.
    ///
    /// This is the counterpart of [`Writer::patch_u32be`] on the *undo* side: the
    /// list encoder optimistically emits the `STRING_EXT` form, and when the spine
    /// turns out not to be a byte list it rewinds the sink to the recorded start
    /// position and re-emits `LIST_EXT` in its place, so no scratch buffer is ever
    /// needed.
    ///
    /// **Internal invariant:** `len` is a position the caller recorded earlier (so
    /// `len <= self.len()`). A forward position would silently *extend* the image
    /// with garbage, so it is rejected — as is a `len` that cannot be a length —
    /// with `BeamError::BadArg` rather than a panic or a corrupt image.
    pub(crate) fn truncate(&mut self, len: usize) -> Result<()> {
        if len > self.buf.len() {
            return Err(error::bad_arg());
        }
        self.buf.truncate(len);
        Ok(())
    }
    /// Applies `mask` to the byte most recently written, leaving every other byte
    /// untouched.
    ///
    /// Like [`Writer::patch_u32be`] this is a backward write, used by the bitstring
    /// encoder to clear the insignificant trailing bits of the last payload byte
    /// (`repack_bits`). A call on an empty writer does nothing rather than
    /// panicking: there is no byte to mask.
    pub(crate) fn mask_last_byte(&mut self, mask: u8) {
        if let Some(last) = self.buf.last_mut() {
            *last &= mask;
        }
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

    /// Overwrites the two bytes starting at `pos` with the big-endian encoding of
    /// `value`, leaving every other byte untouched.
    ///
    /// The 16-bit twin of [`Writer::patch_u32be`], for a length-prefixed block whose
    /// size is only known once its body has been emitted: the `STRING_EXT` element
    /// count (the list encoder writes a zero placeholder, appends the bytes, then
    /// patches the count in).
    ///
    /// **Internal invariant:** the caller has already written the two bytes at
    /// `pos` (typically the placeholder), so `pos + 2 <= len`. A `pos` that is out
    /// of range — or `pos + 2` overflowing `usize` — yields `BeamError::BadArg`
    /// rather than a panic or a silent short write.
    pub(crate) fn patch_u16be(&mut self, pos: usize, value: u16) -> Result<()> {
        let end = pos
            .checked_add(core::mem::size_of::<u16>())
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
    use crate::tag::{ATOM_UTF8_EXT, MAP_EXT, NEW_FLOAT_EXT, NIL_EXT, SMALL_INTEGER_EXT};

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
    fn head_u8_appends_a_tag_and_one_payload_byte_and_reserves_once() {
        // The bytes are exactly `u8(tag); u8(value)`, appended at the end.
        let mut writer = Writer::new();
        writer.bytes(&[0xAA]);
        writer.head_u8(SMALL_INTEGER_EXT, 0x07);
        assert_eq!(writer.as_slice(), &[0xAA, 97, 0x07]);
        assert_eq!(writer.len(), 3);

        // `SMALL_INTEGER_EXT` 255 must not collide with the negative-arm tag: both
        // ends of the byte range are written verbatim.
        let mut edges = Writer::new();
        edges.head_u8(0, 0);
        edges.head_u8(0xFF, 0xFF);
        assert_eq!(edges.as_slice(), &[0, 0, 0xFF, 0xFF]);

        // Capacity: the pair fits in a two-byte reservation, so a writer with exactly
        // two spare bytes does **not** reallocate …
        let mut fitted = Writer::with_capacity(2);
        let cap = fitted.capacity();
        fitted.head_u8(1, 2);
        assert_eq!(fitted.capacity(), cap);
        assert_eq!(fitted.as_slice(), &[1, 2]);
        // … while one spare byte is not enough and the sink grows (never truncating).
        let mut grown = Writer::with_capacity(1);
        let before = grown.capacity();
        grown.head_u8(0xAA, 0xBB);
        assert!(grown.capacity() > before);
        assert_eq!(grown.as_slice(), &[0xAA, 0xBB]);
    }

    #[test]
    fn head_u16_appends_a_tag_and_a_big_endian_u16_and_reserves_once() {
        let mut writer = Writer::new();
        writer.bytes(&[0xAA]);
        writer.head_u16(ATOM_UTF8_EXT, 0x1234);
        assert_eq!(writer.as_slice(), &[0xAA, 118, 0x12, 0x34]);
        assert_eq!(writer.len(), 4);

        // Both extremes stay network-order.
        let mut edges = Writer::new();
        edges.head_u16(1, 0);
        edges.head_u16(2, u16::MAX);
        assert_eq!(edges.as_slice(), &[1, 0, 0, 2, 0xFF, 0xFF]);

        // Capacity: three bytes fit a three-byte reservation, not a two-byte one.
        let mut fitted = Writer::with_capacity(3);
        let cap = fitted.capacity();
        fitted.head_u16(9, 0xBEEF);
        assert_eq!(fitted.capacity(), cap);
        assert_eq!(fitted.as_slice(), &[9, 0xBE, 0xEF]);
        let mut grown = Writer::with_capacity(2);
        let before = grown.capacity();
        grown.head_u16(9, 0xBEEF);
        assert!(grown.capacity() > before);
        assert_eq!(grown.as_slice(), &[9, 0xBE, 0xEF]);
    }

    #[test]
    fn head_u32_appends_a_tag_and_a_big_endian_u32_and_reserves_once() {
        let mut writer = Writer::new();
        writer.bytes(&[0xAA]);
        writer.head_u32(MAP_EXT, 0xDEAD_BEEF);
        assert_eq!(writer.as_slice(), &[0xAA, 116, 0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(writer.len(), 6);

        // Both extremes stay network-order.
        let mut edges = Writer::new();
        edges.head_u32(1, 0);
        edges.head_u32(2, u32::MAX);
        assert_eq!(
            edges.as_slice(),
            &[1, 0, 0, 0, 0, 2, 0xFF, 0xFF, 0xFF, 0xFF]
        );

        // Capacity: five bytes fit a five-byte reservation, not a four-byte one.
        let mut fitted = Writer::with_capacity(5);
        let cap = fitted.capacity();
        fitted.head_u32(116, 32);
        assert_eq!(fitted.capacity(), cap);
        assert_eq!(fitted.as_slice(), &[116, 0, 0, 0, 32]);
        let mut grown = Writer::with_capacity(4);
        let before = grown.capacity();
        grown.head_u32(116, 32);
        assert!(grown.capacity() > before);
        assert_eq!(grown.as_slice(), &[116, 0, 0, 0, 32]);
    }

    #[test]
    fn head_bytes_appends_a_tag_then_the_payload_verbatim_with_one_reservation() {
        // No length field is inserted: the payload follows the tag byte directly.
        let mut writer = Writer::new();
        writer.bytes(&[0xAA]);
        writer.head_bytes(NEW_FLOAT_EXT, &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            writer.as_slice(),
            &[0xAA, 70, 1, 2, 3, 4, 5, 6, 7, 8],
            "the payload must not be length-prefixed"
        );
        assert_eq!(writer.len(), 10);

        // An empty payload writes the tag alone (a bare `SMALL_INTEGER_EXT`-style
        // header is the caller's business, but the call must not panic or grow).
        let mut bare = Writer::new();
        bare.head_bytes(NIL_EXT, &[]);
        assert_eq!(bare.as_slice(), &[106]);

        // Capacity: `1 + len` bytes fit a `1 + len` reservation, not a `len` one.
        let mut fitted = Writer::with_capacity(4);
        let cap = fitted.capacity();
        fitted.head_bytes(119, b"abc");
        assert_eq!(fitted.capacity(), cap);
        assert_eq!(fitted.as_slice(), &[119, b'a', b'b', b'c']);
        let mut grown = Writer::with_capacity(3);
        let before = grown.capacity();
        grown.head_bytes(119, b"abc");
        assert!(grown.capacity() > before);
        assert_eq!(grown.as_slice(), &[119, b'a', b'b', b'c']);
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

    #[test]
    fn patch_u16be_overwrites_only_the_two_addressed_bytes() {
        let mut writer = Writer::new();
        writer.bytes(&[0xAA, 0, 0, 0xCC]);
        writer.patch_u16be(1, 0xBEEF).unwrap();
        assert_eq!(writer.as_slice(), &[0xAA, 0xBE, 0xEF, 0xCC]);

        // The patched region may be the whole buffer, and values are network-order.
        let mut tail = Writer::new();
        tail.u16be(0);
        tail.patch_u16be(0, 1).unwrap();
        assert_eq!(tail.as_slice(), &[0, 1]);
        tail.patch_u16be(0, u16::MAX).unwrap();
        assert_eq!(tail.as_slice(), &[0xFF; 2]);
    }

    #[test]
    fn patch_u16be_rejects_an_out_of_range_position() {
        let mut writer = Writer::new();
        writer.u16be(0);
        // Past the end, the end itself, and a `pos` that cannot hold two bytes.
        assert!(matches!(writer.patch_u16be(1, 1), Err(BeamError::BadArg)));
        assert!(matches!(writer.patch_u16be(2, 1), Err(BeamError::BadArg)));
        assert!(matches!(
            writer.patch_u16be(usize::MAX, 1),
            Err(BeamError::BadArg)
        ));
        // The rejected calls left the buffer untouched.
        assert_eq!(writer.as_slice(), &[0, 0]);
    }

    #[test]
    fn truncate_drops_bytes_after_the_position_and_keeps_the_rest() {
        let mut writer = Writer::with_capacity(64);
        writer.bytes(&[1, 2, 3, 4]);
        writer.truncate(2).unwrap();
        assert_eq!(writer.as_slice(), &[1, 2]);
        assert_eq!(writer.len(), 2);
        // Truncating to the current length, and to zero, are both no-ops/clears.
        writer.truncate(2).unwrap();
        assert_eq!(writer.as_slice(), &[1, 2]);
        writer.truncate(0).unwrap();
        assert!(writer.is_empty());
        // The allocation survives, so a rewind costs no allocation.
        assert!(writer.into_vec().capacity() >= 64);
    }

    #[test]
    fn truncate_rejects_a_forward_position() {
        let mut writer = Writer::new();
        writer.bytes(&[1, 2]);
        // A position past the end would *extend* the image with garbage.
        assert!(matches!(writer.truncate(3), Err(BeamError::BadArg)));
        assert_eq!(writer.as_slice(), &[1, 2]);
    }
}
