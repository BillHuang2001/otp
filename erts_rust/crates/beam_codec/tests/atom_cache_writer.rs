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

//! Integration tests for the two encoder-side seams added for `beam_dist`:
//!
//! 1. **Outbound atom cache** — [`AtomCacheWriter`] installed with
//!    [`Encoder::with_atom_cache`], which lets the encoder emit
//!    `ATOM_CACHE_REF` (`82`) instead of inline atoms.
//! 2. **`LOCAL_EXT` (`121`) wrapping** — [`Encoder::finish_local`] /
//!    [`encode_term_local`], the `term_to_binary(Term, [local])` image.
//!
//! Everything here goes through the **public** API only; the crate stays
//! `#![deny(unsafe_code)]` and no `unsafe` appears below.

use beam_alloc::Heap;
use beam_codec::{
    encode_term, encode_term_body, encode_term_local, AtomCacheWriter, AtomEncoding, Encoder,
};
use beam_term::prelude::{make_atom, make_small, Atom, AtomTable, NIL};

/// Builds a heap big enough for the terms these tests construct.
fn new_heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

/// A test [`AtomCacheWriter`]: it caches exactly the atoms listed in `cached`, in
/// order, assigning them consecutive payload indices from `start`, and declines
/// (returns `None`) every other atom.
struct TestCache {
    /// The atoms the cache accepts, in index order.
    cached: Vec<Atom>,
    /// The payload index assigned to `cached[0]`.
    start: u8,
    /// Every atom the encoder consulted, in order.
    seen: Vec<Atom>,
}

impl TestCache {
    fn new(cached: Vec<Atom>, start: u8) -> TestCache {
        TestCache {
            cached,
            start,
            seen: Vec::new(),
        }
    }
}

impl AtomCacheWriter for TestCache {
    fn intern(&mut self, atom: Atom, _atoms: &AtomTable) -> Option<u8> {
        self.seen.push(atom);
        self.cached
            .iter()
            .position(|&cached| cached == atom)
            .map(|slot| self.start + slot as u8)
    }
}

#[test]
fn an_atom_with_no_cache_is_written_inline() {
    let mut atoms = AtomTable::new();
    let ok = atoms.intern("ok");

    // No `with_atom_cache` at all: the inline UTF-8 form, byte-for-byte.
    assert_eq!(
        encode_term_body(make_atom(ok), &atoms).unwrap(),
        vec![119, 2, 111, 107]
    );
    assert_eq!(
        encode_term(make_atom(ok), &atoms).unwrap(),
        vec![131, 119, 2, 111, 107]
    );
}

#[test]
fn installing_a_cache_that_declines_every_atom_changes_nothing() {
    let mut atoms = AtomTable::new();
    let a = atoms.intern("a");
    let mut heap = new_heap(256);
    // A representative term: a tuple holding an atom, a small and a byte list.
    let tail = heap.cons(make_small(2), NIL).unwrap();
    let list = heap.cons(make_small(1), tail).unwrap();
    let term = heap.tuple(&[make_atom(a), make_small(7), list]).unwrap();

    let plain = encode_term(term, &atoms).unwrap();

    let mut cache = TestCache::new(Vec::new(), 0);
    let mut encoder = Encoder::new(&atoms).with_atom_cache(&mut cache);
    encoder.encode(term).unwrap();
    assert_eq!(encoder.finish(), plain);
    // `a` really was offered to the cache (declined, not skipped).
    assert_eq!(cache.seen, vec![a]);
}

#[test]
fn a_cached_atom_becomes_atom_cache_ref() {
    let mut atoms = AtomTable::new();
    let ok = atoms.intern("ok");

    let mut cache = TestCache::new(vec![ok], 3);
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
    encoder.encode(make_atom(ok)).unwrap();
    // `ATOM_CACHE_REF_EXT` (`82`) + the one-byte index — and nothing else.
    assert_eq!(encoder.finish(), vec![82, 3]);
}

#[test]
fn a_cache_miss_falls_through_to_the_inline_form() {
    let mut atoms = AtomTable::new();
    let ok = atoms.intern("ok");
    let other = atoms.intern("other");

    // The cache only accepts `other`, so `ok` must be written inline.
    let mut cache = TestCache::new(vec![other], 0);
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
    encoder.encode(make_atom(ok)).unwrap();
    assert_eq!(encoder.finish(), vec![119, 2, 111, 107]);
}

#[test]
fn a_nested_cached_atom_is_a_ref_while_the_others_stay_inline() {
    let mut atoms = AtomTable::new();
    let cached = atoms.intern("cache_me");
    let inline = atoms.intern("inline_me");
    let mut heap = new_heap(128);
    let term = heap
        .tuple(&[make_atom(cached), make_small(1), make_atom(inline)])
        .unwrap();

    let mut cache = TestCache::new(vec![cached], 7);
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
    encoder.encode(term).unwrap();

    let mut expected = vec![104u8, 3, 82, 7, 97, 1];
    expected.extend_from_slice(&[119, 9]);
    expected.extend_from_slice(b"inline_me");
    assert_eq!(encoder.finish(), expected);
}

#[test]
fn the_cache_composes_with_the_version_byte_and_the_atom_encoding() {
    let mut atoms = AtomTable::new();
    let ok = atoms.intern("ok");
    let term = make_atom(ok);

    // Declining cache + Latin-1: identical to an encoder with neither.
    let mut miss = TestCache::new(Vec::new(), 0);
    let mut encoder = Encoder::new(&atoms).with_atom_cache(&mut miss);
    encoder.set_atom_encoding(AtomEncoding::Latin1);
    encoder.encode(term).unwrap();
    assert_eq!(encoder.finish(), vec![131, 115, 2, 111, 107]);

    // A hit replaces the atom with `[82, index]`; the version byte is untouched and
    // the selected atom encoding is irrelevant on the cached path.
    let mut hit = TestCache::new(vec![ok], 4);
    let mut encoder = Encoder::new(&atoms).with_atom_cache(&mut hit);
    encoder.set_atom_encoding(AtomEncoding::Latin1);
    encoder.encode(term).unwrap();
    assert_eq!(encoder.finish(), vec![131, 82, 4]);
}

#[test]
fn a_cached_atom_skips_the_name_length_check() {
    // The C does no name-length validation on the cached path (the name travels in
    // the distribution header), so a >255-character atom is encodable through the
    // cache even though the inline path rejects it with `SystemLimit`.
    let mut atoms = AtomTable::new();
    let long = atoms.intern(&"a".repeat(256));

    // Inline: over-long names are a system limit.
    assert!(encode_term_body(make_atom(long), &atoms).is_err());

    let mut cache = TestCache::new(vec![long], 1);
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
    encoder.encode(make_atom(long)).unwrap();
    assert_eq!(encoder.finish(), vec![82, 1]);
}

#[test]
fn encode_term_local_prepends_the_local_wrapper() {
    let atoms = AtomTable::new();
    assert_eq!(
        encode_term_local(make_small(17), &atoms, 0xAABB_CCDD).unwrap(),
        vec![131, 121, 0xAA, 0xBB, 0xCC, 0xDD, 97, 17]
    );
}

#[test]
fn the_local_body_is_exactly_encode_term_body() {
    let mut atoms = AtomTable::new();
    let a = atoms.intern("a");
    let mut heap = new_heap(128);
    let term = heap.tuple(&[make_atom(a), make_small(1)]).unwrap();

    let body = encode_term_body(term, &atoms).unwrap();
    let image = encode_term_local(term, &atoms, 0x0102_0304).unwrap();

    // The wrapper is exactly six bytes: version, `LOCAL_EXT`, then the 4-byte hash.
    assert_eq!(&image[..6], &[131, 121, 0x01, 0x02, 0x03, 0x04]);
    assert_eq!(&image[6..], &body[..]);
}

#[test]
fn finish_local_normalizes_an_encoder_that_wrote_the_version_byte() {
    let mut atoms = AtomTable::new();
    let a = atoms.intern("a");
    let mut heap = new_heap(64);
    let list = heap.cons(make_atom(a), NIL).unwrap();

    let mut with_version = Encoder::new(&atoms);
    with_version.encode(list).unwrap();
    // `Encoder::new`'s leading version byte is stripped, so the two agree...
    let mut without_version = Encoder::new_body(&atoms);
    without_version.encode(list).unwrap();
    assert_eq!(
        with_version.finish_local(9),
        without_version.finish_local(9)
    );

    // ... and both equal the convenience entry point.
    let mut expected = vec![131u8, 121, 0, 0, 0, 9];
    expected.extend_from_slice(&encode_term_body(list, &atoms).unwrap());
    assert_eq!(encode_term_local(list, &atoms, 9).unwrap(), expected);
}

#[test]
fn local_and_atom_cache_seams_compose() {
    let mut atoms = AtomTable::new();
    let ok = atoms.intern("ok");
    let mut cache = TestCache::new(vec![ok], 2);

    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
    encoder.encode(make_atom(ok)).unwrap();
    // `[VERSION_MAGIC][LOCAL_EXT][hash:0,0,0,1][ATOM_CACHE_REF, 2]`.
    assert_eq!(encoder.finish_local(1), vec![131, 121, 0, 0, 0, 1, 82, 2]);
}
