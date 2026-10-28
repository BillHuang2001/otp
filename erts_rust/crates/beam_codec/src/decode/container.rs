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
//! The `Decoder`'s **container** arms: tuples, lists and maps.
//!
//! These are the tags whose element count comes from the wire, and they are the
//! only tags whose decode allocates a *variable* number of terms. The wire rules
//! (the arity bounds, the bit-31 pair-count guard, the duplicate-key rejection)
//! live here with the construction they guard; [`super`] holds the tag dispatch.
//!
//! ## Small containers are staged on the stack
//!
//! The elements of a tuple/list/map are decoded into a staging buffer before the
//! container term is built (a container's header carries its size, so the elements
//! must exist first). A staging `Vec` was one allocation per container on the hot
//! path; every container a real payload has is far below
//! [`STACK_STAGE`] elements, so the small path stages into a stack array and the
//! `Vec` path is only reached by a container the wire claims is larger — where the
//! existing [`CONTAINER_PREALLOC_CAP`](super::CONTAINER_PREALLOC_CAP) clamp keeps a
//! hostile count from driving an unbounded allocation.
//!
//! A list's *spine* is written by [`crate::term::write_list`] in one heap
//! reservation (the C's `hp += 2` per cell, `external.c`), so a `LIST_EXT` of `n`
//! elements costs one bump reservation instead of `n` cons calls.
use beam_alloc::Heap;
use beam_common::Result;
use beam_term::prelude::{Eterm, NIL};
use beam_term::AtomTable;

use super::{Decoder, CONTAINER_PREALLOC_CAP};

/// The largest container staged in a stack array; a bigger wire count falls back to
/// a `Vec` (see the module docs). Sized to cover every container in the compatibility
/// corpus many times over while keeping the frame small: a tuple stage is
/// `STACK_STAGE * 8` B = 256 B, a map stage `STACK_STAGE * 16` B = 512 B, and the
/// two stages never live in the same frame (they are in different functions).
const STACK_STAGE: usize = 32;

impl Decoder<'_> {
    /// Decodes `arity` tuple elements and builds the tuple term.
    ///
    /// The arity has already been read and bounded by the caller ([`super`]: a
    /// `SMALL_TUPLE_EXT` arity is a byte, a `LARGE_TUPLE_EXT` arity a `u32` bounded
    /// by `MAX_ARITYVAL`, and an over-long one is `BadArg` — what the oracle
    /// answers). The elements come from the wire in order, so a truncated image
    /// fails on the first short read.
    pub(super) fn decode_tuple(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
        arity: usize,
    ) -> Result<Eterm> {
        if arity <= STACK_STAGE {
            let mut elems = [NIL; STACK_STAGE];
            for slot in &mut elems[..arity] {
                *slot = self.decode(heap, atoms)?;
            }
            heap.tuple(&elems[..arity])
        } else {
            // Pre-sized from the wire arity but clamped to `CONTAINER_PREALLOC_CAP`:
            // a large tuple spends one allocation instead of a chain of `Vec`
            // growths, while an attacker-controlled arity still cannot drive an
            // unbounded allocation (the cap is the bound, and an arity above it just
            // grows the buffer on demand). The reserved slots are never trusted as
            // decoded elements, so a truncated read still fails on the first short
            // read.
            let mut elems = Vec::with_capacity(arity.min(CONTAINER_PREALLOC_CAP));
            for _ in 0..arity {
                elems.push(self.decode(heap, atoms)?);
            }
            heap.tuple(&elems)
        }
    }

    /// Decodes a `LIST_EXT` of `len` elements plus its tail and links the spine.
    ///
    /// The tail is the **next** term on the wire: `NIL` for a proper list, otherwise
    /// the improper tail (the C's `LIST_EXT` arm decodes the tail in place and
    /// prepends the elements). The elements are staged first because the wire order
    /// is head-first while the spine must be built tail-first.
    pub(super) fn decode_list(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
        len: usize,
    ) -> Result<Eterm> {
        if len <= STACK_STAGE {
            let mut elems = [NIL; STACK_STAGE];
            for slot in &mut elems[..len] {
                *slot = self.decode(heap, atoms)?;
            }
            let tail = self.decode(heap, atoms)?;
            crate::term::write_list(heap, &elems[..len], tail)
        } else {
            // Bounded like a tuple (see `decode_tuple`): a claimed length can
            // over-reserve at most `CONTAINER_PREALLOC_CAP`, and the elements still
            // come from the wire one short read at a time.
            let mut elems = Vec::with_capacity(len.min(CONTAINER_PREALLOC_CAP));
            for _ in 0..len {
                elems.push(self.decode(heap, atoms)?);
            }
            let tail = self.decode(heap, atoms)?;
            crate::term::write_list(heap, &elems, tail)
        }
    }

    /// Decodes a `MAP_EXT` of `count` `Key,Value` pairs and builds the map.
    ///
    /// The pair count has already been read and rejected when bit 31 is set (the
    /// sizing pass' `(n >> 31) != 0` guard on 64-bit `ARCH_64` limits a map to
    /// `2^31 - 1` pairs, `external.c:6311-6315`; a larger count is malformed). The
    /// wire order is interleaved `K1,V1,…,Kn,Vn`.
    pub(super) fn decode_map(
        &mut self,
        heap: &mut Heap,
        atoms: &mut AtomTable,
        count: usize,
    ) -> Result<Eterm> {
        if count <= STACK_STAGE {
            let mut pairs = [(NIL, NIL); STACK_STAGE];
            for slot in &mut pairs[..count] {
                let key = self.decode(heap, atoms)?;
                let value = self.decode(heap, atoms)?;
                *slot = (key, value);
            }
            finish_map(heap, atoms, &pairs[..count], count)
        } else {
            // Pre-sized like a tuple (see `decode_tuple`): the pair buffer gets one
            // allocation for a large map instead of a chain of `Vec` growths, while a
            // count the wire only claims can over-reserve at most the cap and a
            // truncated stream still fails on the first short read.
            let mut pairs = Vec::with_capacity(count.min(CONTAINER_PREALLOC_CAP));
            for _ in 0..count {
                let key = self.decode(heap, atoms)?;
                let value = self.decode(heap, atoms)?;
                pairs.push((key, value));
            }
            finish_map(heap, atoms, &pairs, count)
        }
    }
}

/// Builds the canonical map term over `pairs` and rejects a duplicate key.
///
/// ERTS canonicalizes the map here (the `external.c:5438-5456` fixup):
/// `count <= MAP_SMALL_MAP_LIMIT` becomes a FLAT map with keys sorted in
/// `erts_cmp_flatmap_keys` order (`erts_validate_and_sort_flatmap`, `erl_map.c:3187`),
/// a larger count a HAMT (`erts_hashmap_from_array(..., 1)`, `erl_map.c:644`, keyed
/// by `erts_map_hash`). `write_map` is that constructor, so the decoded map carries
/// ERTS's storage order and re-encodes byte-identically (see the crate `CONTEXT.md`,
/// §"Map key encode order").
///
/// `write_map` collapses duplicate keys (last value wins), whereas ERTS **rejects**
/// them in both representations (`external.c:5448` `error_map_fixup`, reached when
/// the fixup's duplicate check fails). `maps:from_list/1` is what collapses, not the
/// codec, so a produced key count below the wire count means a duplicate was present
/// and the image is malformed.
fn finish_map(
    heap: &mut Heap,
    atoms: &mut AtomTable,
    pairs: &[(Eterm, Eterm)],
    count: usize,
) -> Result<Eterm> {
    let map = crate::term::make_map(heap, pairs, atoms)?;
    if beam_term::map::map_size(map, atoms)? != count {
        return Err(crate::error::bad_arg());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    //! The stack-staged and `Vec` paths must build identical terms, and both must
    //! keep the wire rules: an improper tail survives, a duplicate map key is
    //! `BadArg`, and a count above `STACK_STAGE` still decodes.
    use crate::decode::decode_term;
    use crate::term::{cons_head, cons_tail, tuple_arity, tuple_element};
    use beam_alloc::Heap;
    use beam_common::BeamError;
    use beam_term::prelude::{is_list, is_nil, signed_value};
    use beam_term::AtomTable;

    fn heap() -> Heap {
        Heap::new(1024).expect("a small heap")
    }

    fn atoms() -> AtomTable {
        AtomTable::new()
    }

    fn small(value: i64) -> Vec<u8> {
        if (0..=255).contains(&value) {
            vec![97, u8::try_from(value).expect("a byte")]
        } else {
            vec![98, 0, 0, 0, u8::try_from(value).expect("a byte")]
        }
    }

    #[test]
    fn a_list_shorter_than_the_stack_stage_keeps_its_improper_tail() {
        let mut heap = heap();
        let mut atoms = atoms();
        // LIST_EXT, 3 elements (1, 2, 3), improper tail 99.
        let image = [131, 108, 0, 0, 0, 3, 97, 1, 97, 2, 97, 3, 97, 99];
        let term = decode_term(&image, &mut heap, &mut atoms).expect("decodes");
        assert!(is_list(term));
        let mut values = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            values.push(signed_value(cons_head(cursor).expect("a head")));
            cursor = cons_tail(cursor).expect("a tail");
        }
        assert_eq!(values, vec![1, 2, 3]);
        assert_eq!(signed_value(cursor), 99, "the improper tail is the last term");
    }

    #[test]
    fn a_list_longer_than_the_stack_stage_decodes_in_order() {
        let mut heap = heap();
        let mut atoms = atoms();
        let n = 40i64;
        let mut image = vec![131, 108];
        image.extend_from_slice(&(n as u32).to_be_bytes());
        for i in 0..n {
            image.extend_from_slice(&small(i));
        }
        image.push(106);
        let term = decode_term(&image, &mut heap, &mut atoms).expect("decodes");
        let mut values = Vec::new();
        let mut cursor = term;
        while is_list(cursor) {
            values.push(signed_value(cons_head(cursor).expect("a head")));
            cursor = cons_tail(cursor).expect("a tail");
        }
        assert_eq!(values, (0..n).collect::<Vec<_>>());
        assert!(is_nil(cursor));
    }

    #[test]
    fn a_tuple_at_the_stack_stage_boundary_decodes() {
        let mut heap = heap();
        let mut atoms = atoms();
        let image = tuple_image(32, 0);
        let term = decode_term(&image, &mut heap, &mut atoms).expect("decodes");
        let arity = tuple_arity(term).expect("a tuple");
        assert_eq!(arity, 32);
        for index in 0..arity {
            assert_eq!(signed_value(tuple_element(term, index).expect("an element")), 0);
        }
    }

    #[test]
    fn a_tuple_beyond_the_stack_stage_decodes_from_the_vec_path() {
        let mut heap = heap();
        let mut atoms = atoms();
        let image = tuple_image(40, 7);
        let term = decode_term(&image, &mut heap, &mut atoms).expect("decodes");
        let arity = tuple_arity(term).expect("a tuple");
        assert_eq!(arity, 40);
        for index in 0..arity {
            let value = tuple_element(term, index).expect("an element");
            assert_eq!(signed_value(value), if index < 32 { 0 } else { 7 });
        }
    }

    /// `SMALL_TUPLE_EXT` of `arity` elements: the first `STACK_STAGE` are `0`, the
    /// rest `fill` (so both staging paths can be built from one shape).
    fn tuple_image(arity: u8, fill: i64) -> Vec<u8> {
        let mut image = vec![131, 104, arity];
        for index in 0..arity {
            image.extend_from_slice(&small(if index < 32 { 0 } else { fill }));
        }
        image
    }

    #[test]
    fn a_map_at_the_stack_stage_boundary_decodes_and_rejects_duplicates() {
        let mut heap = heap();
        let mut atoms = atoms();
        // MAP_EXT, 32 distinct atom keys `k00`..`k31`.
        let mut image = vec![131, 116, 0, 0, 0, 32];
        for index in 0..32u8 {
            let name = format!("k{index:02}");
            image.push(119);
            image.push(u8::try_from(name.len()).expect("a short name"));
            image.extend_from_slice(name.as_bytes());
            image.push(97);
            image.push(1);
        }
        let term = decode_term(&image, &mut heap, &mut atoms).expect("decodes");
        assert_eq!(beam_term::map::map_size(term, &atoms).expect("a map"), 32);

        // The same key twice is malformed on either staging path.
        for pairs in [2u32, 40] {
            let mut dup = vec![131, 116];
            dup.extend_from_slice(&pairs.to_be_bytes());
            for _ in 0..pairs {
                dup.extend_from_slice(&[119, 1, b'k', 97, 1]);
            }
            let err = decode_term(&dup, &mut heap, &mut atoms).expect_err("a duplicate key");
            assert!(matches!(err, BeamError::BadArg));
        }
    }
}
