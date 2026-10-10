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
//! `thr_id`/marker bits, exactly as the value reconstruction does) and an id that
//! does not fit a 60-bit small is refused by
//! [`crate::term::make_internal_ref`] with a typed error — never a substituted id.

/// `REF_MASK` (`erl_bif_unique.h:115`, `:127`): the 18-bit reference number carried
/// in `num[0]`, bounded by `MAX_REFERENCE` (`1 << _REF_NUM_SIZE`).
pub(crate) const REF_MASK: u32 = (1 << 18) - 1;

/// `REF1_NUM_MASK` (`erl_bif_unique.h:122`, `:131`): the bits of `num[1]` that carry
/// reference-number bits — bits 18..31. Bits 0..15 are the C scheduler `thr_id` and
/// bits 16..17 are markers, which the frozen M1 id does not model (see the module
/// doc).
pub(crate) const REF1_NUM_MASK: u32 = 0xFFFC_0000;

/// The number of 32-bit numbers a canonical `NEWER_REFERENCE_EXT` (`90`) local
/// reference carries (`ERTS_REF_NUMBERS`, `erl_term.h:780`): the C zero-pads a
/// shorter image to this many and uses only the first this many of a longer one.
pub(crate) const REF_NUMBERS: usize = 3;

/// Packs a 60-bit internal-reference `id` into the three wire numbers
/// (`erts_set_ref_numbers`).
///
/// Total: no information of an `id < 2^60` is lost, and `numbers[0]` is always below
/// `REF_MASK` (so the resulting local reference is oracle-valid).
pub(crate) fn ref_id_to_numbers(id: u64) -> [u32; REF_NUMBERS] {
    [
        (id & u64::from(REF_MASK)) as u32,
        (id & u64::from(REF1_NUM_MASK)) as u32,
        (id >> 32) as u32,
    ]
}

/// Unpacks the wire numbers of a reference into a 60-bit internal-reference `id`
/// (`erts_get_ref_numbers_value`).
///
/// Mirrors the C's handling of a non-canonical count: a slice shorter than
/// [`REF_NUMBERS`] is **zero-padded** and a longer one (a decodable 4- or 5-number
/// image) uses only its first [`REF_NUMBERS`] entries. `numbers[0]` is masked to
/// `REF_MASK` and `numbers[1]` to `REF1_NUM_MASK`, dropping the C `thr_id`/marker
/// bits the M1 id does not carry (see the module doc).
pub(crate) fn ref_numbers_to_id(numbers: &[u32]) -> u64 {
    let at = |index: usize| u64::from(numbers.get(index).copied().unwrap_or(0));
    (at(0) & u64::from(REF_MASK))
        | (at(1) & u64::from(REF1_NUM_MASK))
        | (at(2) << 32)
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
        assert_eq!(ref_numbers_to_id(&[1, 2, 3, 0xDEAD_BEEF]), (3u64 << 32) | 2 | 1);
        assert_eq!(
            ref_numbers_to_id(&[1, 2, 3, 4, 5]),
            ref_numbers_to_id(&[1, 2, 3])
        );
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
}
