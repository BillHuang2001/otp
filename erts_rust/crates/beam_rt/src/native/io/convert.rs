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

//! `unicode:characters_to_binary/3` for the `io` device paths.
//!
//! The io-device protocol carries character data in a **named encoding** and every device
//! transcodes it to its own: `file_io_server:put_chars/3` ends in
//! `unicode:characters_to_binary(Chars, InEncoding, OutEncoding)`
//! (`lib/kernel/src/file_io_server.erl:438`) and `standard_error:put_chars/2` in
//! `wrap_characters_to_binary/3` (`lib/kernel/src/standard_error.erl:260-279`). Reproducing
//! the oracle's *bytes* therefore means reproducing that call — `String::from_utf8_lossy`
//! is not it (it would replace every non-UTF-8 byte with `U+FFFD`).
//!
//! # The three answers
//!
//! `unicode:characters_to_binary/3` has exactly three observable outcomes, and all three are
//! load-bearing here (each was measured against OTP 29.1.2, see [`super`]):
//!
//! * the encoded bytes;
//! * `{error, Encoded, Rest}` / `{incomplete, Encoded, Rest}` — surfaced as
//!   [`Chars::NoTranslation`], which a device reports as `{error, {no_translation, In, Out}}`
//!   (the caller of `io:format/3` then raises `no_translation`, `io.erl:785`);
//! * a **`badarg` raise** for a term that is not chardata at all (an atom, a float, a tuple, a
//!   map, a non-byte-aligned bitstring) — `unicode:characters_to_binary(foo, unicode, latin1)`
//!   raises in `unicode.erl:1376`, which is *uncaught* inside `file_io_server:put_chars/3`, so
//!   the oracle's device process **dies** and its caller sees `{error, terminated}`
//!   ([`Chars::BadArg`]).
//!
//! # Consecutive-binary grouping
//!
//! `unicode.erl` decodes **runs** of adjacent binary/bitstring elements as one UTF-8 stream
//! (an integer element in between breaks the run), so `[<<195>>, <<169>>]` is the one code
//! point `U+00E9`, not an incomplete sequence. The walk below keeps that grouping.

use beam_engine::terms;
use beam_io::io_device::Encoding;
use beam_term::prelude::*;

/// The outcome of a `characters_to_binary(Chars, From, To)` conversion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Chars {
    /// The encoded bytes.
    Bytes(Vec<u8>),
    /// `{error, …}` / `{incomplete, …}`: an invalid or out-of-range code point, or a truncated
    /// UTF-8 sequence. The device answers `{error, {no_translation, From, To}}`.
    NoTranslation,
    /// The argument is not chardata: `characters_to_binary/3` **raises** `badarg`.
    BadArg,
}

/// `unicode:characters_to_binary(Chars, from, to)`.
pub(crate) fn characters_to_binary(chars: Eterm, from: Encoding, to: Encoding) -> Chars {
    let mut code_points: Vec<u32> = Vec::new();
    let mut pending: Vec<u8> = Vec::new();
    // An explicit work list, not recursion: this runs on the runtime's own stack, which (unlike
    // a BEAM process's) does not grow.
    let mut work: Vec<Eterm> = vec![chars];
    while let Some(term) = work.pop() {
        if term == NIL {
            continue;
        }
        if is_small(term) {
            if !flush(&mut pending, &mut code_points, from) {
                return Chars::NoTranslation;
            }
            let code = signed_value(term);
            let Ok(code) = u32::try_from(code) else {
                return Chars::NoTranslation;
            };
            if code > 0x10ffff {
                return Chars::NoTranslation;
            }
            code_points.push(code);
            continue;
        }
        if let Ok(bytes) = terms::binary_bytes(term) {
            match from {
                Encoding::Latin1 => {
                    if !flush(&mut pending, &mut code_points, from) {
                        return Chars::NoTranslation;
                    }
                    code_points.extend(bytes.iter().map(|byte| u32::from(*byte)));
                }
                // A run of binaries is decoded as one stream (see the module docs).
                Encoding::Unicode => pending.extend_from_slice(bytes),
            }
            continue;
        }
        if is_list(term) {
            if let (Ok(head), Ok(tail)) = (terms::cons_head(term), terms::cons_tail(term)) {
                work.push(tail);
                work.push(head);
                continue;
            }
        }
        // Not chardata at all — the `badarg` raise.
        return Chars::BadArg;
    }
    if !flush(&mut pending, &mut code_points, from) {
        return Chars::NoTranslation;
    }
    encode(&code_points, to)
}

/// Decodes the `pending` UTF-8 context into `code_points` (a no-op for a latin1 input, whose
/// bytes are already code points).
fn flush(pending: &mut Vec<u8>, code_points: &mut Vec<u32>, from: Encoding) -> bool {
    if from == Encoding::Latin1 {
        pending.clear();
        return true;
    }
    let mut rest = &pending[..];
    while !rest.is_empty() {
        let (code, width) = match decode_utf8(rest) {
            Some(pair) => pair,
            None => return false,
        };
        code_points.push(code);
        rest = &rest[width..];
    }
    pending.clear();
    true
}

/// One UTF-8 sequence: its code point and its width, or `None` for an invalid/incomplete one.
fn decode_utf8(bytes: &[u8]) -> Option<(u32, usize)> {
    let first = bytes[0];
    let (width, minimum, initial) = match first {
        0x00..=0x7f => return Some((u32::from(first), 1)),
        0xc2..=0xdf => (2, 0x80, u32::from(first) & 0x1f),
        0xe0..=0xef => (3, 0x800, u32::from(first) & 0x0f),
        0xf0..=0xf4 => (4, 0x10000, u32::from(first) & 0x07),
        _ => return None,
    };
    if bytes.len() < width {
        return None;
    }
    let mut code = initial;
    for byte in &bytes[1..width] {
        if byte & 0xc0 != 0x80 {
            return None;
        }
        code = (code << 6) | u32::from(byte & 0x3f);
    }
    if code < minimum || code > 0x10ffff || (0xd800..=0xdfff).contains(&code) {
        return None;
    }
    Some((code, width))
}

/// Encodes `code_points` in `to`.
fn encode(code_points: &[u32], to: Encoding) -> Chars {
    let mut out = Vec::with_capacity(code_points.len());
    for code in code_points {
        match to {
            Encoding::Latin1 => match u8::try_from(*code) {
                Ok(byte) => out.push(byte),
                Err(_) => return Chars::NoTranslation,
            },
            Encoding::Unicode => {
                if (0xd800..=0xdfff).contains(code) {
                    return Chars::NoTranslation;
                }
                let Some(character) = char::from_u32(*code) else {
                    return Chars::NoTranslation;
                };
                let mut buf = [0u8; 4];
                out.extend_from_slice(character.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    Chars::Bytes(out)
}

/// The [`Encoding`] an io request's encoding term names.
///
/// `file_io_server`/`standard_error` match `latin1` and `unicode`/`utf8`; any other atom
/// reaches `characters_to_binary/3` as an unknown encoding, which **raises** `badarg`
/// (`unicode.erl:366`), so an unknown name is [`Chars::BadArg`] at the call site.
pub(crate) fn encoding_of(name: Option<&str>) -> Option<Encoding> {
    match name {
        Some("latin1") => Some(Encoding::Latin1),
        Some("unicode") | Some("utf8") => Some(Encoding::Unicode),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test arena holding the terms under test.
    struct Arena {
        heap: beam_term::heap::Heap,
    }

    impl Arena {
        fn new() -> Arena {
            Arena {
                heap: beam_term::heap::Heap::new(1024),
            }
        }

        fn list(&mut self, elements: &[Eterm]) -> Eterm {
            terms::make_list(&mut self.heap, elements).expect("list")
        }

        fn string(&mut self, text: &str) -> Eterm {
            let codes: Vec<Eterm> = text.chars().map(|c| make_small(i64::from(c as u32))).collect();
            self.list(&codes)
        }
    }

    #[test]
    fn latin1_input_is_transcoded_to_utf8() {
        let mut arena = Arena::new();
        let chars = arena.list(&[make_small(233)]);
        assert_eq!(
            characters_to_binary(chars, Encoding::Latin1, Encoding::Unicode),
            Chars::Bytes(vec![195, 169])
        );
    }

    #[test]
    fn unicode_input_is_narrowed_to_one_latin1_byte() {
        let mut arena = Arena::new();
        let chars = arena.list(&[make_small(233)]);
        assert_eq!(
            characters_to_binary(chars, Encoding::Unicode, Encoding::Latin1),
            Chars::Bytes(vec![233])
        );
    }

    #[test]
    fn a_code_point_above_latin1_is_no_translation() {
        let mut arena = Arena::new();
        let chars = arena.list(&[make_small(1000)]);
        assert_eq!(
            characters_to_binary(chars, Encoding::Unicode, Encoding::Latin1),
            Chars::NoTranslation
        );
    }

    #[test]
    fn an_atom_payload_is_the_badarg_raise() {
        let mut arena = Arena::new();
        let atom = beam_term::AtomTable::new()
            .lookup("foo")
            .map(make_atom)
            .unwrap_or_else(|| make_atom(beam_term::AtomTable::new().intern("foo")));
        assert_eq!(
            characters_to_binary(atom, Encoding::Unicode, Encoding::Latin1),
            Chars::BadArg
        );
    }

    #[test]
    fn adjacent_binaries_are_one_utf8_stream() {
        let mut arena = Arena::new();
        let first = terms::make_heap_binary(&mut arena.heap, &[195]).expect("binary");
        let second = terms::make_heap_binary(&mut arena.heap, &[169]).expect("binary");
        let chars = arena.list(&[first, second]);
        assert_eq!(
            characters_to_binary(chars, Encoding::Unicode, Encoding::Latin1),
            Chars::Bytes(vec![233])
        );
    }

    #[test]
    fn an_integer_between_binaries_breaks_the_utf8_stream() {
        let mut arena = Arena::new();
        let first = terms::make_heap_binary(&mut arena.heap, &[195]).expect("binary");
        let second = terms::make_heap_binary(&mut arena.heap, &[169]).expect("binary");
        let chars = arena.list(&[first, make_small(65), second]);
        assert_eq!(
            characters_to_binary(chars, Encoding::Unicode, Encoding::Latin1),
            Chars::NoTranslation
        );
    }

    #[test]
    fn an_ascii_string_round_trips() {
        let mut arena = Arena::new();
        let chars = arena.string("abc");
        assert_eq!(
            characters_to_binary(chars, Encoding::Latin1, Encoding::Unicode),
            Chars::Bytes(b"abc".to_vec())
        );
    }

    #[test]
    fn the_encoding_names_are_the_three_io_uses() {
        assert_eq!(encoding_of(Some("latin1")), Some(Encoding::Latin1));
        assert_eq!(encoding_of(Some("unicode")), Some(Encoding::Unicode));
        assert_eq!(encoding_of(Some("utf8")), Some(Encoding::Unicode));
        assert_eq!(encoding_of(Some("utf16")), None);
    }
}
