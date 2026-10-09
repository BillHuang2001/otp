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

//! Shared support for the `beam_loader` integration tests.
//!
//! Currently one responsibility: **raw (non-normalizing) `LitT` payload
//! inspection**. [`beam_codec::decode_term`] normalizes a flat map's keys into
//! ERTS's `erts_cmp_flatmap_keys` order, so the key order the compiler wrote
//! (`+deterministic`) is only observable in the payload bytes themselves. The
//! helpers here walk a payload with [`beam_codec::Reader`] — the bounds-checked
//! big-endian cursor, and deliberately *not* the decoder — and never materialize a
//! term.
//!
//! This module is a `tests/` subdirectory module, so Cargo does not build it as a
//! test target of its own; it is compiled into whichever test file declares
//! `mod common;`.

use beam_codec::tag as etf;
use beam_codec::Reader;

/// The **raw** (payload) key-name order of the flat map at element 0 of the
/// `nested/0` literal fixture term.
///
/// Reads the payload's own bytes (never `decode_term`, which would normalize the key
/// order away): `[VERSION_MAGIC][SMALL_TUPLE_EXT, 3][MAP_EXT, N][(atom key, value)*]`.
pub fn raw_map_key_names(payload: &[u8]) -> Vec<String> {
    let what = "the `nested/0` literal payload";
    let mut reader = Reader::new(payload);
    assert_eq!(read_u8(&mut reader, what), etf::VERSION_MAGIC, "{what}: the leading version byte");
    assert_eq!(
        read_u8(&mut reader, what),
        etf::SMALL_TUPLE_EXT,
        "{what}: the outer term is a small tuple"
    );
    assert_eq!(read_u8(&mut reader, what), 3, "{what}: the outer tuple has three elements");
    assert_eq!(read_u8(&mut reader, what), etf::MAP_EXT, "{what}: element 0 is a flat map");
    let pairs = read_u32(&mut reader, what);
    assert_eq!(pairs, 3, "{what}: the inner map has three pairs");

    let mut names = Vec::with_capacity(pairs as usize);
    for _ in 0..pairs {
        names.push(read_raw_atom(&mut reader, what));
        skip_raw_value(&mut reader, what);
    }
    names
}

/// Reads one byte through `reader`, or fails naming `what`.
fn read_u8(reader: &mut Reader<'_>, what: &str) -> u8 {
    reader.u8().unwrap_or_else(|error| panic!("{what}: {error:?}"))
}

/// Reads a big-endian `u16` through `reader`, or fails naming `what`.
fn read_u16(reader: &mut Reader<'_>, what: &str) -> u16 {
    reader.u16be().unwrap_or_else(|error| panic!("{what}: {error:?}"))
}

/// Reads a big-endian `u32` through `reader`, or fails naming `what`.
fn read_u32(reader: &mut Reader<'_>, what: &str) -> u32 {
    reader.u32be().unwrap_or_else(|error| panic!("{what}: {error:?}"))
}

/// Borrows `n` bytes through `reader`, or fails naming `what`.
fn read_bytes(reader: &mut Reader<'_>, n: usize, what: &str) -> Vec<u8> {
    reader.bytes(n).unwrap_or_else(|error| panic!("{what}: {error:?}")).to_vec()
}

/// Skips `n` bytes through `reader`, or fails naming `what`.
fn skip_bytes(reader: &mut Reader<'_>, n: usize, what: &str) {
    reader.bytes(n).unwrap_or_else(|error| panic!("{what}: {error:?}"));
}

/// Reads the body of one raw ETF atom **whose tag byte has already been consumed**,
/// and returns its name.
fn read_atom_body(reader: &mut Reader<'_>, tag: u8, what: &str) -> String {
    let length = match tag {
        etf::SMALL_ATOM_UTF8_EXT | etf::SMALL_ATOM_EXT => usize::from(read_u8(reader, what)),
        etf::ATOM_UTF8_EXT | etf::ATOM_EXT => usize::from(read_u16(reader, what)),
        other => panic!("{what}: expected an atom tag, found {other}"),
    };
    let bytes = read_bytes(reader, length, what);
    String::from_utf8(bytes).unwrap_or_else(|error| panic!("{what}: {error}"))
}

/// Reads one raw ETF atom — any of the four wire forms — and returns its name.
fn read_raw_atom(reader: &mut Reader<'_>, what: &str) -> String {
    let tag = read_u8(reader, what);
    read_atom_body(reader, tag, what)
}

/// Advances `reader` past one raw ETF value **without decoding it**.
///
/// NARROW BY DESIGN: it understands the tags the committed fixtures' payloads can
/// carry and panics — naming the offending tag and offset — on any other, so a
/// fixture that grows a new tag fails loudly instead of silently mis-scanning the
/// map's key order.
fn skip_raw_value(reader: &mut Reader<'_>, what: &str) {
    let offset = reader.position();
    let tag = read_u8(reader, what);
    match tag {
        etf::SMALL_INTEGER_EXT => skip_bytes(reader, 1, what),
        etf::INTEGER_EXT => skip_bytes(reader, 4, what),
        etf::NEW_FLOAT_EXT => skip_bytes(reader, 8, what),
        etf::FLOAT_EXT => skip_bytes(reader, 31, what),
        etf::NIL_EXT => {}
        etf::SMALL_ATOM_UTF8_EXT
        | etf::SMALL_ATOM_EXT
        | etf::ATOM_UTF8_EXT
        | etf::ATOM_EXT => {
            // The tag is already consumed, so skip the atom's body directly.
            read_atom_body(reader, tag, what);
        }
        etf::STRING_EXT => {
            let n = usize::from(read_u16(reader, what));
            skip_bytes(reader, n, what);
        }
        etf::BINARY_EXT => {
            let n = read_u32(reader, what) as usize;
            skip_bytes(reader, n, what);
        }
        etf::BIT_BINARY_EXT => {
            let n = read_u32(reader, what) as usize;
            skip_bytes(reader, n + 1, what);
        }
        etf::SMALL_BIG_EXT => {
            let n = usize::from(read_u8(reader, what));
            skip_bytes(reader, n + 1, what);
        }
        etf::LARGE_BIG_EXT => {
            let n = read_u32(reader, what) as usize;
            skip_bytes(reader, n + 1, what);
        }
        etf::EXPORT_EXT => {
            read_raw_atom(reader, what);
            read_raw_atom(reader, what);
        }
        etf::LIST_EXT => {
            let n = read_u32(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
            }
            // ... plus the tail, which `NIL_EXT` terminates for a proper list.
            skip_raw_value(reader, what);
        }
        etf::SMALL_TUPLE_EXT => {
            let n = read_u8(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
            }
        }
        etf::LARGE_TUPLE_EXT => {
            let n = read_u32(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
            }
        }
        etf::MAP_EXT => {
            let n = read_u32(reader, what);
            for _ in 0..n {
                skip_raw_value(reader, what);
                skip_raw_value(reader, what);
            }
        }
        other => panic!(
            "{what}: unmodelled ETF tag {other} at offset {offset} — extend `skip_raw_value`"
        ),
    }
}
