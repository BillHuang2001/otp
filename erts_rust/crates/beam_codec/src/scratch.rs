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
//! [`EncodeScratch`] — a reusable output buffer for `term_to_binary`.
//!
//! Every [`Encoder`](crate::Encoder) owns its output [`Writer`], and
//! [`Encoder::finish`](crate::Encoder::finish) moves the `Vec` out. A host that
//! encodes repeatedly (`beam_bifs`'s `term_to_binary/1,2`) therefore paid one
//! `Vec` allocation per call even though it copies the image into its own heap
//! immediately (the bytes never outlive the call). This type owns that buffer
//! across calls: [`EncodeScratch::writer`] lends it to an
//! [`Encoder::with_writer`](crate::Encoder::with_writer), and
//! [`EncodeScratch::recycle`] takes it back once the image has been copied out.
//!
//! It is the encode-side twin of `beam_bifs`'s per-thread decode scratch, and it
//! carries **no** term or codec state — only capacity — so recycling it can never
//! change an encoded byte.
use crate::writer::Writer;

/// The capacity [`EncodeScratch::new`] reserves: the encoder's own default image
/// reservation, so the common small image never grows the buffer
/// (`DEFAULT_IMAGE_CAPACITY`, `src/encode/mod.rs`).
const INITIAL_CAPACITY: usize = 128;

/// A reusable, caller-owned encode buffer (see the module docs).
///
/// The buffer is *moved into* the encoder by [`EncodeScratch::writer`] and moved back
/// by [`EncodeScratch::recycle`]; between those two calls the scratch is empty, which
/// is the only state in which it can be handed out again. A dropped/unrecycled buffer
/// is a lost allocation, never a wrong image.
pub struct EncodeScratch {
    /// The recyclable byte buffer (empty between [`writer`](Self::writer) and
    /// [`recycle`](Self::recycle)).
    buf: Vec<u8>,
}

impl EncodeScratch {
    /// Creates a scratch with the encoder's default image capacity reserved.
    pub fn new() -> EncodeScratch {
        EncodeScratch::with_capacity(INITIAL_CAPACITY)
    }

    /// Creates a scratch reserving `capacity` bytes up front.
    pub fn with_capacity(capacity: usize) -> EncodeScratch {
        EncodeScratch {
            buf: Vec::with_capacity(capacity),
        }
    }

    /// Takes the buffer as a [`Writer`], leaving the scratch empty.
    ///
    /// Feed it to [`Encoder::with_writer`](crate::Encoder::with_writer) (whole image)
    /// or [`Encoder::new_body_with`](crate::Encoder::new_body_with) (body only), then
    /// hand the finished buffer back with [`EncodeScratch::recycle`].
    pub fn writer(&mut self) -> Writer {
        Writer::from_vec(core::mem::take(&mut self.buf))
    }

    /// Returns a finished writer's buffer, keeping its allocation for the next encode.
    ///
    /// The buffer is cleared first, so the next [`EncodeScratch::writer`] starts at an
    /// empty image — the bytes handed out are always the caller's own.
    pub fn recycle(&mut self, writer: Writer) {
        let mut buf = writer.into_vec();
        buf.clear();
        self.buf = buf;
    }

    /// Discards the buffered bytes, keeping the allocation.
    pub fn clear(&mut self) {
        self.buf.clear();
    }

    /// Whether the scratch currently holds no bytes (it is empty between
    /// [`writer`](Self::writer) and [`recycle`](Self::recycle)).
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// The buffer's reserved capacity, in bytes.
    pub fn capacity(&self) -> usize {
        self.buf.capacity()
    }
}

impl Default for EncodeScratch {
    fn default() -> EncodeScratch {
        EncodeScratch::new()
    }
}

#[cfg(test)]
mod tests {
    //! Reuse must not change a byte: two encodes of the same term through one scratch
    //! agree with each other and with `encode_term`, and the buffer is empty between
    //! hand-outs (so a stale image can never leak into the next encode).
    use super::*;
    use crate::encode_term;
    use beam_alloc::Heap;
    use beam_term::prelude::{make_small, Eterm};
    use beam_term::AtomTable;

    fn payload(heap: &mut Heap, atoms: &mut AtomTable) -> Eterm {
        // A small list is enough to exercise several tags (atoms, a list, smalls).
        let a = beam_term::prelude::make_atom(atoms.intern("a"));
        let b = beam_term::prelude::make_atom(atoms.intern("b"));
        let mut tail = beam_term::prelude::NIL;
        tail = heap.cons(make_small(2), tail).unwrap();
        tail = heap.cons(make_small(1), tail).unwrap();
        heap.tuple(&[a, b, tail]).unwrap()
    }

    fn encode_through(scratch: &mut EncodeScratch, term: Eterm, atoms: &AtomTable) -> Vec<u8> {
        let mut encoder = crate::Encoder::with_writer(atoms, scratch.writer());
        encoder.encode(term).expect("the term encodes");
        let writer = encoder.take_writer();
        let image = writer.as_slice().to_vec();
        scratch.recycle(writer);
        image
    }

    #[test]
    fn two_encodes_through_one_scratch_are_identical_and_the_buffer_is_emptied() {
        let mut atoms = AtomTable::new();
        let mut heap = Heap::new(64).expect("a tiny heap");
        let term = payload(&mut heap, &mut atoms);

        let mut scratch = EncodeScratch::new();
        let initial_capacity = scratch.capacity();
        assert!(scratch.is_empty());

        let first = encode_through(&mut scratch, term, &atoms);
        // Recycled: the scratch keeps the capacity but holds no bytes.
        assert!(scratch.is_empty());
        assert_eq!(scratch.capacity(), initial_capacity, "the allocation is reused");

        let second = encode_through(&mut scratch, term, &atoms);
        assert_eq!(first, second, "a reused buffer produces the same bytes");
        assert_eq!(
            first,
            encode_term(term, &atoms).expect("the reference encode"),
            "the scratch path is byte-identical to `Encoder::new` + `finish`"
        );

        // Taking the writer empties the scratch until it is recycled.
        let writer = scratch.writer();
        assert!(writer.is_empty());
        assert!(scratch.is_empty());
        assert_eq!(scratch.capacity(), 0, "the buffer moved into the writer");
        scratch.recycle(writer);
        assert_eq!(scratch.capacity(), initial_capacity);
    }
}
