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
//! `prim_file:internal_normalize_utf8/1` — the C's
//! `prim_file_internal_normalize_utf8_1` (`erts/emulator/beam/erl_unicode.c:2826`)
//! over `do_utf8_to_list_normalize` (`:1569`), a hand-written port.
//!
//! # What it does (and does not) do
//!
//! It is a **composition-only** pass, and not NFC/NFD/NFKC by any name: there is
//! * no canonical *decomposition*,
//! * no canonical *reordering* (marks are never sorted; `a + diaeresis + acute`
//!   stays three code points, it does not become `a-umlaut + acute`),
//! * no Hangul composition, and
//! * no exclusion *table*: the C's committed table simply has no chain for an
//!   excluded pair, so e.g. `U+0915 U+093C` does not compose — the exclusion is
//!   honoured by omission, which is why the differential oracle agrees.
//!
//! Composition happens only where the committed header
//! (`erts/emulator/beam/erl_unicode_normalize.h`, transcribed into
//! [`table`]) holds an explicit chain. A chain is 2–4 code points, the rightmost
//! (combining) one first; the walk below visits the input **right to left**
//! exactly as the C does, holding at most four code points in a pending window.
//!
//! # Decoding
//!
//! The C validates the whole image first (`erts_analyze_utf8`, `:1177`) and only
//! then walks the bytes by masking, so this port decodes with the crate's
//! existing [`decode_one`] — the same classification: low surrogates and
//! noncharacters are **accepted** and pass through, while a high surrogate, an
//! overlong form, a stray continuation byte and a truncated trailing sequence
//! are all `badarg`. A non-binary argument (and a non-byte-aligned bitstring) is
//! `badarg` too (`erts_get_aligned_binary_bytes`).
//!
//! # An oracle defect this must not reproduce in a test
//!
//! The oracle (OTP 29.1.1 / erts-17.1) **aborts the VM** (SIGABRT) when the
//! *first* code point of the input is a combining mark, a missing write-back in
//! its `cleanup_norm`: `numpoints` is reset on the roll-up path, so the trailing
//! `if (numpoints) cleanup_norm(...)` reads an uninitialised `savepoints[0]`.
//! This port answers the sane thing (the mark passes through) and the case is
//! deliberately absent from every corpus here, since a differential case would
//! compare against a crash rather than a value.

pub(crate) mod table;

use beam_common::{BeamError, Result};

use crate::unicode::{decode_one, Decode};

/// `is_candidate` (`erts/emulator/beam/erl_unicode.c:1418`): may this code point
/// *open* a pending composition group? The C short-circuits below 768 and above
/// 4023 (the two Hangul-jamo-adjacent code points aside) and consults the
/// bitmap in between.
pub(crate) fn is_candidate(cp: u32) -> bool {
    if cp < 768 {
        return false;
    }
    if cp > 4023 {
        return cp == 12441 || cp == 12442;
    }
    let index = (cp / 32 - table::CANDIDATE_MAP_OFFSET) as usize;
    let pos = cp % 32;
    match table::CANDIDATE_MAP.get(index) {
        // A candidate outside the bitmap's span cannot compose: the C would read
        // past the array, but the generator proves the span covers `is_candidate`'s
        // whole non-short-circuited range, so this arm is unreachable and
        // conservative rather than UB-equivalent.
        None => false,
        Some(word) => word & (1u32 << pos) != 0,
    }
}

/// The C's `translate/3` verdict (`:1443`): `TRANSLATE_NO` 0, `TRANSLATE_MAYBE`
/// -1, or a positive "a chain ended here".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Translate {
    /// No node on this path: the C's `x < 0` from `hashsearch`.
    No,
    /// A live prefix that needs one more code point (`pos == slen`).
    Maybe,
    /// A chain matched at depth `pos`, composing to `result`. `pos` is never `0`
    /// (the table has no single-code-point chain, and the C's `pos == 0` return
    /// would be indistinguishable from `TRANSLATE_NO`).
    Composed { result: u32, pos: usize },
}

/// Pack a reversed chain prefix the way [`table::CHAINS`] is packed: `s[0]` (the
/// combining mark) in the lowest 16 bits, zero in every unused slot.
fn pack(s: &[u16]) -> u64 {
    s.iter()
        .rev()
        .fold(0u64, |acc, &c| (acc << 16) | u64::from(c))
}

/// `translate/3` (`:1443`) over the flattened tables: walk `s` from the mark
/// outwards and answer the first node that ends a chain, or [`Translate::Maybe`]
/// when the whole of `s` is a live prefix, or [`Translate::No`] when the path
/// dies.
fn translate(s: &[u16]) -> Translate {
    for pos in 0..s.len() {
        let key = pack(&s[..=pos]);
        if let Ok(i) = table::CHAINS.binary_search_by_key(&key, |&(k, _)| k) {
            let (_, result) = table::CHAINS[i];
            // `pos == 0` is the C's `return 0`, which its callers read as
            // `TRANSLATE_NO`; no such row exists, so report it as `No` rather than
            // invent a `Composed` with a zero depth.
            return if pos == 0 {
                Translate::No
            } else {
                Translate::Composed {
                    result,
                    pos,
                }
            };
        }
        if table::CHAIN_PREFIXES.binary_search(&key).is_err() {
            return Translate::No;
        }
    }
    Translate::Maybe
}

/// The C's `savepoints[4]` window plus the code points emitted so far, in the
/// C's own (right-to-left) building order.
struct Norm {
    out: Vec<u32>,
    savepoints: [u16; 4],
    numpoints: usize,
}

impl Norm {
    fn new(capacity: usize) -> Self {
        Norm {
            out: Vec::with_capacity(capacity),
            savepoints: [0; 4],
            numpoints: 0,
        }
    }

    /// The C `CONS`es onto an accumulator; this port pushes and reverses once, so
    /// [`Norm::finish`] hands back the forward-order list.
    fn emit(&mut self, cp: u32) {
        self.out.push(cp);
    }

    /// `handle_first_norm` (`:1468`).
    fn handle_first_norm(&mut self, cp: u32) {
        self.numpoints = 1;
        self.savepoints[0] = cp as u16;
    }

    /// The tail of both roll-up loops — `savepoints[0]` plus the `i = 1..`
    /// walk — with the caller's comparison for the non-composing case. The C
    /// spells this twice (once in `cleanup_norm` with `<= 0`, once in the
    /// `TRANSLATE_NO` arm of `handle_potential_norm` with `== 0` plus a
    /// "maybe" branch), so the difference is a parameter here.
    fn roll_up(&mut self, maybe_reopens: bool) {
        let numpoints = self.numpoints;
        self.emit(u32::from(self.savepoints[0]));
        let mut i = 1;
        while i < numpoints {
            let rest = &self.savepoints[i..numpoints];
            let verdict = if is_candidate(u32::from(self.savepoints[i])) {
                Some(translate(rest))
            } else {
                None
            };
            match verdict {
                Some(Translate::Composed { result, pos }) if pos > 0 => {
                    self.emit(result);
                    i += pos;
                }
                Some(Translate::Maybe) if maybe_reopens => {
                    // "A 'maybe', means we are not done yet": compact the live
                    // prefix down to slot 0 and keep it pending (`goto breakaway`).
                    let mut j = 0;
                    while i < numpoints {
                        self.savepoints[j] = self.savepoints[i];
                        j += 1;
                        i += 1;
                    }
                    self.numpoints = j;
                    return;
                }
                _ => {
                    self.emit(u32::from(self.savepoints[i]));
                    i += 1;
                }
            }
        }
        self.numpoints = 0;
    }

    /// `cleanup_norm` (`:1475`) — emitted once the input is exhausted with a
    /// pending window.
    fn cleanup_norm(&mut self) {
        self.roll_up(false);
    }

    /// `handle_potential_norm` (`:1500`).
    fn handle_potential_norm(&mut self, cp: u32) {
        if cp >> 16 == 0 {
            // `savepoints` is `Uint16`, so only a BMP code point can extend a
            // group. The window can never need a fifth slot: `translate` answers
            // `Maybe` only for a *proper* prefix of a chain and no chain is longer
            // than four code points (the generator refuses a deeper one).
            debug_assert!(self.numpoints < self.savepoints.len());
            if self.numpoints >= self.savepoints.len() {
                self.roll_up(false);
                self.emit(cp);
                return;
            }
            self.savepoints[self.numpoints] = cp as u16;
            self.numpoints += 1;
            match translate(&self.savepoints[..self.numpoints]) {
                Translate::No => self.roll_up(true),
                Translate::Composed { result, pos } if pos > 0 => {
                    self.numpoints = 0;
                    self.emit(result);
                }
                // A "maybe" (or, unreachably, a zero-depth match): go on.
                _ => {}
            }
            return;
        }
        // "Unconditional rollup, this character is larger than 16 bit": an astral
        // code point cannot enter the window, so the pending group closes, the
        // code point is emitted, and the window restarts empty.
        self.roll_up(false);
        self.emit(cp);
        self.numpoints = 0;
    }

    fn finish(mut self) -> Vec<u32> {
        self.out.reverse();
        self.out
    }
}

/// `do_utf8_to_list_normalize` (`:1569`) over already-decoded code points, walked
/// right to left as the C walks the bytes.
pub(crate) fn normalize(code_points: &[u32]) -> Vec<u32> {
    let mut norm = Norm::new(code_points.len());
    for &cp in code_points.iter().rev() {
        if norm.numpoints != 0 {
            norm.handle_potential_norm(cp);
        } else if is_candidate(cp) {
            norm.handle_first_norm(cp);
        } else {
            norm.emit(cp);
        }
    }
    if norm.numpoints != 0 {
        norm.cleanup_norm();
    }
    norm.finish()
}

/// Decode + normalize a UTF-8 image, `badarg` for anything `erts_analyze_utf8`
/// rejects (`:1177`).
pub(crate) fn normalize_utf8(bytes: &[u8]) -> Result<Vec<u32>> {
    let mut points = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match decode_one(&bytes[i..]) {
            Decode::Ok(cp, len) => {
                points.push(cp);
                i += len;
            }
            // `ERTS_UTF8_ERROR` and `ERTS_UTF8_INCOMPLETE` are both `BADARG` in the
            // C BIF (it tests `!= ERTS_UTF8_OK`).
            Decode::Invalid | Decode::Incomplete(_) => return Err(BeamError::BadArg),
        }
    }
    Ok(normalize(&points))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The BIF's observable, as the oracle prints it: a plain list of code points.
    fn norm(bytes: &[u8]) -> Result<Vec<u32>> {
        normalize_utf8(bytes)
    }

    fn norm_of(points: &[u32]) -> Vec<u32> {
        normalize(points)
    }

    #[test]
    fn the_candidate_gate_matches_the_c_short_circuits() {
        // Below 768 the C answers 0 without touching the bitmap.
        assert!(!is_candidate(0x41));
        assert!(!is_candidate(767));
        // Above 4023 only the two Hangul-jamo-adjacent points are candidates.
        assert!(is_candidate(12441));
        assert!(is_candidate(12442));
        assert!(!is_candidate(4024));
        assert!(!is_candidate(0x1F600));
        // In range, the bitmap decides: U+0301 (769) is a candidate mark and
        // U+0041 (65) is out of range, so 768 itself is the bitmap's first bit's
        // neighbour at index 0.
        assert!(is_candidate(769));
        assert!(is_candidate(778));
        // U+0361 (865) is a combining mark the table uses; U+03E8 (1000) is not.
        assert!(!is_candidate(0x3E8));
    }

    #[test]
    fn the_packed_key_puts_the_combining_mark_in_the_low_bits() {
        // Chain [mark, base] is packed with the mark lowest, so numeric order is
        // lexicographic by the C's own right-to-left walk.
        assert_eq!(pack(&[769, 65]), (65 << 16) | 769);
        assert_eq!(pack(&[769]), 769);
        assert_eq!(pack(&[769, 778, 65]), (65 << 16 << 16) | (778 << 16) | 769);
    }

    #[test]
    fn translate_answers_no_maybe_and_composed() {
        // No path at all.
        assert_eq!(translate(&[769, 66]), Translate::No);
        // A live prefix: acute+ring is a proper prefix of acute+ring+A.
        assert_eq!(translate(&[769, 778]), Translate::Maybe);
        // A full chain.
        assert_eq!(
            translate(&[769, 778, 65]),
            Translate::Composed {
                result: 506,
                pos: 2
            }
        );
    }

    #[test]
    fn an_empty_image_is_the_empty_list() {
        assert_eq!(norm(b"").unwrap(), Vec::<u32>::new());
    }

    #[test]
    fn a_lone_ascii_byte_passes_through() {
        assert_eq!(norm(b"A").unwrap(), vec![65]);
    }

    /// A + U+0301 composes to Á (U+00C1) — the oracle's `[193]`.
    #[test]
    fn a_base_plus_a_combining_mark_composes() {
        assert_eq!(norm(b"A\xCC\x81").unwrap(), vec![193]);
        assert_eq!(norm_of(&[101, 769]), vec![233]);
    }

    /// A + ring + acute is a three-code-point chain: the oracle's `[506]`.
    #[test]
    fn a_three_code_point_chain_composes() {
        assert_eq!(norm_of(&[65, 778, 769]), vec![506]);
        assert_eq!(norm_of(&[97, 770, 769]), vec![7845]);
        assert_eq!(norm_of(&[65, 770, 769]), vec![7844]);
    }

    /// The same three code points in the other order do NOT compose — no
    /// canonical reordering happens — so the oracle answers `[65, 1, 10]`.
    #[test]
    fn marks_are_never_reordered() {
        assert_eq!(norm_of(&[65, 769, 778]), vec![65, 1, 10]);
        assert_eq!(norm_of(&[97, 776, 769]), vec![97, 8, 1]);
        assert_eq!(norm_of(&[97, 769, 776]), vec![97, 1, 8]);
    }

    /// A pair with no chain in the committed table stays two code points. The
    /// composition exclusions are honoured by omission (the C table has no row),
    /// not by an exclusion test: `U+0915 U+093C` is the classic excluded pair.
    #[test]
    fn a_non_composing_pair_stays_apart() {
        assert_eq!(norm_of(&[0x0915, 0x093C]), vec![0x0915, 0x093C]);
        assert_eq!(norm_of(&[2328, 2364]), vec![2328, 2364]);
        assert_eq!(norm_of(&[3953, 3954]), vec![3953, 3954]);
    }

    /// Only the chain the table holds composes: `i + circumflex + acute` has no
    /// three-code-point row (the oracle answers `[105, 238, 769]`), while
    /// `i + diaeresis + acute` does (`[7727]`).
    #[test]
    fn only_the_chains_the_table_holds_compose() {
        assert_eq!(norm_of(&[105, 770, 769]), vec![105, 238, 769]);
        assert_eq!(norm_of(&[117, 770, 769]), vec![117, 251, 769]);
        assert_eq!(norm_of(&[105, 776, 769]), vec![7727]);
        assert_eq!(norm_of(&[105, 769, 776]), vec![237, 776]);
    }

    /// A pending group that cannot extend is rolled up and the following base
    /// starts over: `[193, 769]`, `[192, 769]`, `[224, 769]`.
    #[test]
    fn a_dead_end_rolls_the_group_up() {
        assert_eq!(norm_of(&[65, 769, 769]), vec![193, 769]);
        assert_eq!(norm_of(&[65, 769, 770]), vec![193, 770]);
        assert_eq!(norm_of(&[65, 768, 769]), vec![192, 769]);
        assert_eq!(norm_of(&[97, 768, 769]), vec![224, 769]);
        assert_eq!(norm_of(&[65, 768, 770, 769]), vec![192, 770, 769]);
    }

    /// An astral code point cannot enter the C's `Uint16 savepoints` window, so it
    /// forces an unconditional roll-up (`unipoint >> 16 != 0`).
    #[test]
    fn an_astral_code_point_forces_a_roll_up() {
        assert_eq!(norm_of(&[0x1F600]), vec![0x1F600]);
        assert_eq!(norm_of(&[65, 769, 0x1F600]), vec![193, 0x1F600]);
        assert_eq!(norm_of(&[65, 0x1F600, 769]), vec![65, 0x1F600, 769]);
    }

    /// Low surrogates and noncharacters decode and pass through; a high surrogate,
    /// an overlong form, a stray continuation byte and a truncated tail are
    /// `badarg`, exactly as `erts_analyze_utf8` classifies them.
    #[test]
    fn the_decoder_classifies_exactly_like_analyze_utf8() {
        // U+DC00 (low surrogate) and U+FDD0 / U+FFFF (noncharacters) are accepted.
        assert_eq!(norm(b"\xED\xB0\x80").unwrap(), vec![0xDC00]);
        assert_eq!(norm("\u{FDD0}".as_bytes()).unwrap(), vec![0xFDD0]);
        assert_eq!(norm("\u{FFFF}".as_bytes()).unwrap(), vec![0xFFFF]);
        // U+D800 (high surrogate).
        assert!(matches!(norm(b"\xED\xA0\x80"), Err(BeamError::BadArg)));
        // Overlong: a two-byte form of 'A', and the three/four-byte forms.
        assert!(matches!(norm(b"\xC1\x81"), Err(BeamError::BadArg)));
        assert!(matches!(norm(b"\xE0\x80\x80"), Err(BeamError::BadArg)));
        assert!(matches!(norm(b"\xF0\x80\x80\x80"), Err(BeamError::BadArg)));
        // A stray continuation byte, and a truncated tail.
        assert!(matches!(norm(b"\x80"), Err(BeamError::BadArg)));
        assert!(matches!(norm(&"A\u{0301}".as_bytes()[..2]), Err(BeamError::BadArg)));
        assert!(matches!(norm(b"\xC3"), Err(BeamError::BadArg)));
    }
}
