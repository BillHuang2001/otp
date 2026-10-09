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

//! The two encodings of the io-device protocol and the code-point conversions the
//! `{get_until,…}` loop performs.
//!
//! The protocol has **two** encoding tags with the same two values:
//!
//! * the request's **output** encoding (`{get_until, Enc, …}`) — [`Encoding`] as handed to
//!   [`crate::io_device::IoDevice::get_until`]; and
//! * the device's own **stream** encoding (`file_io_server`'s `State#state.unic`, the
//!   `encoding` option of `file:open/2`) — [`IoDevice::encoding`](super::IoDevice).
//!
//! The device loop reproduces the four `unicode:characters_to_binary/3` /
//! `unicode:characters_to_list/2` conversions it depends on
//! (`lib/kernel/src/file_io_server.erl:642-691`), including the distinction between a
//! **complete** decoding, a decoding with an **incomplete trailing sequence** (the bytes
//! that must be re-fed once more input has been read) and an **invalid** sequence (a
//! faithful `invalid_unicode` failure).
//!
//! `unsafe` and allocation-free: this module is pure byte/code-point arithmetic.

use beam_common::{BeamError, ExitReason, Raise};
/// The encoding tag of an io-device request or device (`latin1` | `unicode`).
///
/// Only the two byte-oriented encodings the `{get_until,…}` request's `Enc` field may
/// carry are modelled. The tuple encodings (`{utf16,binary}`, `{utf32,little}`, …) that a
/// device's `encoding` option also accepts are **not** modelled; a caller must reject
/// them before building a device (they are a documented capability gap elsewhere).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Encoding {
    /// `latin1`: every byte is a code point `0..=255`.
    #[default]
    Latin1,
    /// `unicode`: the bytes are UTF-8.
    Unicode,
}

impl Encoding {
    /// The BEAM atom name of this encoding.
    pub const fn name(self) -> &'static str {
        match self {
            Encoding::Latin1 => "latin1",
            Encoding::Unicode => "unicode",
        }
    }

    /// Parses a BEAM encoding atom.
    ///
    /// Returns `None` for any other atom (including the tuple encodings), so an unknown
    /// tag is refused by the caller rather than silently defaulted.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "latin1" => Some(Encoding::Latin1),
            "unicode" => Some(Encoding::Unicode),
            _ => None,
        }
    }
}

impl std::fmt::Display for Encoding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// The result of decoding bytes to code points — `unicode:characters_to_list/2` with its
/// three distinguishable outcomes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Decoded {
    /// Every byte decoded.
    Complete(Vec<u32>),
    /// A decoded prefix plus the **incomplete** trailing byte sequence to re-feed.
    Incomplete {
        /// The code points decoded before the incomplete sequence.
        prefix: Vec<u32>,
        /// The trailing bytes that form (or begin) an incomplete sequence.
        tail: Vec<u8>,
    },
    /// A decoded prefix plus the input from the first **invalid** sequence onward.
    Invalid {
        /// The code points decoded before the invalid sequence.
        prefix: Vec<u32>,
        /// The input from the start of the invalid sequence.
        rest: Vec<u8>,
    },
}

/// The result of re-encoding bytes from one encoding to another —
/// `unicode:characters_to_binary/3` for a **binary** input, with its three outcomes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) enum Transcoded {
    /// The whole input was transcoded.
    Complete(Vec<u8>),
    /// A transcoded prefix plus the **incomplete** trailing input to re-feed.
    Incomplete {
        /// The output produced before the incomplete sequence.
        prefix: Vec<u8>,
        /// The trailing input bytes that form (or begin) an incomplete sequence.
        tail: Vec<u8>,
    },
    /// A transcoded prefix plus the input from the first **invalid** sequence onward.
    Invalid {
        /// The output produced before the invalid sequence.
        prefix: Vec<u8>,
        /// The input from the start of the invalid sequence.
        rest: Vec<u8>,
    },
}

/// `binary_to_list/1`: every byte as a code point.
pub(super) fn bytes_to_code_points(bytes: &[u8]) -> Vec<u32> {
    bytes.iter().map(|&byte| u32::from(byte)).collect()
}

/// `unicode:characters_to_list(Bytes, Enc)`.
pub(super) fn decode(bytes: &[u8], enc: Encoding) -> Decoded {
    match enc {
        Encoding::Latin1 => Decoded::Complete(bytes_to_code_points(bytes)),
        Encoding::Unicode => match scan_utf8(bytes) {
            Utf8::Ok => {
                let mut cps = Vec::with_capacity(bytes.len());
                decode_utf8_into(&mut cps, bytes);
                Decoded::Complete(cps)
            }
            Utf8::Incomplete(cut) => {
                let mut cps = Vec::new();
                decode_utf8_into(&mut cps, &bytes[..cut]);
                Decoded::Incomplete {
                    prefix: cps,
                    tail: bytes[cut..].to_vec(),
                }
            }
            Utf8::Invalid(cut) => {
                let mut cps = Vec::new();
                decode_utf8_into(&mut cps, &bytes[..cut]);
                Decoded::Invalid {
                    prefix: cps,
                    rest: bytes[cut..].to_vec(),
                }
            }
        },
    }
}

/// `unicode:characters_to_binary(Bytes, From, To)` for a **binary** input.
pub(super) fn transcode(bytes: &[u8], from: Encoding, to: Encoding) -> Transcoded {
    match (from, to) {
        (Encoding::Latin1, Encoding::Latin1) => Transcoded::Complete(bytes.to_vec()),
        (Encoding::Latin1, Encoding::Unicode) => {
            let mut out = Vec::with_capacity(bytes.len() * 2);
            for &byte in bytes {
                push_utf8(&mut out, u32::from(byte));
            }
            Transcoded::Complete(out)
        }
        (Encoding::Unicode, Encoding::Unicode) => match scan_utf8(bytes) {
            Utf8::Ok => Transcoded::Complete(bytes.to_vec()),
            Utf8::Incomplete(cut) => Transcoded::Incomplete {
                prefix: bytes[..cut].to_vec(),
                tail: bytes[cut..].to_vec(),
            },
            Utf8::Invalid(cut) => Transcoded::Invalid {
                prefix: bytes[..cut].to_vec(),
                rest: bytes[cut..].to_vec(),
            },
        },
        // A code point that does not fit the target encoding cuts the output at that
        // point (the oracle's `{error, Prefix, Rest}`).
        (Encoding::Unicode, Encoding::Latin1) => {
            let mut out = Vec::with_capacity(bytes.len());
            let mut i = 0;
            while i < bytes.len() {
                match next_utf8(bytes, i) {
                    Step::Ok(cp, len) => match u8::try_from(cp) {
                        Ok(byte) => {
                            out.push(byte);
                            i += len;
                        }
                        Err(_) => {
                            return Transcoded::Invalid {
                                prefix: out,
                                rest: bytes[i..].to_vec(),
                            }
                        }
                    },
                    Step::Incomplete => {
                        return Transcoded::Incomplete {
                            prefix: out,
                            tail: bytes[i..].to_vec(),
                        }
                    }
                    Step::Invalid => {
                        return Transcoded::Invalid {
                            prefix: out,
                            rest: bytes[i..].to_vec(),
                        }
                    }
                }
            }
            Transcoded::Complete(out)
        }
    }
}

/// `unicode:characters_to_binary(CodePoints, unicode, Enc)` for a **code-point list**.
///
/// `Err(i)` reports the index of the first code point that cannot be represented in
/// `enc` (the caller renders the oracle's `badarg`-class failure it produces downstream).
pub(super) fn encode_code_points(cps: &[u32], enc: Encoding) -> std::result::Result<Vec<u8>, usize> {
    let mut out = Vec::with_capacity(cps.len());
    match enc {
        Encoding::Latin1 => {
            for (index, &cp) in cps.iter().enumerate() {
                match u8::try_from(cp) {
                    Ok(byte) => out.push(byte),
                    Err(_) => return Err(index),
                }
            }
        }
        Encoding::Unicode => {
            for (index, &cp) in cps.iter().enumerate() {
                if cp > 0x0010_FFFF || (0xD800..=0xDFFF).contains(&cp) {
                    return Err(index);
                }
                push_utf8(&mut out, cp);
            }
        }
    }
    Ok(out)
}

/// The error of an unrepresentable code point in a *leftover* conversion: the oracle's
/// `badarg`-class failure raised by `list_to_binary/1` when the conversion returns a tuple.
pub(super) fn invalid_unicode() -> BeamError {
    BeamError::Raise(Raise::exit(ExitReason::atom("invalid_unicode")))
}

/// The `badarg` a failed round-trip of a continuation's leftover produces.
pub(super) fn bad_leftover() -> BeamError {
    crate::error::badarg()
}

/// The outcome of validating a whole UTF-8 byte string.
enum Utf8 {
    /// The whole string is valid UTF-8.
    Ok,
    /// The string ends with an incomplete sequence starting at this byte index.
    Incomplete(usize),
    /// The string contains an invalid sequence starting at this byte index.
    Invalid(usize),
}

/// One UTF-8 sequence's decode attempt.
enum Step {
    /// A complete sequence: the code point and its byte length.
    Ok(u32, usize),
    /// The sequence runs past the end of the input.
    Incomplete,
    /// The sequence is not valid UTF-8.
    Invalid,
}

/// Validates a UTF-8 byte string, reporting the first cut.
fn scan_utf8(bytes: &[u8]) -> Utf8 {
    let mut i = 0;
    while i < bytes.len() {
        match next_utf8(bytes, i) {
            Step::Ok(_, len) => i += len,
            Step::Incomplete => return Utf8::Incomplete(i),
            Step::Invalid => return Utf8::Invalid(i),
        }
    }
    Utf8::Ok
}

/// Decodes one UTF-8 sequence at `i` of `bytes` (which must hold a complete sequence).
fn next_utf8(bytes: &[u8], i: usize) -> Step {
    let first = bytes[i];
    let len = if first < 0x80 {
        1
    } else if (0xC2..=0xDF).contains(&first) {
        2
    } else if (0xE0..=0xEF).contains(&first) {
        3
    } else if (0xF0..=0xF4).contains(&first) {
        4
    } else {
        // A continuation byte, or the always-overlong `0xC0`/`0xC1` starters.
        return Step::Invalid;
    };
    if i + len > bytes.len() {
        return Step::Incomplete;
    }
    let continuation = |offset: usize| bytes[i + offset] & 0xC0 == 0x80;
    let valid = match len {
        1 => true,
        2 => continuation(1),
        3 => {
            continuation(1)
                && continuation(2)
                && !(first == 0xE0 && bytes[i + 1] < 0xA0)
                && !(first == 0xED && bytes[i + 1] > 0x9F)
        }
        _ => {
            continuation(1)
                && continuation(2)
                && continuation(3)
                && !(first == 0xF0 && bytes[i + 1] < 0x90)
                && !(first == 0xF4 && bytes[i + 1] > 0x8F)
        }
    };
    if !valid {
        return Step::Invalid;
    }
    let cp = match len {
        1 => u32::from(first),
        2 => (u32::from(first & 0x1F) << 6) | u32::from(bytes[i + 1] & 0x3F),
        3 => {
            (u32::from(first & 0x0F) << 12)
                | (u32::from(bytes[i + 1] & 0x3F) << 6)
                | u32::from(bytes[i + 2] & 0x3F)
        }
        _ => {
            (u32::from(first & 0x07) << 18)
                | (u32::from(bytes[i + 1] & 0x3F) << 12)
                | (u32::from(bytes[i + 2] & 0x3F) << 6)
                | u32::from(bytes[i + 3] & 0x3F)
        }
    };
    Step::Ok(cp, len)
}

/// Decodes `bytes`, which must be valid UTF-8, appending its code points to `out`.
fn decode_utf8_into(out: &mut Vec<u32>, bytes: &[u8]) {
    let mut i = 0;
    while i < bytes.len() {
        match next_utf8(bytes, i) {
            Step::Ok(cp, len) => {
                out.push(cp);
                i += len;
            }
            // `decode_utf8_into` is only called with a validated prefix.
            Step::Incomplete | Step::Invalid => return,
        }
    }
}

/// Appends `cp` to `out` as UTF-8.
fn push_utf8(out: &mut Vec<u8>, cp: u32) {
    if cp < 0x80 {
        out.push(cp as u8);
    } else if cp < 0x0800 {
        out.push(0xC0 | (cp >> 6) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else if cp < 0x1_0000 {
        out.push(0xE0 | (cp >> 12) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    } else {
        out.push(0xF0 | (cp >> 18) as u8);
        out.push(0x80 | ((cp >> 12) & 0x3F) as u8);
        out.push(0x80 | ((cp >> 6) & 0x3F) as u8);
        out.push(0x80 | (cp & 0x3F) as u8);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_encoding_names_itself_and_round_trips() {
        assert_eq!(Encoding::Latin1.name(), "latin1");
        assert_eq!(Encoding::Unicode.name(), "unicode");
        assert_eq!(Encoding::from_name("latin1"), Some(Encoding::Latin1));
        assert_eq!(Encoding::from_name("unicode"), Some(Encoding::Unicode));
        assert_eq!(Encoding::from_name("utf8"), None);
        assert_eq!(Encoding::from_name(""), None);
        assert_eq!(Encoding::Latin1.to_string(), "latin1");
        // The pre-R13 `{get_until,_Prompt,…}` tag defaults to `latin1`.
        assert_eq!(Encoding::default(), Encoding::Latin1);
    }

    #[test]
    fn latin1_decoding_is_byte_wise_and_never_incomplete() {
        assert_eq!(
            decode(&[0x00, 0x41, 0xFF], Encoding::Latin1),
            Decoded::Complete(vec![0x00, 0x41, 0xFF])
        );
        assert_eq!(decode(&[], Encoding::Latin1), Decoded::Complete(vec![]));
        assert_eq!(bytes_to_code_points(b"hi"), vec![0x68, 0x69]);
    }

    #[test]
    fn utf8_decoding_reports_the_incomplete_tail() {
        // "a" then the first byte of "é" (U+00E9 = C3 A9).
        assert_eq!(
            decode(&[0x61, 0xC3], Encoding::Unicode),
            Decoded::Incomplete {
                prefix: vec![0x61],
                tail: vec![0xC3]
            }
        );
        // A lone `eof`-adjacent starter with nothing decoded before it.
        assert_eq!(
            decode(&[0xC3], Encoding::Unicode),
            Decoded::Incomplete {
                prefix: vec![],
                tail: vec![0xC3]
            }
        );
        // A four-byte starter cut short.
        assert_eq!(
            decode(&[0xF0, 0x9F], Encoding::Unicode),
            Decoded::Incomplete {
                prefix: vec![],
                tail: vec![0xF0, 0x9F]
            }
        );
        assert_eq!(
            decode(&[0x61, 0xC3, 0xA9, 0x62], Encoding::Unicode),
            Decoded::Complete(vec![0x61, 0xE9, 0x62])
        );
    }

    #[test]
    fn utf8_decoding_reports_an_invalid_sequence() {
        // A stray continuation byte.
        assert_eq!(
            decode(&[0x61, 0x80], Encoding::Unicode),
            Decoded::Invalid {
                prefix: vec![0x61],
                rest: vec![0x80]
            }
        );
        // `0xC0` is an always-overlong starter.
        assert_eq!(
            decode(&[0xC0, 0x80], Encoding::Unicode),
            Decoded::Invalid {
                prefix: vec![],
                rest: vec![0xC0, 0x80]
            }
        );
        // A surrogate half (`ED A0 80` = U+D800) is invalid.
        assert_eq!(
            decode(&[0xED, 0xA0, 0x80], Encoding::Unicode),
            Decoded::Invalid {
                prefix: vec![],
                rest: vec![0xED, 0xA0, 0x80]
            }
        );
        // Above U+10FFFF (`F4 90 80 80`).
        assert_eq!(
            decode(&[0xF4, 0x90, 0x80, 0x80], Encoding::Unicode),
            Decoded::Invalid {
                prefix: vec![],
                rest: vec![0xF4, 0x90, 0x80, 0x80]
            }
        );
        // A three-byte starter whose second byte is out of range for `E0`.
        assert_eq!(
            decode(&[0xE0, 0x80, 0x80], Encoding::Unicode),
            Decoded::Invalid {
                prefix: vec![],
                rest: vec![0xE0, 0x80, 0x80]
            }
        );
    }

    #[test]
    fn transcoding_between_the_two_byte_orientations() {
        // latin1 -> utf8: U+00E9 has no single-byte representation in UTF-8.
        assert_eq!(
            transcode(&[0xE9], Encoding::Latin1, Encoding::Unicode),
            Transcoded::Complete(vec![0xC3, 0xA9])
        );
        // utf8 -> latin1 round-trips for a code point in range.
        assert_eq!(
            transcode(&[0xC3, 0xA9], Encoding::Unicode, Encoding::Latin1),
            Transcoded::Complete(vec![0xE9])
        );
        // ... and cuts at the first code point that does not fit (the euro sign, U+20AC).
        assert_eq!(
            transcode(&[0x61, 0xE2, 0x82, 0xAC], Encoding::Unicode, Encoding::Latin1),
            Transcoded::Invalid {
                prefix: vec![0x61],
                rest: vec![0xE2, 0x82, 0xAC]
            }
        );
        // Identity is a validation only.
        assert_eq!(
            transcode(&[0xC3, 0xA9], Encoding::Unicode, Encoding::Unicode),
            Transcoded::Complete(vec![0xC3, 0xA9])
        );
        assert_eq!(
            transcode(&[0x61, 0xC3], Encoding::Unicode, Encoding::Unicode),
            Transcoded::Incomplete {
                prefix: vec![0x61],
                tail: vec![0xC3]
            }
        );
        // An invalid utf8 byte survives as an `Invalid` with an empty prefix.
        assert_eq!(
            transcode(&[0xFF], Encoding::Unicode, Encoding::Unicode),
            Transcoded::Invalid {
                prefix: vec![],
                rest: vec![0xFF]
            }
        );
    }

    #[test]
    fn encoding_code_points_reports_the_offending_index() {
        assert_eq!(encode_code_points(&[0x68, 0x69], Encoding::Latin1), Ok(b"hi".to_vec()));
        assert_eq!(encode_code_points(&[0x68, 0x100], Encoding::Latin1), Err(1));
        assert_eq!(
            encode_code_points(&[0xE9], Encoding::Unicode),
            Ok(vec![0xC3, 0xA9])
        );
        // A lone surrogate is not a code point.
        assert_eq!(encode_code_points(&[0xD800], Encoding::Unicode), Err(0));
        assert_eq!(encode_code_points(&[0x11_0000], Encoding::Unicode), Err(0));
    }

    #[test]
    fn the_device_error_sentinels_are_typed() {
        let error = invalid_unicode();
        assert!(error.is_raise());
        let raise = error.into_raise().expect("a raise");
        assert_eq!(raise.reason.as_atom(), Some("invalid_unicode"));
        assert!(matches!(bad_leftover(), BeamError::BadArg));
    }
}
