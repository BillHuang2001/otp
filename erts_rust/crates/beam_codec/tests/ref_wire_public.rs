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

//! The **public** reference wire mapping (`beam_codec::{ref_id_to_numbers,
//! ref_numbers_to_id, representable_ref_id, representable_ref_numbers}` plus the
//! `REF_MASK`/`REF1_NUM_MASK`/`REF_NUMBERS` constants), exercised from *outside* the
//! crate — so a missing re-export fails here, not in a dependent crate.
//!
//! The mapping is the C's `erts_set_ref_numbers` layout
//! (`../../erts/emulator/beam/erl_bif_unique.h:117-131`): `num[0]` = the id's low 18
//! bits, `num[1]` = id bits 18..31 in place, `num[2]` = id bits 32..63, emitted in
//! that order by a `NEWER_REFERENCE_EXT` (`external.c:3664-3667`). Because the
//! codec's decoder demotes a **local** reference back to the internal form by
//! unpacking it, a caller that packs a reference's id differently gets a different
//! id back — the failure mode these tests exist to prevent.
//!
//! Every test is oracle-free; the goldens are written out literally from the C masks.

use beam_alloc::Heap;
use beam_codec::{
    decode_term_with_local_node, internal_ref_id, ref_id_to_numbers, ref_numbers_to_id,
    representable_ref_id, representable_ref_numbers, Encoder, NEWER_REFERENCE_EXT,
    SMALL_ATOM_UTF8_EXT, VERSION_MAGIC, Writer,
};
use beam_common::BeamError;
use beam_term::prelude::AtomTable;

/// The sysname the fixtures use (the sandbox oracle's `node()`).
const LOCAL_NODE: &str = "nonode@nohost";

/// The creation the fixtures use (the oracle's `erlang:system_info(creation)` here).
const CREATION: u32 = 0;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// `SMALL_ATOM_UTF8_EXT` for `name` (the fixture name is short ASCII).
fn utf8_atom(name: &str) -> Vec<u8> {
    assert!(name.len() < 256, "the fixture atom name is short");
    let mut bytes = vec![SMALL_ATOM_UTF8_EXT, name.len() as u8];
    bytes.extend_from_slice(name.as_bytes());
    bytes
}

/// `[131][90][len:2][node atom][creation:4][numbers…]` — a hand-built
/// `NEWER_REFERENCE_EXT` image carrying exactly `numbers`, the numbers a caller that
/// follows this mapping would write.
fn newer_reference_image(node: &str, creation: u32, numbers: &[u32]) -> Vec<u8> {
    let mut writer = Writer::new();
    writer.u8(VERSION_MAGIC);
    writer.u8(NEWER_REFERENCE_EXT);
    writer.u16be(numbers.len() as u16);
    writer.bytes(&utf8_atom(node));
    writer.u32be(creation);
    for &number in numbers {
        writer.u32be(number);
    }
    writer.into_vec()
}

/// The wire numbers of every reference image the coupling test packs.
const COUPLED_IDS: [u64; 4] = [1, 42, 1 << 40, (1 << 33) | 0x8000_0001];

// ---------------------------------------------------------------------------
// 1. The goldens pin the C word order (a swap must fail here)
// ---------------------------------------------------------------------------

/// The three words for a handful of ids, written out literally from the C's masks:
/// `num[0] = id & 0x3FFFF`, `num[1] = id & 0xFFFC0000`, `num[2] = id >> 32`. A pack
/// that swapped any two words — the failure this API exists to prevent — is caught by
/// comparing whole arrays.
#[test]
fn the_public_pack_matches_the_hand_written_c_goldens() {
    let goldens: [(u64, [u32; 3]); 6] = [
        (0, [0, 0, 0]),
        (1, [1, 0, 0]),
        // `0x40000` is the first bit above `REF_MASK` → it lands in `num[1]`.
        (0x4_0000, [0, 0x4_0000, 0]),
        // `2^32` is the first bit of `num[2]`.
        (0x1_0000_0000, [0, 0, 1]),
        (0xFFFF_FFFF, [0x3_FFFF, 0xFFFC_0000, 0]),
        // `0x1234_5678_9ABC`: id bits 32..47 = `0x1234`; id bits 16..31 = `0x5678`
        // (low two bits 0), so bits 18..31 = `0x5678` in `num[1]`'s position; the low
        // 18 bits = `0x9ABC`.
        (0x1234_5678_9ABC, [0x9ABC, 0x5678_0000, 0x1234]),
    ];
    for (id, expected) in goldens {
        assert_eq!(ref_id_to_numbers(id), expected, "pack of {id:#x}");
        assert_eq!(ref_numbers_to_id(&expected), id, "unpack of {id:#x}");
    }
}

// ---------------------------------------------------------------------------
// 2. Boundary ids round-trip; non-representable ids are refused
// ---------------------------------------------------------------------------

/// Every wire-word boundary, the internal-reference slot's edges and the top of the
/// `u64` space survive pack → unpack unchanged; the checked packer refuses an id the
/// internal-reference slot cannot hold (never a truncated id), and the decode-side
/// gate answers `None` for the same id.
#[test]
fn boundary_ids_round_trip_and_a_past_slot_id_is_refused() {
    let max_small = beam_term::prelude::MAX_SMALL as u64;
    for id in [
        0u64,
        u64::from(beam_codec::REF_MASK),
        u64::from(beam_codec::REF_MASK) + 1,
        u64::from(u32::MAX),
        1 << 32,
        max_small,
        i64::MAX as u64,
        u64::MAX,
    ] {
        assert_eq!(ref_numbers_to_id(&ref_id_to_numbers(id)), id, "id = {id:#x}");
    }
    // `MAX_SMALL` (2^59 - 1) is the slot's ceiling and packs; one past it does not.
    assert_eq!(
        representable_ref_numbers(max_small).expect("MAX_SMALL fits the reference id slot"),
        ref_id_to_numbers(max_small)
    );
    for id in [
        max_small + 1,
        1 << 59,
        (1 << 60) - 1,
        i64::MAX as u64,
        u64::MAX,
    ] {
        assert!(
            matches!(
                representable_ref_numbers(id),
                Err(BeamError::SystemLimit)
            ),
            "id {id:#x} must be a typed refusal, not a truncated id"
        );
        assert_eq!(
            representable_ref_id(&ref_id_to_numbers(id)),
            None,
            "id {id:#x} must not be demotable"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. The packing is what the decoder demotes and what the encoder emits
// ---------------------------------------------------------------------------

/// An image built with this packing decodes — with the local node installed — back to
/// an **internal** reference carrying exactly that id, and re-encodes to the very same
/// numbers. A caller that packed the id differently would fail both halves.
#[test]
fn the_packing_is_the_one_the_decoder_demotes_and_the_encoder_emits() {
    for id in COUPLED_IDS {
        let numbers = representable_ref_numbers(id).expect("the fixture ids fit the slot");
        assert_eq!(numbers, ref_id_to_numbers(id), "id = {id:#x}");
        assert_eq!(ref_numbers_to_id(&numbers), id, "id = {id:#x}");

        let image = newer_reference_image(LOCAL_NODE, CREATION, &numbers);
        let mut atoms = AtomTable::new();
        let node = atoms.intern(LOCAL_NODE);
        let mut heap = Heap::new(256).expect("the carrier allocator never fails for these sizes");
        let term = decode_term_with_local_node(&image, &mut heap, &mut atoms, node, CREATION, None)
            .expect("a local reference with a representable id decodes");
        assert_eq!(
            internal_ref_id(term).expect("the local reference demoted to the internal form"),
            id,
            "the decoder must demote the packed id back to {id:#x}"
        );

        let mut encoder = Encoder::new(&atoms).with_local_node(node, CREATION);
        encoder
            .encode(term)
            .expect("the internal reference encodes with a local identity");
        let ours = encoder.finish();
        let mut expected = vec![VERSION_MAGIC, NEWER_REFERENCE_EXT];
        expected.extend_from_slice(&3u16.to_be_bytes());
        expected.extend_from_slice(&utf8_atom(LOCAL_NODE));
        expected.extend_from_slice(&CREATION.to_be_bytes());
        for number in numbers {
            expected.extend_from_slice(&number.to_be_bytes());
        }
        assert_eq!(
            ours, expected,
            "the encoder must emit exactly this packing (id = {id:#x})"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. The prelude re-exports the same mapping
// ---------------------------------------------------------------------------

/// `beam_codec::prelude::*` carries the mapping too, under the same names.
#[test]
fn the_prelude_reexports_the_mapping() {
    mod via_prelude {
        pub use beam_codec::prelude::{ref_id_to_numbers, ref_numbers_to_id, REF_NUMBERS};
    }

    assert_eq!(via_prelude::REF_NUMBERS, 3);
    let numbers = via_prelude::ref_id_to_numbers(0x1234_5678_9ABC);
    assert_eq!(numbers, [0x9ABC, 0x5678_0000, 0x1234]);
    assert_eq!(via_prelude::ref_numbers_to_id(&numbers), 0x1234_5678_9ABC);
}
