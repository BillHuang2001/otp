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

//! A self-contained, safe-Rust raw-DEFLATE (RFC 1951) **decoder**.
//!
//! # Why this exists here
//!
//! `zlib:zip/1`/`zlib:unzip/1` (`../../erts/preloaded/src/zlib.erl:727-753`) are the
//! **raw** members of the `zlib` module: `zip/1` calls
//! `deflateInit(Z, default, deflated, -MAX_WBITS, 8, default)` and `unzip/1` calls
//! `inflateInit(Z, -MAX_WBITS)`, i.e. a 15-bit window with **no** zlib header and
//! **no** Adler-32 trailer. `zlib:zip/1` is *not* gzip (that is `zlib:gzip/1`, an
//! RFC 1952 stream).
//!
//! The workspace's own codec, [`beam_codec::compress`], is RFC 1950**-only** — its
//! `ZlibDecoder` validates the 2-byte header and the Adler-32 trailer — so it cannot
//! consume a raw stream, and this crate may not add an external dependency. The
//! deflating direction needs no new code at all (`zip/1` strips the framing off what
//! `beam_codec::compress::compress` emits), so only the *inflating* direction is
//! implemented here, from the specification.
//!
//! # Scope
//!
//! All three RFC 1951 block types — stored (type 0), fixed Huffman (type 1) and
//! dynamic Huffman (type 2, including the code-length alphabet and the repeat codes
//! 16/17/18) — plus the LZ77 back-reference copy with its 32 KiB window. The stream
//! ends at the END-OF-BLOCK symbol of a **final** (`BFINAL=1`) block, and the consumed
//! byte count is reported so trailing bytes can be ignored (the oracle does the same:
//! `zlib:unzip(<<Zip/binary, 99, 99>>)` returns the payload).
//!
//! Every rejection follows `../../erts/emulator/zlib/inftrees.c`'s table checks — an
//! over-subscribed code, an incomplete code (except the code-length alphabet and the
//! single-code degenerate tree), a reserved block type, a missing end-of-block code,
//! a `LEN`/`NLEN` mismatch, a distance past the output produced so far, and input that
//! runs out before a final block — so a stream the oracle's inflater rejects is
//! rejected here too, and vice versa.
//!
//! # Failure is one class
//!
//! Every failure — malformed input, or output past the caller's ceiling — is reported
//! as [`BeamError::BadArg`], the class [`beam_codec::compress::decompress`] uses for a
//! stream it cannot inflate. The caller (`crate::zlib`) translates that into the
//! oracle's `data_error`, exactly as it does for the RFC 1950 codec. A partially
//! decoded stream is **never** returned: there is no "best effort" result.
//!
//! This module is safe Rust (`#![deny(unsafe_code)]` is crate-wide): the window is a
//! `Vec<u8>` indexed by checked arithmetic, never a raw pointer.

use beam_common::{BeamError, Result};

/// The longest code a DEFLATE Huffman table may assign (RFC 1951 §3.2.7).
const MAX_CODE_LENGTH: u8 = 15;

/// The failure this module reports: the input is not a well-formed raw-DEFLATE stream,
/// or it inflates past the caller's ceiling.
///
/// [`BeamError::BadArg`] is the class the workspace's RFC 1950 codec uses for a stream
/// it cannot inflate, and it is the only class that reaches the caller as the oracle's
/// `data_error`; see the module docs.
fn malformed() -> BeamError {
    BeamError::BadArg
}

/// Inflates one raw-DEFLATE stream (RFC 1951), stopping at the end of its final block.
///
/// Returns the inflated bytes and the number of **input bytes consumed**. The stream is
/// a bit stream, so a final block may end mid-byte and trailing bytes after it are
/// simply not consumed. Output is capped at `max_output` bytes; a stream that would
/// inflate past the cap is a [`malformed`] error rather than an unbounded allocation.
///
/// # Errors
///
/// [`BeamError::BadArg`] for every malformed-stream and ceiling case (see the module
/// docs); never a partially decoded result.
pub(crate) fn inflate(input: &[u8], max_output: usize) -> Result<(Vec<u8>, usize)> {
    let mut bits = Bits::new(input);
    let mut out = Vec::new();
    loop {
        let last = bits.bits(1)? == 1;
        match bits.bits(2)? {
            // 00 — a stored (uncompressed) block.
            0 => stored_block(&mut bits, &mut out, max_output)?,
            // 01 — fixed Huffman codes (RFC 1951 §3.2.6).
            1 => {
                let (literals, distances) = (fixed_literal_tree()?, fixed_distance_tree()?);
                compressed_block(&mut bits, &mut out, max_output, &literals, &distances)?;
            }
            // 10 — dynamic Huffman codes (RFC 1951 §3.2.7).
            2 => {
                let (literals, distances) = dynamic_trees(&mut bits)?;
                compressed_block(&mut bits, &mut out, max_output, &literals, &distances)?;
            }
            // 11 — reserved.
            _ => return Err(malformed()),
        }
        if last {
            return Ok((out, bits.consumed()));
        }
    }
}

// ---------------------------------------------------------------------------
// The bit stream
// ---------------------------------------------------------------------------

/// A reader over the DEFLATE bit stream: bits are stored least-significant-bit first
/// within each byte (RFC 1951 §3.1.1), while a Huffman code is read
/// most-significant-bit first (see [`Huffman::decode`]).
struct Bits<'a> {
    input: &'a [u8],
    /// The bit offset of the next unread bit.
    pos: usize,
}

impl<'a> Bits<'a> {
    fn new(input: &'a [u8]) -> Bits<'a> {
        Bits { input, pos: 0 }
    }

    /// The next bit, or [`malformed`] when the input is exhausted.
    fn bit(&mut self) -> Result<u32> {
        if self.pos >= self.input.len() * 8 {
            return Err(malformed());
        }
        let byte = self.input[self.pos / 8];
        let shift = self.pos % 8;
        self.pos += 1;
        Ok(u32::from((byte >> shift) & 1))
    }

    /// `n` bits, least-significant-bit first — the order of every integer field of the
    /// format (an extra length, a repeat count, `HLIT`, a stored `LEN`).
    fn bits(&mut self, n: u32) -> Result<u32> {
        let mut value = 0u32;
        for index in 0..n {
            value |= self.bit()? << index;
        }
        Ok(value)
    }

    /// Skips the padding bits of a stored block's header (RFC 1951 §3.2.4).
    fn align_to_byte(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }

    /// One whole byte (the reader is byte-aligned where this is used).
    fn byte(&mut self) -> Result<u8> {
        Ok(self.bits(8)? as u8)
    }

    /// A little-endian 16-bit field (a stored block's `LEN`/`NLEN`).
    fn u16_le(&mut self) -> Result<u16> {
        let low = self.byte()?;
        let high = self.byte()?;
        Ok(u16::from(low) | (u16::from(high) << 8))
    }

    /// The number of input bytes the stream has consumed: the bits read, rounded up to
    /// a whole byte (a final block may end mid-byte).
    fn consumed(&self) -> usize {
        self.pos.div_ceil(8)
    }
}

// ---------------------------------------------------------------------------
// Huffman decoding
// ---------------------------------------------------------------------------

/// A canonical Huffman decoding table (RFC 1951 §3.2.2).
///
/// The table is stored the way the specification defines the codes: for every code
/// length, the symbols that use it, in ascending symbol order. The code of a symbol is
/// then found by counting, not by a lookup keyed on the code, which keeps the whole
/// decoder allocation-light and free of any size assumption.
struct Huffman {
    /// `counts[len]` — how many codes of length `len` (`counts[0]` is always 0: no
    /// symbol is ever assigned a zero-length code).
    counts: [u16; 16],
    /// The symbols in canonical order: every symbol with a code of length 1 (ascending),
    /// then length 2, and so on.
    symbols: Vec<u16>,
}

impl Huffman {
    /// Builds the table from a code-length-per-symbol array.
    ///
    /// `allow_incomplete` mirrors the C `inftrees.c:62` exception: an incomplete code is
    /// an error unless it is the **code-length alphabet** (`type == CODES`) or a
    /// degenerate single-code tree. An alphabet with no codes at all is accepted (the
    /// C's `max == 0` arm) and simply fails to decode any symbol.
    fn new(lengths: &[u8], allow_incomplete: bool) -> Result<Huffman> {
        if lengths.len() > usize::from(u16::MAX) {
            return Err(malformed());
        }
        let mut counts = [0u16; 16];
        for &length in lengths {
            if length > MAX_CODE_LENGTH {
                return Err(malformed());
            }
            counts[usize::from(length)] += 1;
        }
        // `max` is the longest code length actually used (0 when there is no code at all).
        let mut max = 0usize;
        for length in (1..=usize::from(MAX_CODE_LENGTH)).rev() {
            if counts[length] != 0 {
                max = length;
                break;
            }
        }
        if max == 0 {
            return Ok(Huffman {
                counts: [0; 16],
                symbols: Vec::new(),
            });
        }
        let mut left = 1i32;
        for length in 1..=usize::from(MAX_CODE_LENGTH) {
            left <<= 1;
            left -= i32::from(counts[length]);
            if left < 0 {
                return Err(malformed()); // over-subscribed
            }
        }
        if left > 0 && !allow_incomplete && max != 1 {
            return Err(malformed()); // incomplete
        }
        // Canonical order: per length, the symbols in ascending order.
        let mut offsets = [0usize; 16];
        let mut total = 0usize;
        for length in 1..=usize::from(MAX_CODE_LENGTH) {
            offsets[length] = total;
            total += usize::from(counts[length]);
        }
        let mut symbols = vec![0u16; total];
        for (symbol, &length) in lengths.iter().enumerate() {
            let length = usize::from(length);
            if length != 0 {
                symbols[offsets[length]] = symbol as u16;
                offsets[length] += 1;
            }
        }
        counts[0] = 0;
        Ok(Huffman { counts, symbols })
    }

    /// Reads one symbol: the code is accumulated bit by bit, most-significant-bit
    /// first, until it falls inside a code length's canonical window.
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16> {
        let mut code = 0usize;
        let mut first = 0usize;
        let mut index = 0usize;
        for length in 1..=usize::from(MAX_CODE_LENGTH) {
            code |= bits.bit()? as usize;
            let count = usize::from(self.counts[length]);
            if code < first + count {
                return Ok(self.symbols[index + code - first]);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(malformed())
    }
}

/// The fixed literal/length alphabet (RFC 1951 §3.2.6): symbols 0..143 and 280..287 use
/// 8-bit codes, 144..255 use 9-bit codes and 256..279 use 7-bit codes.
fn fixed_literal_tree() -> Result<Huffman> {
    let mut lengths = [0u8; 288];
    for (symbol, length) in lengths.iter_mut().enumerate() {
        *length = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    Huffman::new(&lengths, false)
}

/// The fixed distance alphabet (RFC 1951 §3.2.6): 32 codes, all 5 bits. Symbols 30 and
/// 31 exist only to fill the code and are rejected as distances.
fn fixed_distance_tree() -> Result<Huffman> {
    Huffman::new(&[5u8; 32], false)
}

// ---------------------------------------------------------------------------
// The blocks
// ---------------------------------------------------------------------------

/// A stored block (RFC 1951 §3.2.4): byte-aligned `LEN`, its complement `NLEN`, then
/// `LEN` literal bytes.
fn stored_block(bits: &mut Bits<'_>, out: &mut Vec<u8>, max_output: usize) -> Result<()> {
    bits.align_to_byte();
    let length = bits.u16_le()?;
    let complement = bits.u16_le()?;
    if length != !complement {
        return Err(malformed());
    }
    let length = usize::from(length);
    if out.len() + length > max_output {
        return Err(malformed());
    }
    for _ in 0..length {
        out.push(bits.byte()?);
    }
    Ok(())
}

/// A Huffman-coded block (both the fixed and the dynamic alphabets): literals, and
/// length/distance pairs copied from the window already produced.
fn compressed_block(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    max_output: usize,
    literals: &Huffman,
    distances: &Huffman,
) -> Result<()> {
    loop {
        let symbol = literals.decode(bits)?;
        match symbol {
            // A literal byte.
            0..=255 => {
                if out.len() >= max_output {
                    return Err(malformed());
                }
                out.push(symbol as u8);
            }
            // 256 — end of block.
            256 => return Ok(()),
            // 257..285 — a length, followed by a distance.
            257..=285 => {
                let index = usize::from(symbol - 257);
                let length =
                    usize::from(LENGTH_BASE[index]) + bits.bits(u32::from(LENGTH_EXTRA[index]))?
                        as usize;
                let distance_symbol = distances.decode(bits)?;
                if distance_symbol > 29 {
                    return Err(malformed());
                }
                let index = usize::from(distance_symbol);
                let distance = usize::from(DISTANCE_BASE[index])
                    + bits.bits(u32::from(DISTANCE_EXTRA[index]))? as usize;
                copy_match(out, distance, length, max_output)?;
            }
            // 286/287 fill the fixed code but name no length.
            _ => return Err(malformed()),
        }
    }
}

/// Copies `length` bytes from `distance` back in the window (RFC 1951 §3.2.3).
///
/// The window is the output itself, so overlapping copies are legal and run
/// byte-by-byte; the elements are re-indexed on every step, which is valid safe Rust
/// even when the `Vec` reallocates.
fn copy_match(out: &mut Vec<u8>, distance: usize, length: usize, max_output: usize) -> Result<()> {
    if distance == 0 || distance > out.len() {
        return Err(malformed());
    }
    if out.len() + length > max_output {
        return Err(malformed());
    }
    let start = out.len() - distance;
    for index in 0..length {
        let byte = out[start + index];
        out.push(byte);
    }
    Ok(())
}

/// Decodes a dynamic block's two alphabets (RFC 1951 §3.2.7): the code-length alphabet,
/// the code lengths it encodes (with the repeat codes 16/17/18), and the
/// literal/length and distance alphabets built from them.
fn dynamic_trees(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman)> {
    let literal_count = bits.bits(5)? as usize + 257;
    let distance_count = bits.bits(5)? as usize + 1;
    let code_length_count = bits.bits(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err(malformed());
    }
    let mut code_lengths = [0u8; 19];
    for index in 0..code_length_count {
        code_lengths[CODE_LENGTH_ORDER[index]] = bits.bits(3)? as u8;
    }
    // The code-length alphabet is the one table an incomplete code may describe.
    let code_length_tree = Huffman::new(&code_lengths, true)?;
    let mut lengths = vec![0u8; literal_count + distance_count];
    let mut index = 0usize;
    while index < lengths.len() {
        let symbol = code_length_tree.decode(bits)?;
        match symbol {
            0..=15 => {
                lengths[index] = symbol as u8;
                index += 1;
            }
            // 16 — repeat the previous length 3..6 times.
            16 => {
                if index == 0 {
                    return Err(malformed());
                }
                let previous = lengths[index - 1];
                let repeat = bits.bits(2)? as usize + 3;
                if index + repeat > lengths.len() {
                    return Err(malformed());
                }
                for _ in 0..repeat {
                    lengths[index] = previous;
                    index += 1;
                }
            }
            // 17 — 3..10 zeros; 18 — 11..138 zeros.
            17 | 18 => {
                let (base, extra_bits) = if symbol == 17 { (3, 3) } else { (11, 7) };
                let repeat = base + bits.bits(extra_bits)? as usize;
                if index + repeat > lengths.len() {
                    return Err(malformed());
                }
                index += repeat;
            }
            _ => return Err(malformed()),
        }
    }
    // The end-of-block code must exist (C's "invalid code -- missing end-of-block").
    if lengths[256] == 0 {
        return Err(malformed());
    }
    let literals = Huffman::new(&lengths[..literal_count], false)?;
    let distances = Huffman::new(&lengths[literal_count..], false)?;
    Ok((literals, distances))
}

/// The order the code-length alphabet's own lengths are transmitted in
/// (RFC 1951 §3.2.7).
const CODE_LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// The base length of each length symbol 257..285 (`lbase`, `inftrees.c`).
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115,
    131, 163, 195, 227, 258,
];

/// The extra bits each length symbol 257..285 reads (`lextra`, `inftrees.c`).
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// The base distance of each distance symbol 0..29 (`dbase`, `inftrees.c`).
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];

/// The extra bits each distance symbol 0..29 reads (`dext`, `inftrees.c`).
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12,
    13, 13,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A bit-level writer, used to hand-build the streams an encoder cannot be asked
    /// for on demand. The stream is little-endian within a byte ([`write_bits`], the
    /// order of every integer field of the format) while a Huffman code is written
    /// most-significant-bit first ([`write_code`]).
    #[derive(Default)]
    struct BitWriter {
        bytes: Vec<u8>,
        bit: usize,
    }

    impl BitWriter {
        fn write_bit(&mut self, bit: u32) {
            if self.bit % 8 == 0 {
                self.bytes.push(0);
            }
            if bit != 0 {
                let index = self.bytes.len() - 1;
                self.bytes[index] |= 1 << (self.bit % 8);
            }
            self.bit += 1;
        }

        /// `n` bits of `value`, least significant first.
        fn write_bits(&mut self, value: u32, n: u32) {
            for index in 0..n {
                self.write_bit((value >> index) & 1);
            }
        }

        /// An `n`-bit Huffman code, most significant bit first.
        fn write_code(&mut self, code: u32, n: u32) {
            for index in (0..n).rev() {
                self.write_bit((code >> index) & 1);
            }
        }

        fn finish(self) -> Vec<u8> {
            self.bytes
        }
    }

    /// A stored (type 0) block holding `abc`, plus its expected consumed length.
    fn stored_abc() -> Vec<u8> {
        // BFINAL=1, BTYPE=00, then the padding bits, LEN=3, NLEN=!3 and the three bytes.
        vec![0x01, 3, 0, 0xFC, 0xFF, b'a', b'b', b'c']
    }

    #[test]
    fn a_stored_block_decodes_and_its_padding_bits_are_skipped() {
        let stream = stored_abc();
        let (out, consumed) = inflate(&stream, usize::MAX).expect("a well-formed stored block");
        assert_eq!(out, b"abc");
        assert_eq!(consumed, stream.len());
    }

    #[test]
    fn the_oracle_goldens_decode() {
        // `zlib:zip(<<"hello">>)` and `zlib:zip(<<>>)` under OTP 29.1.1 / erts-17.1
        // (both fixed-Huffman blocks; verified live against the oracle).
        assert_eq!(
            inflate(&[203, 72, 205, 201, 201, 7, 0], usize::MAX)
                .expect("the oracle's stream")
                .0,
            b"hello"
        );
        assert_eq!(
            inflate(&[3, 0], usize::MAX).expect("the empty stream").0,
            Vec::<u8>::new()
        );
    }

    #[test]
    fn trailing_bytes_after_the_final_block_are_not_consumed() {
        let stream = stored_abc();
        let mut padded = stream.clone();
        padded.extend_from_slice(&[99, 99]);
        let (out, consumed) = inflate(&padded, usize::MAX).expect("trailing bytes are ignored");
        assert_eq!(out, b"abc");
        assert_eq!(consumed, stream.len());
    }

    /// A hand-built **dynamic** (type 2) block, so the dynamic path — the code-length
    /// alphabet, the repeat codes 16/17/18 and a complete dynamic literal tree — is
    /// covered without relying on an external encoder's block choice.
    ///
    /// The literal alphabet is `{65..71, 256}`, all length 3 (canonical codes
    /// `000`..`111`), the distance alphabet is empty, and the block inflates to `AB`.
    fn dynamic_ab() -> Vec<u8> {
        let mut w = BitWriter::default();
        w.write_bit(1); // BFINAL
        w.write_bits(2, 2); // BTYPE = 10 (dynamic)
        w.write_bits(0, 5); // HLIT  = 0  → 257 literal/length codes
        w.write_bits(0, 5); // HDIST = 0  → 1 distance code
        w.write_bits(10, 4); // HCLEN = 10 → 14 code-length entries
        // The code-length alphabet's own lengths, in the RFC's fixed order
        // `16,17,18,0,8,7,9,6,10,5,11,4,12,3`: 16→2, 17→3, 18→3, 0→2, 3→2, rest 0.
        for length in [2u32, 3, 3, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2] {
            w.write_bits(length, 3);
        }
        // Canonical codes over {0, 3, 16} (length 2 → 00, 01, 10) and
        // {17, 18} (length 3 → 110, 111).
        w.write_code(0b111, 3); // 18: 65 zero lengths …
        w.write_bits(54, 7); //     11 + 54 = 65
        w.write_code(0b01, 2); //  3: symbol 65 has length 3
        w.write_code(0b10, 2); // 16: repeat the previous length …
        w.write_bits(3, 2); //      3 + 3 = 6 more (symbols 66..71)
        w.write_code(0b111, 3); // 18: 184 zero lengths (symbols 72..255) …
        w.write_bits(127, 7); //    11 + 127 = 138
        for _ in 0..4 {
            w.write_code(0b110, 3); // 17: ten zero lengths …
            w.write_bits(7, 3);
        }
        w.write_code(0b110, 3); // 17: … and six more (138 + 40 + 6 = 184)
        w.write_bits(3, 3);
        w.write_code(0b01, 2); //  3: symbol 256 (end of block) has length 3
        w.write_code(0b00, 2); //  0: the single distance symbol has length 0
        // The literal tree: 65 → 000, 66 → 001, 67 → 010, 68 → 011, 69 → 100,
        // 70 → 101, 71 → 110, 256 → 111.
        w.write_code(0b000, 3); // 'A'
        w.write_code(0b001, 3); // 'B'
        w.write_code(0b111, 3); // end of block
        w.finish()
    }

    #[test]
    fn a_hand_built_dynamic_block_decodes() {
        let stream = dynamic_ab();
        let (out, consumed) = inflate(&stream, usize::MAX).expect("a well-formed dynamic block");
        assert_eq!(out, b"AB");
        assert_eq!(consumed, stream.len(), "the block ends on a byte boundary");
    }

    #[test]
    fn a_match_copies_from_the_window_and_may_overlap() {
        // A fixed block: literal 'a', then length 258 (symbol 285) at distance 1, then
        // end of block. The overlapping copy must run byte by byte.
        let mut w = BitWriter::default();
        w.write_bit(1);
        w.write_bits(1, 2); // BTYPE = 01 (fixed Huffman)
        // The fixed codes for symbols 0..143 are the 8-bit value 0x30 + symbol, so
        // 'a' (97) is 145.
        w.write_code(0b0011_0000 + 97, 8);
        // Symbols 280..287 use the 8-bit codes 192..199, so 285 is 197; its length is
        // 258 with no extra bits.
        w.write_code(197, 8);
        w.write_code(0, 5); // distance code 0 → distance 1, no extra bits
        w.write_code(0, 7); // end of block (symbol 256 → code 0)
        let stream = w.finish();

        let out = inflate(&stream, usize::MAX).expect("a well-formed fixed block").0;
        assert_eq!(out.len(), 259);
        assert!(out.iter().all(|&byte| byte == b'a'));
    }

    #[test]
    fn a_reserved_block_type_is_rejected() {
        // BFINAL=1, BTYPE=11.
        assert!(inflate(&[0b0000_0111], usize::MAX).is_err());
    }

    #[test]
    fn an_empty_or_truncated_stream_is_rejected() {
        assert!(inflate(&[], usize::MAX).is_err());
        // A stored block whose `LEN` never arrives.
        assert!(inflate(&[0x01], usize::MAX).is_err());
        // A fixed block with no end-of-block symbol.
        assert!(inflate(&[0x03], usize::MAX).is_err());
        // A fixed block that decodes a literal and then ends before its end-of-block
        // symbol (the input runs out inside the code).
        let mut w = BitWriter::default();
        w.write_bit(1);
        w.write_bits(1, 2);
        w.write_code(0b0011_0000 + 97, 8); // 'a'
        assert!(inflate(&w.finish(), usize::MAX).is_err());
    }

    #[test]
    fn a_stored_length_that_does_not_complement_is_rejected() {
        // LEN = 3 with NLEN = 0 (must be 0xFFFC), even though the bytes are present.
        let stream = vec![0x01, 3, 0, 0, 0, b'a', b'b', b'c'];
        assert!(inflate(&stream, usize::MAX).is_err());
    }

    #[test]
    fn a_distance_beyond_the_output_so_far_is_rejected() {
        // A fixed block: length code 257 (7-bit code 0000001) and distance code 0 with
        // nothing in the window yet.
        let mut w = BitWriter::default();
        w.write_bit(1);
        w.write_bits(1, 2);
        w.write_code(0b000_0001, 7);
        w.write_code(0, 5);
        w.write_code(0, 7);
        assert!(inflate(&w.finish(), usize::MAX).is_err());
    }

    #[test]
    fn the_output_ceiling_is_reported_not_overrun() {
        let stream = stored_abc();
        assert!(inflate(&stream, 2).is_err(), "three bytes over a two-byte ceiling");
        assert_eq!(
            inflate(&stream, 3).expect("exactly at the ceiling").0,
            b"abc"
        );
    }

    #[test]
    fn a_huffman_table_is_rejected_when_over_subscribed_or_incomplete() {
        // Over-subscribed: three codes of length 1.
        let mut lengths = vec![0u8; 4];
        lengths[0] = 1;
        lengths[1] = 1;
        lengths[2] = 1;
        assert!(Huffman::new(&lengths, false).is_err());
        assert!(
            Huffman::new(&lengths, true).is_err(),
            "the code-length exception does not license over-subscription"
        );

        // Incomplete: two codes of length 2 leave a hole — an error for an alphabet,
        // the documented exception for the code-length table.
        let mut lengths = vec![0u8; 4];
        lengths[0] = 2;
        lengths[1] = 2;
        assert!(Huffman::new(&lengths, false).is_err());
        assert!(Huffman::new(&lengths, true).is_ok());

        // A single code of length 1 is the one incomplete alphabet the C allows.
        let mut lengths = vec![0u8; 2];
        lengths[0] = 1;
        assert!(Huffman::new(&lengths, false).is_ok());

        // An alphabet with no codes at all is accepted, as the C accepts it …
        let empty = Huffman::new(&[0u8; 8], false).expect("an empty alphabet is not an error");
        // … but it cannot decode a symbol, so a stream that reaches for one fails
        // instead of silently producing an empty result.
        let mut bits = Bits::new(&[0xFF, 0xFF, 0xFF, 0xFF]);
        assert!(empty.decode(&mut bits).is_err());
    }

    #[test]
    fn a_code_length_beyond_the_maximum_is_rejected() {
        let mut lengths = vec![0u8; 4];
        lengths[0] = 16;
        assert!(Huffman::new(&lengths, true).is_err());
    }
}
