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

//! The **reference wire mapping**: this runtime's single 60-bit internal-reference
//! id ↔ the three 32-bit numbers a `REFERENCE_EXT` / `NEW_REFERENCE_EXT` /
//! `NEWER_REFERENCE_EXT` image carries.
//!
//! The C reference identity is spread across three `Uint32` numbers, packed and
//! unpacked by `erts_set_ref_numbers` / `erts_get_ref_numbers_value`
//! (`../../erts/emulator/beam/erl_bif_unique.h:115-134`, `:159`):
//!
//! ```text
//! REF_MASK      = (1 << 18) - 1 = 0x3FFFF   // num[0]: the 18-bit reference number
//! REF1_NUM_MASK = 0xFFFC_0000               // num[1] bits 18..31 (bits 0..15 are the
//!                                           //   C scheduler thr_id, 16..17 markers)
//! num[2]        = value >> 32
//! ```
//!
//! This runtime's frozen M1 internal-reference thing stores one 60-bit `id` (the C
//! `value`) in a single word ([`beam_term::boxed::write_internal_ref`]), so this
//! module is the *single* place that maps that id to and from the wire numbers — the
//! decoder's local-node demotion and the encoder's internal-reference branch both go
//! through it (Prevent-Duplication).
//!
//! ## Why this is exact for our own references, and only approximate for foreign ones
//!
//! The pack is loss-free for any `u64` (bits 0..17 → `num[0]`, 18..31 → `num[1]`,
//! 32..63 → `num[2]`), and `num[0]` is kept below `REF_MASK` so the emitted local
//! reference is oracle-valid; only an id that does not fit the 60-bit small the M1
//! reference stores is refused on construction. A round trip of a reference **this** runtime
//! produced (`decode(encode(R))`) therefore restores `R` exactly, and the emitted
//! bytes match the oracle byte-for-byte whenever the oracle's own `num[1]` carries a
//! zero `thr_id` (bits 0..15) and zero markers (bits 16..17).
//!
//! An **arbitrary** oracle `make_ref()` sets a nonzero scheduler `thr_id` in those
//! low bits of `num[1]`, plus a 30-bit `num[1]` field — identity the frozen 60-bit
//! M1 id has no room for. Such a reference **cannot** round-trip byte-exactly and is
//! not faked here: the unpack masks `num[1]` down to `REF1_NUM_MASK` (dropping the
//! `thr_id`/marker bits, exactly as the value reconstruction does).
//!
//! A wire image whose packed id does **not** fit the 60-bit small the M1 id slot
//! holds (a `num[2] >= 2^27`, i.e. essentially every oracle-minted `#Ref<…>`) is
//! therefore **not** demoted to the internal form: [`representable_ref_id`] reports
//! `None` for it and the decoder keeps the faithful **external** reference form
//! (which already round-trips byte-exactly), rather than refusing the image with an
//! error or substituting a truncated id.
//!
//! ## Public API — the crate's single authority for this mapping
//!
//! [`ref_id_to_numbers`] and [`ref_numbers_to_id`] are part of the crate's public
//! surface (re-exported at the crate root and in [`crate::prelude`]): a crate that
//! writes or reads a wire reference calls them instead of mirroring the bit layout
//! (crate-layer `CONTEXT.md`, Prevent-Duplication).
//!
//! A caller writing a reference's wire image **must** use exactly this packing. The
//! counterpart's [`Decoder`](crate::Decoder) demotes a *local* reference back to the
//! internal form by unpacking it with [`ref_numbers_to_id`], so any other bit layout
//! makes the same reference come back as a **different** id.
//!
//! The pack is total over `u64` — every bit survives, exactly as the C pack is total
//! — but only an id the internal-reference slot can hold
//! ([`beam_term::prelude::MAX_SMALL`], `2^59 - 1`) can be demoted back. For that
//! requirement use the checked form [`representable_ref_numbers`], which refuses a
//! larger id with the typed [`BeamError::SystemLimit`](beam_common::BeamError)
//! instead of writing one that can never come back as an internal reference. No
//! function here truncates an id.

/// `REF_MASK` (`erl_term.h:772`, `erl_bif_unique.h:115`, `:127`): the 18-bit
/// reference number carried in `num[0]`, bounded by `MAX_REFERENCE`
/// (`1 << _REF_NUM_SIZE`).
pub const REF_MASK: u32 = (1 << 18) - 1;

/// `REF1_NUM_MASK` (`ERTS_REF1_NUM_MASK__`, `erl_bif_unique.h:112`, `:128`): the
/// bits of `num[1]` that carry reference-number bits — bits 18..31. Bits 0..15 are
/// the C scheduler `thr_id` and bits 16..17 are markers, which the frozen M1 id does
/// not model (see the module doc).
pub const REF1_NUM_MASK: u32 = 0xFFFC_0000;

/// The number of 32-bit numbers a canonical `NEWER_REFERENCE_EXT` (`90`) local
/// reference carries (`ERTS_REF_NUMBERS`, `erl_term.h:773`): the C zero-pads a
/// shorter image to this many and uses only the first this many of a longer one.
pub const REF_NUMBERS: usize = 3;

/// Packs a 60-bit internal-reference `id` into the three wire numbers in wire order
/// — the C's `erts_set_ref_numbers` pack
/// (`../../erts/emulator/beam/erl_bif_unique.h:117-131`) with a zero scheduler
/// `thr_id`, over the masks `REF_MASK` / `ERTS_REF1_NUM_MASK__`
/// (`../../erts/emulator/beam/erl_term.h:768-773`,
/// `../../erts/emulator/beam/erl_bif_unique.h:102-113`):
///
/// ```text
/// numbers[0] = id & 0x0003_FFFF   // the 18-bit reference number  (REF_MASK)
/// numbers[1] = id & 0xFFFC_0000   // id bits 18..31; the C ORs in thr_id + markers
/// numbers[2] = id >> 32           // id bits 32..63
/// ```
///
/// The array order **is** the wire order: a `NEWER_REFERENCE_EXT` (`90`) image emits
/// `numbers[0]`, `numbers[1]`, `numbers[2]` in that sequence after the node atom and
/// creation (`../../erts/emulator/beam/external.c:3664-3667`), so a caller building a
/// reference image must serialize them in this order too.
///
/// Total over `u64`: no bit of the id is lost, and `numbers[0]` is always below
/// `MAX_REFERENCE` (`1 << 18`), so the resulting local reference is oracle-valid
/// (`external.c:4807`). Totality does **not** mean every id can come back as an
/// internal reference — see [`representable_ref_numbers`] for the checked form and
/// [`representable_ref_id`] for the matching decode-side gate.
pub fn ref_id_to_numbers(id: u64) -> [u32; REF_NUMBERS] {
    [
        (id & u64::from(REF_MASK)) as u32,
        (id & u64::from(REF1_NUM_MASK)) as u32,
        (id >> 32) as u32,
    ]
}

/// Unpacks the wire numbers of a reference into a 60-bit internal-reference `id` —
/// the C's `erts_get_ref_numbers_value`
/// (`../../erts/emulator/beam/erl_bif_unique.h:158-167`).
///
/// Mirrors the C's handling of a non-canonical count: a slice shorter than
/// [`REF_NUMBERS`] is **zero-padded** and a longer one (a decodable 4- or 5-number
/// image) uses only its first [`REF_NUMBERS`] entries
/// (`../../erts/emulator/beam/external.c:4821-4839`) — so the id of a legacy
/// one-number `REFERENCE_EXT` is just that number. `numbers[0]` is masked to
/// [`REF_MASK`] and `numbers[1]` to [`REF1_NUM_MASK`], dropping the C
/// `thr_id`/marker bits the M1 id does not carry (see the module doc).
///
/// Exact for any number list: the mapping never truncates, and pairing it with
/// [`ref_id_to_numbers`] is the identity over all of `u64`. Whether the resulting id
/// can materialize as an *internal* reference is [`representable_ref_id`]'s answer,
/// not this function's.
pub fn ref_numbers_to_id(numbers: &[u32]) -> u64 {
    let at = |index: usize| u64::from(numbers.get(index).copied().unwrap_or(0));
    (at(0) & u64::from(REF_MASK))
        | (at(1) & u64::from(REF1_NUM_MASK))
        | (at(2) << 32)
}

/// The packed internal-reference `id` of a wire reference image, **when it fits**
/// the single small-integer word the M1 internal-reference thing stores
/// ([`beam_term::prelude::MAX_SMALL`]); `None` when it does not.
///
/// The decoder's local-node demotion is gated on this: a local reference whose
/// packed id is representable materializes as the internal form, otherwise the
/// faithful **external** reference form is kept (see the module doc). Mirroring the
/// small-int range check here — rather than hard-coding a width — keeps the id slot
/// a [`beam_term`] layout decision.
///
/// Never truncates: the id is either the exact pack of `numbers` or absent. `None`
/// is the typed refusal — a caller that needs an internal reference must not
/// substitute a truncated id, and a caller reading a wire image stays on the external
/// form.
pub fn representable_ref_id(numbers: &[u32]) -> Option<u64> {
    let id = ref_numbers_to_id(numbers);
    (id <= beam_term::prelude::MAX_SMALL as u64).then_some(id)
}

/// The **checked** form of [`ref_id_to_numbers`]: the three wire numbers of `id`
/// **when the internal-reference slot can hold it**, else the typed
/// [`BeamError::SystemLimit`](beam_common::BeamError).
///
/// Use this when the reference being written must come back as an *internal*
/// reference on the counterpart: it refuses (`Err`) an id above
/// [`beam_term::prelude::MAX_SMALL`] (`2^59 - 1`) instead of putting an id on the
/// wire that [`representable_ref_id`] would answer `None` for. It never truncates.
///
/// [`ref_id_to_numbers`] stays total because the C pack is total and an *external*
/// reference round-trips any `u64` byte-exactly; the representation limit is the M1
/// internal-reference id slot's, not the wire's.
pub fn representable_ref_numbers(id: u64) -> beam_common::Result<[u32; REF_NUMBERS]> {
    if id > beam_term::prelude::MAX_SMALL as u64 {
        return Err(crate::error::system_limit());
    }
    Ok(ref_id_to_numbers(id))
}

#[cfg(test)]
mod tests {
    //! The pack/unpack are mutual inverses over the whole 60-bit id space, and the
    //! unpack mirrors the C's count handling (pad short, truncate long).

    use super::*;

    #[test]
    fn round_trips_across_id_magnitudes() {
        for id in [
            0u64,
            1,
            42,
            1 << 17,
            1 << 18,
            1 << 31,
            1 << 32,
            1 << 40, // ETS table-handle scale
            1 << 45,
            (1u64 << 59) - 1, // the largest 60-bit small
        ] {
            assert_eq!(ref_numbers_to_id(&ref_id_to_numbers(id)), id, "id = {id}");
        }
    }

    #[test]
    fn first_number_stays_below_max_reference() {
        // Whatever the id, `numbers[0]` must stay below `MAX_REFERENCE` (`1 << 18`)
        // so an emitted local reference is valid (`external.c:4807`).
        for id in [0u64, REF_MASK as u64, (1u64 << 59) - 1, u64::MAX] {
            assert!(ref_id_to_numbers(id)[0] < (1 << 18), "id = {id}");
        }
    }

    #[test]
    fn a_short_number_list_is_zero_padded() {
        // A legacy `REFERENCE_EXT` carries one number (`r0`); the C zero-pads to
        // three (`external.c:4832-4839`), so `[42]` is the id `42`.
        assert_eq!(ref_numbers_to_id(&[42]), 42);
        assert_eq!(ref_numbers_to_id(&[]), 0);
        // Two numbers use only the first two slots.
        assert_eq!(ref_numbers_to_id(&[1, 1 << 18]), (1 << 18) | 1);
    }

    #[test]
    fn a_long_number_list_uses_the_first_three() {
        // A 4/5-number image contributes only its first three numbers
        // (`external.c:4821-4830`).
        assert_eq!(
            ref_numbers_to_id(&[1, 2, 3, 4, 5]),
            ref_numbers_to_id(&[1, 2, 3])
        );
        assert_eq!(ref_numbers_to_id(&[1, 2, 3, 0xDEAD_BEEF]), 1 | (3u64 << 32));
        // …and nothing from the ignored tail leaks into the id.
        assert_eq!(ref_numbers_to_id(&[0, 0, 0, u32::MAX, u32::MAX]), 0);
    }

    #[test]
    fn the_thr_id_and_marker_bits_are_dropped() {
        // `num[1]` low 18 bits (`thr_id` + markers) are not part of the M1 id.
        assert_eq!(ref_numbers_to_id(&[0, 0xFFFF]), 0);
        assert_eq!(ref_numbers_to_id(&[0, 0x0003_FFFF]), 0);
        // …but bits 18..31 survive in place.
        assert_eq!(ref_numbers_to_id(&[0, 0x0004_0000]), 1 << 18);
    }

    #[test]
    fn pack_drops_nothing_up_to_sixty_bits() {
        // Every 60-bit id is reconstructed exactly, including the boundary.
        let max = (1u64 << 60) - 1;
        assert_eq!(ref_numbers_to_id(&ref_id_to_numbers(max)), max);
    }

    #[test]
    fn representable_ref_id_admits_exactly_the_small_range() {
        // The largest representable id — `MAX_SMALL` — is admitted, and its own wire
        // numbers pack back to it.
        let max = beam_term::prelude::MAX_SMALL as u64;
        assert_eq!(representable_ref_id(&ref_id_to_numbers(max)), Some(max));
        assert_eq!(representable_ref_id(&ref_id_to_numbers(0)), Some(0));
        assert_eq!(
            representable_ref_id(&ref_id_to_numbers(1 << 40)),
            Some(1 << 40)
        );
        // One past the small range is **not** representable — no truncation.
        assert_eq!(
            representable_ref_id(&ref_id_to_numbers(beam_term::prelude::MAX_SMALL as u64 + 1)),
            None
        );
        // A wire image whose `num[2] >= 2^27` cannot be a small: the packed id's top
        // 32 bits already exceed the small range.
        assert_eq!(representable_ref_id(&[0, 0, 1 << 27]), None);
        assert_eq!(
            representable_ref_id(&[0, 0, (1 << 27) - 1]),
            Some((1u64 << 59) - (1u64 << 32))
        );
        // The zero-padding of a short image keeps `[r0]` representable (`r0` is
        // bounded by `MAX_REFERENCE` on the wire).
        assert_eq!(representable_ref_id(&[REF_MASK]), Some(u64::from(REF_MASK)));
    }

    #[test]
    fn every_boundary_id_round_trips_through_the_public_pair() {
        // The edge of each wire word, the internal-reference slot's edges and the top
        // of the `u64` space: the public pack/unpack pair is the identity over all of
        // it (the pack is total, so even an id no internal reference can hold survives
        // the wire exactly).
        let max_small = beam_term::prelude::MAX_SMALL as u64;
        for id in [
            0u64,                    // the zero id
            u64::from(REF_MASK),     // 0x3FFFF — the `num[0]` ceiling
            u64::from(REF_MASK) + 1, // 0x40000 — the first `num[1]` bit
            u64::from(u32::MAX),     // every `num[0]` + `num[1]` bit set
            1 << 32,                 // the first `num[2]` bit
            (1 << 33) | 0x8000_0001, // `num[2]`, `num[1]`'s top bit and `num[0]` together
            max_small,               // the largest internal-reference id
            i64::MAX as u64,         // past the slot, still exact on the wire
            u64::MAX,                // the top of the `u64` space
        ] {
            assert_eq!(ref_numbers_to_id(&ref_id_to_numbers(id)), id, "id = {id:#x}");
        }
    }

    #[test]
    fn the_golden_packs_pin_the_c_word_order() {
        // Hand-derived from the C masks (`erts_set_ref_numbers`,
        // `erl_bif_unique.h:127-130`) and written out literally, so a pack that swaps
        // `numbers[1]`/`numbers[2]` or takes `num[0]` from the wrong bits fails here:
        // `num[0]` is the low 18 bits, `num[1]` holds id bits 18..31 **in place**,
        // `num[2]` holds id bits 32..63.
        let goldens: [(u64, [u32; REF_NUMBERS]); 6] = [
            (0, [0, 0, 0]),
            (1, [1, 0, 0]),
            (0x4_0000, [0, 0x4_0000, 0]),
            (0x1_0000_0000, [0, 0, 1]),
            (0xFFFF_FFFF, [0x3_FFFF, 0xFFFC_0000, 0]),
            // `0x1234_5678_9ABC`: id bits 32..47 are `0x1234`; id bits 16..31 are
            // `0x5678`, whose low two bits are 0, so bits 18..31 are `0x5678` in
            // `num[1]`'s position (`0x5678 << 18`); the low 18 bits are `0x9ABC`.
            (0x1234_5678_9ABC, [0x9ABC, 0x5678_0000, 0x1234]),
        ];
        for (id, expected) in goldens {
            assert_eq!(ref_id_to_numbers(id), expected, "pack of {id:#x}");
            assert_eq!(ref_numbers_to_id(&expected), id, "unpack of {id:#x}");
        }
        // The largest internal-reference id (`2^59 - 1`): `num[2]` is its bits 32..59.
        let max_small = beam_term::prelude::MAX_SMALL as u64;
        assert_eq!(
            ref_id_to_numbers(max_small),
            [0x3_FFFF, 0xFFFC_0000, 0x07FF_FFFF]
        );
    }

    #[test]
    fn an_id_past_the_internal_slot_is_a_typed_refusal() {
        let max_small = beam_term::prelude::MAX_SMALL as u64;
        // The slot's ceiling is representable…
        assert_eq!(
            representable_ref_numbers(max_small).expect("MAX_SMALL fits the id slot"),
            ref_id_to_numbers(max_small)
        );
        assert_eq!(representable_ref_id(&ref_id_to_numbers(max_small)), Some(max_small));
        // …one past it is refused by the checked packer with the crate's typed
        // resource-limit error, and the decode-side gate answers `None` for the same
        // id — so neither direction ever substitutes a truncated id.
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
                    Err(beam_common::BeamError::SystemLimit)
                ),
                "id {id:#x} must be refused, not truncated"
            );
            assert_eq!(
                representable_ref_id(&ref_id_to_numbers(id)),
                None,
                "id {id:#x} must not be demotable"
            );
        }
    }
}
