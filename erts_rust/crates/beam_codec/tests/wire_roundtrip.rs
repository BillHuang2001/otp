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

//! End-to-end round-trips for the two encoder/decoder seams that `beam_dist`
//! needs together:
//!
//! 1. **`LOCAL_EXT` (`121`) wrapping** — [`encode_term_local`] /
//!    [`Encoder::finish_local`] write the `term_to_binary(Term, [local])` image
//!    `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE][body]`, and [`decode_term`] consumes
//!    the 4-byte hash and decodes the term that follows it.
//! 2. **The atom cache** — the outbound [`AtomCacheWriter`] installed with
//!    [`Encoder::with_atom_cache`] emits `ATOM_CACHE_REF` (`82`) instead of the
//!    inline atom, and the inbound [`AtomCache`] attached with
//!    [`Decoder::with_atom_cache`] resolves that reference back to the atom.
//!
//! Neither seam can be proven in isolation: the write side (`Encoder` +
//! `AtomCacheWriter` / `finish_local`) and the read side (`Decoder` +
//! `AtomCache` / `decode_term`) must meet on the same wire bytes. These tests
//! drive both halves against each other through the **public** API only.
//!
//! **Structural equality across heaps.** The original and the decoded terms live
//! on different [`Heap`]s, so the `__eq__` primitive (a raw-word comparison) is
//! unusable. Every round-trip therefore re-encodes the *decoded* term with the
//! *decoded* term's [`AtomTable`] and asserts the bytes equal the original
//! term's body encoding — a byte-level structural-equality proxy, since atoms
//! encode by name.
//!
//! The crate stays `#![deny(unsafe_code)]`; no `unsafe` appears below.

use beam_alloc::Heap;
use beam_codec::{
    decode_term, encode_term_body, encode_term_local, AtomCache, AtomCacheWriter, Decoder, Encoder,
    ATOM_CACHE_REF_EXT, LOCAL_EXT, VERSION_MAGIC,
};
use beam_term::prelude::{make_atom, make_small, Atom, AtomTable, NIL};

/// Builds a heap big enough for the handful of words these terms need.
fn new_heap(words: usize) -> Heap {
    Heap::new(words).expect("the carrier allocator never fails for these sizes")
}

// ---------------------------------------------------------------------------
// 1. `LOCAL_EXT` (`121`) encode → decode round trip
// ---------------------------------------------------------------------------

/// Builds the `{ok, 17, [a, b, a]}` term suggested by the objective: a tuple that
/// mixes an inline atom, a small integer and a non-byte cons list, so the body
/// exercises several tag families at once.
fn build_local_term(atoms: &mut AtomTable, heap: &mut Heap) -> beam_term::Eterm {
    let ok = atoms.intern("ok");
    let a = atoms.intern("a");
    let b = atoms.intern("b");
    // `[a, b, a]` — right-folded: `a : b : a : []`.
    let tail = heap.cons(make_atom(a), NIL).unwrap();
    let middle = heap.cons(make_atom(b), tail).unwrap();
    let list = heap.cons(make_atom(a), middle).unwrap();
    heap.tuple(&[make_atom(ok), make_small(17), list]).unwrap()
}

#[test]
fn local_ext_image_has_the_exact_wrapper_prefix() {
    let mut atoms = AtomTable::new();
    let mut heap = new_heap(64);
    let term = build_local_term(&mut atoms, &mut heap);

    let body = encode_term_body(term, &atoms).unwrap();
    let hash = 0xAABB_CCDDu32;
    let image = encode_term_local(term, &atoms, hash).unwrap();

    // `[VERSION_MAGIC][LOCAL_EXT][hash:4 BE][body]` — the wrapper is exactly six
    // bytes and the body is `encode_term_body`, byte-for-byte.
    assert_eq!(image[0], VERSION_MAGIC);
    assert_eq!(image[1], LOCAL_EXT);
    assert_eq!(&image[2..6], hash.to_be_bytes().as_slice());
    assert_eq!(&image[6..], body.as_slice());
    assert_eq!(image.len(), 6 + body.len());
}

#[test]
fn local_ext_image_round_trips_through_decode() {
    let mut atoms = AtomTable::new();
    let mut heap = new_heap(64);
    let term = build_local_term(&mut atoms, &mut heap);
    let body = encode_term_body(term, &atoms).unwrap();

    let image = encode_term_local(term, &atoms, 0x0102_0304).unwrap();

    // Decode into a *fresh* heap and atom table: `decode_term` consumes the 4-byte
    // hash and materializes the following term.
    let mut heap2 = new_heap(64);
    let mut atoms2 = AtomTable::new();
    let decoded = decode_term(&image, &mut heap2, &mut atoms2).unwrap();

    // Structural equality across the two heaps: the decoded term re-encodes to the
    // exact bytes of the original body (atoms encode by name).
    assert_eq!(encode_term_body(decoded, &atoms2).unwrap(), body);
}

#[test]
fn a_different_hash_changes_only_the_wrapper_prefix() {
    let mut atoms = AtomTable::new();
    let mut heap = new_heap(64);
    let term = build_local_term(&mut atoms, &mut heap);
    let body = encode_term_body(term, &atoms).unwrap();

    let first = encode_term_local(term, &atoms, 0x0000_0001).unwrap();
    let second = encode_term_local(term, &atoms, 0xFFFF_FFFE).unwrap();

    // Same body twice, different hash: the prefixes differ and so do the images.
    assert_eq!(&first[6..], body.as_slice());
    assert_eq!(&second[6..], body.as_slice());
    assert_eq!(&first[..6], &[VERSION_MAGIC, LOCAL_EXT, 0, 0, 0, 1]);
    assert_eq!(&second[..6], &[VERSION_MAGIC, LOCAL_EXT, 255, 255, 255, 254]);
    assert_ne!(first[2..6].to_vec(), second[2..6].to_vec());

    // Both still decode to the same term.
    let mut heap2 = new_heap(64);
    let mut atoms2 = AtomTable::new();
    let decoded = decode_term(&second, &mut heap2, &mut atoms2).unwrap();
    assert_eq!(encode_term_body(decoded, &atoms2).unwrap(), body);
}

// ---------------------------------------------------------------------------
// 2. Atom-cache encode → decode round trip (the `beam_dist` usage pattern)
// ---------------------------------------------------------------------------

/// A test [`AtomCacheWriter`] that caches `a` at payload index `0` and `b` at
/// index `1`, declining every other atom — the fixed-index mapping described in
/// the objective.
struct FixedIndexCache;

impl AtomCacheWriter for FixedIndexCache {
    fn intern(&mut self, atom: Atom, atoms: &AtomTable) -> Option<u8> {
        match atoms.name(atom) {
            Some("a") => Some(0),
            Some("b") => Some(1),
            _ => None,
        }
    }
}

/// A test [`AtomCacheWriter`] that declines *every* atom (the no-cache control),
/// matching C's `enc_atom` when the cache lookup returns `-1`.
struct DecliningCache;

impl AtomCacheWriter for DecliningCache {
    fn intern(&mut self, _atom: Atom, _atoms: &AtomTable) -> Option<u8> {
        None
    }
}

/// Builds the `{a, b, a, a}` tuple: four atoms, two of them repeated, so the
/// cache must fire more than once for the same atom and interleave two indices.
fn build_cache_term(atoms: &mut AtomTable, heap: &mut Heap) -> beam_term::Eterm {
    let a = atoms.intern("a");
    let b = atoms.intern("b");
    heap.tuple(&[make_atom(a), make_atom(b), make_atom(a), make_atom(a)])
        .unwrap()
}

#[test]
fn cached_atoms_encode_as_atom_cache_refs() {
    let mut atoms = AtomTable::new();
    let mut heap = new_heap(64);
    let term = build_cache_term(&mut atoms, &mut heap);

    let mut cache = FixedIndexCache;
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
    encoder.encode(term).unwrap();
    let payload = encoder.finish();

    // `[SMALL_TUPLE_EXT, arity 4, [82,0], [82,1], [82,0], [82,0]]` — every atom is
    // a cache reference, in wire order.
    assert_eq!(payload, vec![104, 4, 82, 0, 82, 1, 82, 0, 82, 0]);
    assert_eq!(payload[2], ATOM_CACHE_REF_EXT);
}

#[test]
fn an_atom_cache_payload_round_trips_through_decode() {
    let mut atoms = AtomTable::new();
    let mut heap = new_heap(64);
    let term = build_cache_term(&mut atoms, &mut heap);
    let body = encode_term_body(term, &atoms).unwrap();

    // Encode with the outbound cache.
    let mut writer = FixedIndexCache;
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut writer);
    encoder.encode(term).unwrap();
    let payload = encoder.finish();

    // The receiver's atom table interns the referenced atoms at the indices the
    // outbound cache assigned (`a -> 0`, `b -> 1`), then the inbound cache resolves
    // the `82` refs to them.
    let mut atoms2 = AtomTable::new();
    let mut cache = AtomCache::new();
    cache.set(0, atoms2.intern("a"));
    cache.set(1, atoms2.intern("b"));

    let mut heap2 = new_heap(64);
    let mut decoder = Decoder::new_body(&payload).with_atom_cache(&cache);
    let decoded = decoder.decode(&mut heap2, &mut atoms2).unwrap();
    decoder.finish().unwrap();

    // The cache-resolved term is structurally identical to the un-cached encoding.
    assert_eq!(encode_term_body(decoded, &atoms2).unwrap(), body);
}

// ---------------------------------------------------------------------------
// 3. No-cache control
// ---------------------------------------------------------------------------

#[test]
fn an_all_declining_cache_is_byte_identical_to_the_inline_form() {
    let mut atoms = AtomTable::new();
    let mut heap = new_heap(64);
    let term = build_cache_term(&mut atoms, &mut heap);

    let expected = encode_term_body(term, &atoms).unwrap();

    let mut cache = DecliningCache;
    let mut encoder = Encoder::new_body(&atoms).with_atom_cache(&mut cache);
    encoder.encode(term).unwrap();
    let payload = encoder.finish();

    // No cache hit ⇒ the payload is byte-identical to the plain body...
    assert_eq!(payload, expected);
    // ... and no atom was written as `ATOM_CACHE_REF` (`82`) anywhere in it.
    assert!(
        !payload.contains(&ATOM_CACHE_REF_EXT),
        "an all-declining cache must not emit a `82` ref: {payload:?}"
    );
}
