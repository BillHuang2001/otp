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

//! The UTF-16 / UTF-32 codecs of the `unicode` module.
//!
//! Split out of [`super`] (which owns the `latin1`/`utf8` engines) because the
//! wide encodings are a self-contained pair: a "one code point at the head"
//! decoder plus the `cbv/2` *prefix length* probe, and the matching encoder.
//!
//! Ground truth (read-only): `../../lib/stdlib/src/unicode.erl` — `do_i_utf16_big`
//! / `do_i_utf16_little` / `do_i_utf32_big` / `do_i_utf32_little` (and their
//! `_chk` variants), `cbv/2`'s four wide clauses (`unicode.erl:957-996`) and
//! `o_trans/1`'s `<<One/utf16>>` / `<<One/utf16-little>>` / `<<One/utf32>>` /
//! `<<One/utf32-little>>` builders. The `unicode:characters_to_binary/2` BIF
//! **traps** to `unicode:characters_to_binary_int/2` for these encodings
//! (`erl_unicode.c:948-954`), so the Erlang engine — not a C helper — is the
//! authority for them; `unicode.erl` is therefore the specification and the
//! oracle-pinned tests below are its transcript.
//!
//! All of this is safe Rust: the decoders only inspect a byte slice and the
//! encoders only build a `Vec<u8>`.

/// One decode step at the head of a wide-encoded binary — the Erlang
/// `<<U/utf16>>` / `<<U/utf32>>` binary-pattern match, with the `cbv/2` fallback
/// when it fails.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Step {
    /// A code point and the number of bytes it occupied.
    Ok(u32, usize),
    /// Not a valid sequence (a lone surrogate, an out-of-range value, or bytes
    /// that are not a prefix of one either) — the Erlang `cbv/2` returning
    /// `false`.
    Invalid,
    /// A valid *prefix*; the payload is the number of bytes still needed
    /// (`cbv/2` returning an integer).
    Incomplete(usize),
}

/// The 16-bit code unit at the head of `bytes` (which must hold at least two).
fn unit_at(bytes: &[u8], little: bool) -> u32 {
    if little {
        u32::from(bytes[0]) | (u32::from(bytes[1]) << 8)
    } else {
        (u32::from(bytes[0]) << 8) | u32::from(bytes[1])
    }
}

/// `true` for the `U+D800..=U+DBFF` high-surrogate code units.
fn is_high(u: u32) -> bool {
    (0xD800..=0xDBFF).contains(&u)
}

/// `true` for the `U+DC00..=U+DFFF` low-surrogate code units.
fn is_low(u: u32) -> bool {
    (0xDC00..=0xDFFF).contains(&u)
}

/// `cbv({utf16,big}, Bin)` — how many more bytes `Bin` needs to complete a code
/// point, or `None` for the Erlang `false`.
///
/// The four clauses are tried in order and each is exact about `Bin`'s size:
/// one byte whose high six bits are not `54` needs one more, a leading `54:6`
/// (i.e. `0xD8..=0xDB`) needs three/two/one depending on how much of the pair is
/// present (`unicode.erl:957-966`).
fn cbv_utf16_big(bytes: &[u8]) -> Option<usize> {
    match bytes.len() {
        1 => {
            let a = bytes[0];
            if a <= 0xD7 || a >= 0xE0 {
                Some(1)
            } else if a >> 2 == 0x36 {
                Some(3)
            } else {
                None
            }
        }
        2 if bytes[0] >> 2 == 0x36 => Some(2),
        3 if bytes[0] >> 2 == 0x36 && bytes[2] >> 2 == 0x37 => Some(1),
        _ => None,
    }
}

/// `cbv({utf16,little}, Bin)` — the little-endian counterpart
/// (`unicode.erl:967-974`). A lone byte is always reported as needing one more
/// (the oracle's "or 3, we'll see").
fn cbv_utf16_little(bytes: &[u8]) -> Option<usize> {
    match bytes.len() {
        1 => Some(1),
        2 if bytes[1] >> 2 == 0x36 => Some(2),
        3 if bytes[1] >> 2 == 0x36 => Some(1),
        _ => None,
    }
}

/// `cbv({utf32,big}, Bin)` (`unicode.erl:977-985`): the leading zero bytes of a
/// big-endian code point, then the `U+D800..=U+DFFF` surrogate exclusion.
fn cbv_utf32_big(bytes: &[u8]) -> Option<usize> {
    match bytes.len() {
        1 if bytes[0] == 0 => Some(3),
        2 if bytes[0] == 0 && bytes[1] <= 16 => Some(2),
        3 if bytes[0] == 0
            && bytes[1] <= 16
            && (bytes[1] > 0 || bytes[2] <= 0xD7 || bytes[2] >= 0xE0) =>
        {
            Some(1)
        }
        _ => None,
    }
}

/// `cbv({utf32,little}, Bin)` (`unicode.erl:986-996`). The three-byte clause that
/// rejects a leading `16#D800..=16#DFFF` is tried *first*, matching the oracle's
/// clause order.
fn cbv_utf32_little(bytes: &[u8]) -> Option<usize> {
    match bytes.len() {
        1 => Some(3),
        2 => Some(2),
        3 => {
            let (lo, mid, hi) = (bytes[0], bytes[1], bytes[2]);
            if (lo == 254 || lo == 255) && mid == 255 && hi == 0 {
                return None;
            }
            if hi <= 16 && (hi > 0 || mid <= 0xD7 || mid >= 0xE0) {
                Some(1)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Decodes the code point at the head of `bytes` as UTF-16 (`do_i_utf16_big` /
/// `do_i_utf16_little`): a lone surrogate is not a code point and a high
/// surrogate only forms one with a following low surrogate.
pub(super) fn decode_utf16(bytes: &[u8], little: bool) -> Step {
    if bytes.len() >= 2 {
        let u = unit_at(bytes, little);
        if is_high(u) {
            if bytes.len() >= 4 {
                let lo = unit_at(&bytes[2..], little);
                if is_low(lo) {
                    let cp = 0x10000 + ((u - 0xD800) << 10) + (lo - 0xDC00);
                    return Step::Ok(cp, 4);
                }
            }
        } else if !is_low(u) {
            return Step::Ok(u, 2);
        }
    }
    match if little {
        cbv_utf16_little(bytes)
    } else {
        cbv_utf16_big(bytes)
    } {
        Some(needed) => Step::Incomplete(needed),
        None => Step::Invalid,
    }
}

/// Decodes the code point at the head of `bytes` as UTF-32 (`do_i_utf32_big` /
/// `do_i_utf32_little`). `<<U/utf32>>` rejects a surrogate and anything above
/// `U+10FFFF`.
pub(super) fn decode_utf32(bytes: &[u8], little: bool) -> Step {
    if bytes.len() >= 4 {
        let quad = [bytes[0], bytes[1], bytes[2], bytes[3]];
        let v = if little {
            u32::from_le_bytes(quad)
        } else {
            u32::from_be_bytes(quad)
        };
        if v <= 0x10FFFF && !(0xD800..=0xDFFF).contains(&v) {
            return Step::Ok(v, 4);
        }
        return Step::Invalid;
    }
    match if little {
        cbv_utf32_little(bytes)
    } else {
        cbv_utf32_big(bytes)
    } {
        Some(needed) => Step::Incomplete(needed),
        None => Step::Invalid,
    }
}

/// `true` iff `value` is a Unicode code point — the precondition every `<<One/…>>`
/// builder in `o_trans/1` enforces by raising `badarg` otherwise.
fn is_code_point(value: i64) -> Option<u32> {
    let v = u32::try_from(value).ok()?;
    if v > 0x10FFFF || (0xD800..=0xDFFF).contains(&v) {
        return None;
    }
    Some(v)
}

/// `<<One/utf16>>` / `<<One/utf16-little>>`; `None` where the Erlang
/// construction raises (which `do_o_binary2` turns into `{error, <<>>, [One|T]}`).
pub(super) fn encode_utf16(value: i64, little: bool) -> Option<Vec<u8>> {
    let v = is_code_point(value)?;
    let mut out = Vec::with_capacity(4);
    if v < 0x10000 {
        push_unit(&mut out, v as u16, little);
    } else {
        let x = v - 0x10000;
        push_unit(&mut out, 0xD800 + (x >> 10) as u16, little);
        push_unit(&mut out, 0xDC00 + (x & 0x3FF) as u16, little);
    }
    Some(out)
}

/// `<<One/utf32>>` / `<<One/utf32-little>>`; `None` where the Erlang construction
/// raises.
pub(super) fn encode_utf32(value: i64, little: bool) -> Option<Vec<u8>> {
    let v = is_code_point(value)?;
    Some(if little {
        v.to_le_bytes().to_vec()
    } else {
        v.to_be_bytes().to_vec()
    })
}

/// Appends the 16-bit `unit` in `little`- or big-endian order.
fn push_unit(out: &mut Vec<u8>, unit: u16, little: bool) {
    if little {
        out.extend_from_slice(&unit.to_le_bytes());
    } else {
        out.extend_from_slice(&unit.to_be_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `(bytes, little, expected)`.
    fn check_utf16(cases: &[(&[u8], bool, Step)]) {
        for (bytes, little, want) in cases {
            assert_eq!(
                decode_utf16(bytes, *little),
                *want,
                "decode_utf16({bytes:?}, little={little})"
            );
        }
    }

    /// `(bytes, little, expected)`.
    fn check_utf32(cases: &[(&[u8], bool, Step)]) {
        for (bytes, little, want) in cases {
            assert_eq!(
                decode_utf32(bytes, *little),
                *want,
                "decode_utf32({bytes:?}, little={little})"
            );
        }
    }

    #[test]
    fn utf16_big_endian_basic_code_points() {
        check_utf16(&[
            (&[0, 65], false, Step::Ok(0x41, 2)),
            (&[0, 65, 0, 66], false, Step::Ok(0x41, 2)),
            (&[0xFE, 0xFF], false, Step::Ok(0xFEFF, 2)),
            (&[0xFF, 0xFE], false, Step::Ok(0xFFFE, 2)),
            // A unit whose high byte is not a surrogate lead byte is plain.
            (&[0, 0xD8], false, Step::Ok(0x00D8, 2)),
        ]);
    }

    #[test]
    fn utf16_big_endian_surrogate_pairs() {
        // `<<0xD83C,0xDE00>>/utf16` => U+1F200 (127488), `<<0xD83D,0xDE00>>` => U+1F600 (128512).
        check_utf16(&[
            (&[0xD8, 0x3C, 0xDE, 0x00], false, Step::Ok(127488, 4)),
            (&[0xD8, 0x3D, 0xDE, 0x00], false, Step::Ok(128512, 4)),
            (&[0xD8, 0x3C, 0xDE, 0x00, 65], false, Step::Ok(127488, 4)),
        ]);
    }

    #[test]
    fn utf16_big_endian_lone_surrogates() {
        // A lone low surrogate is invalid outright.
        check_utf16(&[
            (&[0xDC, 0x00], false, Step::Invalid),
            (&[0xDC, 0x00, 0x00, 0x65], false, Step::Invalid),
            // A high surrogate followed by a non-low unit is invalid, not a prefix.
            (&[0xD8, 0x3C, 0x41, 0x42], false, Step::Invalid),
            // ... and a one-byte `0xDC` names no prefix at all (the cbv clause
            // only accepts `0xD8..=0xDB`).
            (&[0xDC], false, Step::Invalid),
        ]);
    }

    #[test]
    fn utf16_big_endian_prefixes() {
        check_utf16(&[
            (&[0], false, Step::Incomplete(1)),
            (&[0xFF], false, Step::Incomplete(1)),
            (&[0xD8], false, Step::Incomplete(3)),
            (&[0xD8, 0x3C], false, Step::Incomplete(2)),
            (&[0xD8, 0x3C, 0xDE], false, Step::Incomplete(1)),
            // A high surrogate whose second unit is not a low one has no prefix.
            (&[0xD8, 0x3C, 0x41], false, Step::Invalid),
        ]);
    }

    #[test]
    fn utf16_little_endian() {
        check_utf16(&[
            (&[65, 0], true, Step::Ok(0x41, 2)),
            (&[60, 0xD8, 0x00, 0xDE], true, Step::Ok(127488, 4)),
            (&[61, 0xD8, 0x00, 0xDE], true, Step::Ok(128512, 4)),
            (&[60, 0xD8, 0x00, 0xDE, 65], true, Step::Ok(127488, 4)),
            // A lone trailing byte always reports one (never three).
            (&[0], true, Step::Incomplete(1)),
            (&[0xFF], true, Step::Incomplete(1)),
            (&[0, 0xD8], true, Step::Incomplete(2)),
            (&[0, 0xD8, 0x00], true, Step::Incomplete(1)),
            (&[0, 0xDC], true, Step::Invalid),
        ]);
    }

    #[test]
    fn utf32_big_endian() {
        check_utf32(&[
            (&[0, 0, 0, 65], false, Step::Ok(0x41, 4)),
            (&[0, 0, 0, 65, 0, 0, 0, 66], false, Step::Ok(0x41, 4)),
            (&[0, 0, 0xFE, 0xFF], false, Step::Ok(0xFEFF, 4)),
            (&[0, 0, 0xFF, 0xFF], false, Step::Ok(0xFFFF, 4)),
            // A surrogate, above U+10FFFF, and a non-prefix.
            (&[0, 0, 0xD8, 0], false, Step::Invalid),
            (&[0, 17, 0, 0], false, Step::Invalid),
            (&[0, 17, 0, 0, 65], false, Step::Invalid),
            (&[0, 0, 0], false, Step::Incomplete(1)),
            (&[0, 17, 0], false, Step::Invalid),
        ]);
    }

    #[test]
    fn utf32_big_endian_prefixes() {
        check_utf32(&[
            (&[0], false, Step::Incomplete(3)),
            (&[0, 0], false, Step::Incomplete(2)),
            (&[0, 16], false, Step::Incomplete(2)),
            (&[0, 0, 0], false, Step::Incomplete(1)),
            // 0x00D800 would be a surrogate once complete.
            (&[0, 0xD8, 0], false, Step::Invalid),
        ]);
    }

    #[test]
    fn utf32_little_endian() {
        check_utf32(&[
            (&[65, 0, 0, 0], true, Step::Ok(0x41, 4)),
            (&[0, 0xF6, 1, 0], true, Step::Ok(0x1F600, 4)),
            (&[0, 0xD8, 0, 0], true, Step::Invalid),
            (&[0], true, Step::Incomplete(3)),
            (&[0, 0], true, Step::Incomplete(2)),
            (&[65, 0, 0], true, Step::Incomplete(1)),
            (&[65, 0xD8, 1], true, Step::Incomplete(1)),
            // The `254/255, 0` guard fires before the general clause.
            (&[254, 255, 0], true, Step::Invalid),
            (&[255, 255, 0], true, Step::Invalid),
            (&[1, 255, 0], true, Step::Incomplete(1)),
        ]);
    }

    #[test]
    fn encoders_round_trip_the_decoders() {
        for &v in &[0i64, 1, 0x7F, 0x80, 0x7FF, 0x800, 0xD7FF, 0xE000, 0xFFFF, 0x10000, 0x1F600, 0x10FFFF] {
            for &little in &[false, true] {
                let b16 = encode_utf16(v, little).expect("a code point");
                assert_eq!(decode_utf16(&b16, little), Step::Ok(v as u32, b16.len()));
                let b32 = encode_utf32(v, little).expect("a code point");
                assert_eq!(decode_utf32(&b32, little), Step::Ok(v as u32, 4));
            }
        }
        assert_eq!(encode_utf16(0x1F600, false).expect("a code point"), vec![0xD8, 0x3D, 0xDE, 0x00]);
        assert_eq!(encode_utf16(0x1F600, true).expect("a code point"), vec![0x3D, 0xD8, 0x00, 0xDE]);
        assert_eq!(encode_utf32(0x1F600, false).expect("a code point"), vec![0, 1, 0xF6, 0]);
        assert_eq!(encode_utf32(0x1F600, true).expect("a code point"), vec![0, 0xF6, 1, 0]);
    }

    #[test]
    fn encoders_reject_non_code_points() {
        for &v in &[-1i64, -2, 0xD800, 0xDFFF, 0x110000, i64::from(i32::MAX)] {
            assert_eq!(encode_utf16(v, false), None, "utf16 {v}");
            assert_eq!(encode_utf16(v, true), None, "utf16-little {v}");
            assert_eq!(encode_utf32(v, false), None, "utf32 {v}");
            assert_eq!(encode_utf32(v, true), None, "utf32-little {v}");
        }
    }
}
